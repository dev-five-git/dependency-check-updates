//! crates.io registry client for looking up Rust crate versions.

use std::sync::Arc;

use reqwest::Client;
use serde::Deserialize;
use tokio::sync::Semaphore;
use tracing::{debug, trace};

use dependency_check_updates_core::{
    DEFAULT_MAX_CONCURRENT_REQUESTS, DcuError, DependencySpec, ResolvedVersion, TargetLevel,
    build_client, collect_task_results, strip_range_prefix,
};

/// crates.io registry client.
#[derive(Clone)]
pub struct CratesIoRegistry {
    client: Client,
    semaphore: Arc<Semaphore>,
    base_url: Arc<str>,
}

#[derive(Debug, Deserialize)]
struct CratesIoResponse {
    versions: Vec<CrateVersion>,
}

#[derive(Debug, Deserialize)]
struct CrateVersion {
    num: String,
    yanked: bool,
    /// ISO-8601 publish timestamp. Used for `--target newest`. Defaulted so
    /// older fixtures / partial responses without the field still deserialize.
    #[serde(default)]
    created_at: String,
}

impl CratesIoRegistry {
    /// Create a new crates.io registry client.
    #[must_use]
    pub fn new() -> Self {
        Self::with_base_url("https://crates.io/api/v1")
    }

    /// Create a client with a custom base URL.
    ///
    /// # Panics
    ///
    /// Panics if the HTTP client cannot be built.
    #[must_use]
    pub fn with_base_url(base_url: &str) -> Self {
        Self {
            client: build_client(),
            semaphore: Arc::new(Semaphore::new(DEFAULT_MAX_CONCURRENT_REQUESTS)),
            base_url: Arc::from(base_url.trim_end_matches('/')),
        }
    }

    /// Fetch all versions of a crate from crates.io.
    async fn fetch_versions(&self, name: &str) -> Result<Vec<CrateVersion>, DcuError> {
        // `acquire` only errors once the semaphore is closed; this registry
        // never closes its semaphore, so success is the sole reachable path
        // (mirrors `build_client`'s infallible-by-construction `expect`).
        let _permit = self
            .semaphore
            .acquire()
            .await
            .expect("semaphore is never closed");

        let url = format!("{}/crates/{name}/versions", self.base_url);
        debug!(crate_name = name, %url, "fetching crate versions");

        let response =
            self.client
                .get(&url)
                .send()
                .await
                .map_err(|e| DcuError::RegistryLookup {
                    package: name.to_owned(),
                    detail: e.to_string(),
                })?;

        if !response.status().is_success() {
            let status = response.status();
            return Err(DcuError::RegistryLookup {
                package: name.to_owned(),
                detail: format!("HTTP {status}"),
            });
        }

        let resp: CratesIoResponse =
            response
                .json()
                .await
                .map_err(|e| DcuError::RegistryLookup {
                    package: name.to_owned(),
                    detail: format!("failed to parse response: {e}"),
                })?;

        Ok(resp.versions)
    }

    /// Resolve the target version for a dependency.
    ///
    /// # Errors
    ///
    /// Returns an error if the registry lookup fails.
    pub async fn resolve_version(
        &self,
        dep: &DependencySpec,
        target: TargetLevel,
    ) -> Result<ResolvedVersion, DcuError> {
        let crate_versions = self.fetch_versions(&dep.name).await?;

        let yanked_count = crate_versions.iter().filter(|v| v.yanked).count();
        let mut versions: Vec<semver::Version> = crate_versions
            .iter()
            .filter(|v| !v.yanked)
            .filter_map(|v| semver::Version::parse(&v.num).ok())
            .collect();
        versions.sort_unstable();

        trace!(
            crate_name = %dep.name,
            total = crate_versions.len(),
            yanked = yanked_count,
            available = versions.len(),
            "fetched version list"
        );

        let latest = versions
            .iter()
            .rev()
            .find(|v| v.pre.is_empty())
            .map(std::string::ToString::to_string);

        let selected = if target == TargetLevel::Newest {
            // "Newest" = most recently published by date, which can differ from
            // the highest version (e.g. a patch backported to an old major
            // after a new major shipped). crates.io exposes `created_at` per
            // version, so resolve it from dates rather than semver ordering.
            newest_by_date(&crate_versions).or_else(|| versions.last().map(ToString::to_string))
        } else {
            select_version(&dep.current_req, latest.as_ref(), &versions, target)
        };

        // NOTE: we do NOT filter out versions that satisfy the current requirement.
        // ncu-style behavior updates the manifest spec itself; `compute_updates` in
        // the CLI uses bare-version comparison as the single source of truth.

        debug!(
            crate_name = %dep.name,
            current = %dep.current_req,
            selected = ?selected,
            target = %target,
            "resolved version"
        );

        Ok(ResolvedVersion { latest, selected })
    }

    /// Resolve versions for a batch of dependencies concurrently.
    pub async fn resolve_batch(
        &self,
        deps: &[DependencySpec],
        target: TargetLevel,
    ) -> Vec<(usize, Result<ResolvedVersion, DcuError>)> {
        let mut handles = Vec::with_capacity(deps.len());

        for (idx, dep) in deps.iter().enumerate() {
            let dep = dep.clone();
            let registry = self.clone();

            let handle = tokio::spawn(async move {
                let result = registry.resolve_version(&dep, target).await;
                (idx, result)
            });

            handles.push(handle);
        }

        let mut results = collect_task_results(handles).await;
        results.sort_unstable_by_key(|(idx, _)| *idx);
        results
    }
}

impl Default for CratesIoRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Pick the most-recently-published (by `created_at`) non-yanked version.
///
/// ISO-8601 timestamps sort lexicographically in chronological order, so a
/// plain string max suffices. Versions whose `num` does not parse as semver
/// are skipped. Returns `None` when no dated, parseable version exists (the
/// caller then falls back to the highest version).
fn newest_by_date(crate_versions: &[CrateVersion]) -> Option<String> {
    crate_versions
        .iter()
        .filter(|v| !v.yanked && !v.created_at.is_empty())
        .filter_map(|v| {
            semver::Version::parse(&v.num)
                .ok()
                .map(|parsed| (&v.created_at, parsed))
        })
        .max_by(|a, b| a.0.cmp(b.0))
        .map(|(_, parsed)| parsed.to_string())
}

/// Select the appropriate version based on target level.
///
/// Thin wrapper over [`dependency_check_updates_core::select_version`]. The
/// crates.io `latest` is already the highest stable version, which doubles as
/// the fallback for both the stable-`Latest` and unparseable-`Minor`/`Patch`
/// cases.
fn select_version(
    current_req_str: &str,
    latest: Option<&String>,
    all_versions: &[semver::Version],
    target: TargetLevel,
) -> Option<String> {
    let current = parse_base_version(current_req_str);
    dependency_check_updates_core::select_version(
        current.as_ref(),
        all_versions,
        target,
        latest.cloned(),
        latest.cloned(),
    )
}

fn parse_base_version(req_str: &str) -> Option<semver::Version> {
    semver::Version::parse(strip_range_prefix(req_str)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dependency_check_updates_core::DependencySection;
    use rstest::rstest;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Shared TLS setup for every async wiremock test. Invoked at the top of
    /// each `#[rstest] #[tokio::test]` body — wrapping it in a typed
    /// `#[fixture]` adds a unit-typed binding that the project's
    /// pedantic-clippy gate (`-D warnings`) rejects as either an underscore
    /// or unused variable. A plain shared helper preserves the
    /// "repeated setup" intent without that friction.
    fn install_tls_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    fn make_versions(vers: &[&str]) -> Vec<semver::Version> {
        let mut v: Vec<_> = vers
            .iter()
            .filter_map(|s| semver::Version::parse(s).ok())
            .collect();
        v.sort();
        v
    }

    fn serde_dep(current_req: &str) -> DependencySpec {
        DependencySpec {
            name: "serde".to_owned(),
            current_req: current_req.to_owned(),
            section: DependencySection::Dependencies,
        }
    }

    async fn mock_versions_endpoint(
        server: &MockServer,
        crate_name: &str,
        body: serde_json::Value,
    ) {
        Mock::given(method("GET"))
            .and(path(format!("/crates/{crate_name}/versions")))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    #[rstest]
    #[case::caret("^1.2.3", (1, 2, 3))]
    #[case::tilde("~1.2.3", (1, 2, 3))]
    fn parse_base_version_cases(#[case] req: &str, #[case] expected: (u64, u64, u64)) {
        let v = parse_base_version(req).unwrap();
        assert_eq!((v.major, v.minor, v.patch), expected);
    }

    /// Pure-function `select_version` cases. `expected_eq` and `expected_ne`
    /// are independent: when `Some`, the assertion runs; when `None`, it is
    /// skipped. This faithfully preserves the original mix of `assert_eq!` /
    /// `assert_ne!` / both per test, with no added or dropped assertions.
    #[rstest]
    #[case::select_latest("^1.0", "2.0.0", &["1.0.0", "1.5.0", "2.0.0"], TargetLevel::Latest, Some("2.0.0"), None)]
    #[case::select_minor("^1.0.0", "2.0.0", &["1.0.0", "1.5.0", "2.0.0"], TargetLevel::Minor, Some("1.5.0"), None)]
    #[case::select_patch("^1.0.0", "2.0.0", &["1.0.0", "1.0.5", "1.1.0", "2.0.0"], TargetLevel::Patch, Some("1.0.5"), None)]
    #[case::skip_prerelease("^1.0.0", "1.0.0", &["1.0.0", "2.0.0-rc.1"], TargetLevel::Latest, Some("1.0.0"), None)]
    #[case::greatest_includes_prerelease("^1.0.0", "1.0.0", &["1.0.0", "2.0.0-rc.1"], TargetLevel::Greatest, Some("2.0.0-rc.1"), None)]
    #[case::newest_includes_prerelease("^1.0.0", "1.0.0", &["1.0.0", "2.0.0-rc.1"], TargetLevel::Newest, Some("2.0.0-rc.1"), None)]
    // Current 2.0.0-rc.37; 3.0.0-alpha.1 (different train) must NOT be picked.
    #[case::tail_ignores_unrelated_prereleases("2.0.0-rc.37", "1.1.20", &["1.1.20", "2.0.0-rc.37", "3.0.0-alpha.1"], TargetLevel::Latest, None, Some("3.0.0-alpha.1"))]
    // Stable from any train is preferred over staying on a prerelease.
    #[case::picks_higher_stable_major("2.0.0-rc.37", "3.0.0", &["1.1.20", "2.0.0-rc.37", "3.0.0"], TargetLevel::Latest, Some("3.0.0"), None)]
    // Unrelated prerelease skipped, but cross-train stable picked.
    #[case::stable_wins_over_unrelated_prerelease("2.0.0-rc.37", "3.0.0", &["1.1.20", "2.0.0-rc.37", "3.0.0-alpha.1", "3.0.0"], TargetLevel::Latest, Some("3.0.0"), None)]
    // When same-train stable available, pick it (stable > prerelease).
    #[case::tail_jumps_to_stable("2.0.0-rc.37", "2.0.0", &["1.1.20", "2.0.0-rc.37", "2.0.0-rc.40", "2.0.0"], TargetLevel::Latest, Some("2.0.0"), None)]
    // sea-orm regression: NOT 1.1.20 (downgrade); self (2.0.0-rc.37) is fine.
    #[case::sea_orm_regression("2.0.0-rc.37", "1.1.20", &["1.1.20", "2.0.0-rc.37"], TargetLevel::Latest, Some("2.0.0-rc.37"), Some("1.1.20"))]
    // Stable current never picks prereleases (preserves stable-user behavior).
    #[case::stable_current_excludes_prerelease("1.0.0", "1.0.0", &["1.0.0", "2.0.0-rc.1"], TargetLevel::Latest, Some("1.0.0"), None)]
    #[case::empty_versions("^1.0.0", "1.0.0", &[], TargetLevel::Latest, Some("1.0.0"), None)]
    #[case::minor_unparseable_falls_back("*", "2.0.0", &["1.0.0", "2.0.0"], TargetLevel::Minor, Some("2.0.0"), None)]
    #[case::patch_unparseable_falls_back("*", "2.0.0", &["1.0.0", "2.0.0"], TargetLevel::Patch, Some("2.0.0"), None)]
    fn select_version_cases(
        #[case] req: &str,
        #[case] latest: &str,
        #[case] versions: &[&str],
        #[case] target: TargetLevel,
        #[case] expected_eq: Option<&str>,
        #[case] expected_ne: Option<&str>,
    ) {
        let latest_owned = latest.to_owned();
        let versions = make_versions(versions);
        let result = select_version(req, Some(&latest_owned), &versions, target);
        if let Some(eq) = expected_eq {
            assert_eq!(result.as_deref(), Some(eq), "expected_eq mismatch");
        }
        if let Some(ne) = expected_ne {
            assert_ne!(result.as_deref(), Some(ne), "expected_ne violated");
        }
    }

    #[test]
    fn new_creates_registry() {
        install_tls_provider();
        let _registry = CratesIoRegistry::new();
    }

    #[test]
    fn default_creates_registry() {
        install_tls_provider();
        let _registry = CratesIoRegistry::default();
    }

    /// Single-mock async resolve scenarios that differ only by request,
    /// target, mock body, and expected `latest`/`selected`. The original
    /// tests asserted only a subset of fields; `Option<&str>` preserves that
    /// exactly (`None` = no assertion).
    #[rstest]
    // (latest, selected) both checked — full coverage of the original test.
    #[case::latest(
        "^1.0.0",
        TargetLevel::Latest,
        json!({"versions": [
            {"num": "2.0.0", "yanked": false},
            {"num": "1.5.0", "yanked": false},
            {"num": "1.0.0", "yanked": false},
            {"num": "0.9.0", "yanked": true}
        ]}),
        Some("2.0.0"),
        Some("2.0.0")
    )]
    // Original asserted only `selected`.
    #[case::minor(
        "=1.0.0",
        TargetLevel::Minor,
        json!({"versions": [
            {"num": "2.0.0", "yanked": false},
            {"num": "1.5.0", "yanked": false},
            {"num": "1.0.0", "yanked": false}
        ]}),
        None,
        Some("1.5.0")
    )]
    // Original asserted only `selected`.
    #[case::patch(
        "=1.0.0",
        TargetLevel::Patch,
        json!({"versions": [
            {"num": "1.1.0", "yanked": false},
            {"num": "1.0.5", "yanked": false},
            {"num": "1.0.3", "yanked": false},
            {"num": "1.0.0", "yanked": false},
            {"num": "2.0.0", "yanked": false}
        ]}),
        None,
        Some("1.0.5")
    )]
    // Yanked 2.0.0 skipped; latest non-yanked is 1.5.0.
    #[case::skips_yanked(
        "=1.0.0",
        TargetLevel::Latest,
        json!({"versions": [
            {"num": "2.0.0", "yanked": true},
            {"num": "1.5.0", "yanked": false},
            {"num": "1.0.0", "yanked": false}
        ]}),
        Some("1.5.0"),
        Some("1.5.0")
    )]
    // ncu-style: report latest even when current range already satisfies it.
    #[case::already_satisfied(
        "^1.0.0",
        TargetLevel::Latest,
        json!({"versions": [
            {"num": "1.5.0", "yanked": false},
            {"num": "1.0.0", "yanked": false}
        ]}),
        Some("1.5.0"),
        Some("1.5.0")
    )]
    // Prereleases skipped; only stable 1.0.0 available.
    #[case::skips_prerelease(
        "^1.0.0",
        TargetLevel::Latest,
        json!({"versions": [
            {"num": "2.0.0-alpha.1", "yanked": false},
            {"num": "1.5.0-rc.1", "yanked": false},
            {"num": "1.0.0", "yanked": false}
        ]}),
        Some("1.0.0"),
        Some("1.0.0")
    )]
    #[tokio::test]
    async fn resolve_version_single_mock(
        #[case] current_req: &str,
        #[case] target: TargetLevel,
        #[case] body: serde_json::Value,
        #[case] expected_latest: Option<&str>,
        #[case] expected_selected: Option<&str>,
    ) {
        install_tls_provider();
        let mock_server = MockServer::start().await;
        mock_versions_endpoint(&mock_server, "serde", body).await;

        let registry = CratesIoRegistry::with_base_url(&mock_server.uri());
        let result = registry
            .resolve_version(&serde_dep(current_req), target)
            .await
            .unwrap();
        if let Some(exp) = expected_latest {
            assert_eq!(result.latest.as_deref(), Some(exp), "latest mismatch");
        }
        if let Some(exp) = expected_selected {
            assert_eq!(result.selected.as_deref(), Some(exp), "selected mismatch");
        }
    }

    /// Covers the `.json().await.map_err(...)` closure in `fetch_versions`
    /// (the parse-error arm). A 200 status with non-JSON body forces
    /// `response.json()` to fail and routes through the `RegistryLookup`
    /// mapper.
    #[rstest]
    #[tokio::test]
    async fn resolve_version_invalid_json_body_is_err() {
        install_tls_provider();
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/crates/serde/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json {{{"))
            .mount(&mock_server)
            .await;

        let registry = CratesIoRegistry::with_base_url(&mock_server.uri());
        let result = registry
            .resolve_version(&serde_dep("^1.0.0"), TargetLevel::Latest)
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, DcuError::RegistryLookup { .. }));
    }

    /// Covers the `.send().await.map_err(...)` closure in `fetch_versions`
    /// (the network-error arm). Port 1 on loopback is reserved/unbindable, so
    /// the connect attempt fails immediately and the error is mapped to
    /// `RegistryLookup`. No mock server is involved.
    #[rstest]
    #[tokio::test]
    async fn resolve_version_network_error_is_err() {
        install_tls_provider();
        let registry = CratesIoRegistry::with_base_url("http://127.0.0.1:1");
        let result = registry
            .resolve_version(&serde_dep("^1.0.0"), TargetLevel::Latest)
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, DcuError::RegistryLookup { .. }));
    }

    #[rstest]
    #[tokio::test]
    async fn resolve_version_404() {
        install_tls_provider();
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/crates/nonexistent/versions"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        let registry = CratesIoRegistry::with_base_url(&mock_server.uri());
        let dep = DependencySpec {
            name: "nonexistent".to_owned(),
            current_req: "^1.0.0".to_owned(),
            section: DependencySection::Dependencies,
        };
        let result = registry.resolve_version(&dep, TargetLevel::Latest).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, DcuError::RegistryLookup { .. }));
    }

    #[rstest]
    #[tokio::test]
    async fn resolve_batch_returns_sorted_results() {
        install_tls_provider();
        let mock_server = MockServer::start().await;
        mock_versions_endpoint(
            &mock_server,
            "serde",
            json!({"versions": [
                {"num": "2.0.0", "yanked": false},
                {"num": "1.0.0", "yanked": false}
            ]}),
        )
        .await;
        mock_versions_endpoint(
            &mock_server,
            "tokio",
            json!({"versions": [
                {"num": "1.40.0", "yanked": false},
                {"num": "1.0.0", "yanked": false}
            ]}),
        )
        .await;

        let registry = CratesIoRegistry::with_base_url(&mock_server.uri());
        let deps = vec![
            DependencySpec {
                name: "serde".to_owned(),
                current_req: "^1.0.0".to_owned(),
                section: DependencySection::Dependencies,
            },
            DependencySpec {
                name: "tokio".to_owned(),
                current_req: "^1.0.0".to_owned(),
                section: DependencySection::Dependencies,
            },
        ];
        let results = registry.resolve_batch(&deps, TargetLevel::Latest).await;
        assert_eq!(results.len(), 2);
        // Results are sorted by index.
        let (idx0, ref res0) = results[0];
        let (idx1, ref res1) = results[1];
        assert_eq!(idx0, 0);
        assert_eq!(idx1, 1);
        assert_eq!(res0.as_ref().unwrap().latest, Some("2.0.0".to_owned()));
        assert_eq!(res1.as_ref().unwrap().latest, Some("1.40.0".to_owned()));
    }

    #[rstest]
    #[tokio::test]
    async fn resolve_version_newest_by_date_vs_greatest() {
        install_tls_provider();
        let mock_server = MockServer::start().await;
        // 1.5.0 has the LATEST publish date despite 2.0.0 being the higher
        // version (a backport published after the new major). `newest` must
        // pick 1.5.0; `greatest`/`latest` would pick 2.0.0.
        mock_versions_endpoint(
            &mock_server,
            "serde",
            json!({"versions": [
                {"num": "2.0.0", "yanked": false, "created_at": "2023-01-01T00:00:00Z"},
                {"num": "1.5.0", "yanked": false, "created_at": "2024-06-01T00:00:00Z"},
                {"num": "1.0.0", "yanked": false, "created_at": "2022-01-01T00:00:00Z"}
            ]}),
        )
        .await;

        let registry = CratesIoRegistry::with_base_url(&mock_server.uri());
        let dep = serde_dep("^1.0.0");
        let result = registry
            .resolve_version(&dep, TargetLevel::Newest)
            .await
            .unwrap();
        assert_eq!(result.selected.as_deref(), Some("1.5.0"));
        // Sanity: Greatest still picks the highest version number.
        let greatest = registry
            .resolve_version(&dep, TargetLevel::Greatest)
            .await
            .unwrap();
        assert_eq!(greatest.selected.as_deref(), Some("2.0.0"));
    }

    #[rstest]
    #[tokio::test]
    async fn resolve_version_with_tracing() {
        install_tls_provider();
        // Install a trace-level subscriber so trace!() arguments are evaluated.
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_test_writer()
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let mock_server = MockServer::start().await;
        mock_versions_endpoint(
            &mock_server,
            "serde",
            json!({"versions": [
                {"num": "2.0.0", "yanked": false},
                {"num": "1.0.0", "yanked": false}
            ]}),
        )
        .await;

        let registry = CratesIoRegistry::with_base_url(&mock_server.uri());
        let result = registry
            .resolve_version(&serde_dep("^1.0.0"), TargetLevel::Latest)
            .await
            .unwrap();
        assert_eq!(result.latest, Some("2.0.0".to_owned()));
    }
}
