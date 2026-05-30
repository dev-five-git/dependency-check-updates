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
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|e| DcuError::RegistryLookup {
                package: name.to_owned(),
                detail: format!("semaphore error: {e}"),
            })?;

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
        .filter_map(|v| semver::Version::parse(&v.num).ok().map(|parsed| (&v.created_at, parsed)))
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

    #[test]
    fn test_parse_base_version_caret() {
        let v = parse_base_version("^1.2.3").unwrap();
        assert_eq!((v.major, v.minor, v.patch), (1, 2, 3));
    }

    #[test]
    fn test_parse_base_version_tilde() {
        let v = parse_base_version("~1.2.3").unwrap();
        assert_eq!((v.major, v.minor, v.patch), (1, 2, 3));
    }

    #[test]
    fn test_select_latest() {
        let latest = "2.0.0".to_owned();
        let versions = make_versions(&["1.0.0", "1.5.0", "2.0.0"]);
        let result = select_version("^1.0", Some(&latest), &versions, TargetLevel::Latest);
        assert_eq!(result, Some("2.0.0".to_owned()));
    }

    #[test]
    fn test_select_minor() {
        let latest = "2.0.0".to_owned();
        let versions = make_versions(&["1.0.0", "1.5.0", "2.0.0"]);
        let result = select_version("^1.0.0", Some(&latest), &versions, TargetLevel::Minor);
        assert_eq!(result, Some("1.5.0".to_owned()));
    }

    #[test]
    fn test_select_patch() {
        let latest = "2.0.0".to_owned();
        let versions = make_versions(&["1.0.0", "1.0.5", "1.1.0", "2.0.0"]);
        let result = select_version("^1.0.0", Some(&latest), &versions, TargetLevel::Patch);
        assert_eq!(result, Some("1.0.5".to_owned()));
    }

    #[test]
    fn test_skip_prerelease() {
        let latest = "1.0.0".to_owned();
        let versions = make_versions(&["1.0.0", "2.0.0-rc.1"]);
        let result = select_version("^1.0.0", Some(&latest), &versions, TargetLevel::Latest);
        assert_eq!(result, Some("1.0.0".to_owned()));
    }

    #[test]
    fn test_select_greatest_includes_prerelease() {
        // Greatest: README says "Highest version number, INCLUDING prereleases".
        // Previously this filtered prereleases out (bug). Now it must include them.
        let latest = "1.0.0".to_owned();
        let versions = make_versions(&["1.0.0", "2.0.0-rc.1"]);
        let result = select_version("^1.0.0", Some(&latest), &versions, TargetLevel::Greatest);
        assert_eq!(result, Some("2.0.0-rc.1".to_owned()));
    }

    #[test]
    fn test_select_newest_includes_prerelease() {
        // Newest is MVP-aliased to Greatest. Must include prereleases.
        let latest = "1.0.0".to_owned();
        let versions = make_versions(&["1.0.0", "2.0.0-rc.1"]);
        let result = select_version("^1.0.0", Some(&latest), &versions, TargetLevel::Newest);
        assert_eq!(result, Some("2.0.0-rc.1".to_owned()));
    }

    #[test]
    fn test_latest_prerelease_tail_ignores_unrelated_prereleases() {
        // Current: 2.0.0-rc.37. 3.0.0-alpha.1 (different train) must NOT be
        // picked. Only the same-train prereleases or stables qualify.
        let latest = "1.1.20".to_owned();
        let versions = make_versions(&["1.1.20", "2.0.0-rc.37", "3.0.0-alpha.1"]);
        let result = select_version("2.0.0-rc.37", Some(&latest), &versions, TargetLevel::Latest);
        // 3.0.0-alpha.1 is unrelated prerelease → excluded.
        // 2.0.0-rc.37 is current → not newer.
        // 1.1.20 stable is older → allowed by select but the compute_updates
        // safety net will skip the downgrade. select_version itself just
        // picks the highest eligible candidate, which is 2.0.0-rc.37 (self).
        // Here we confirm that unrelated prereleases are NOT selected.
        assert_ne!(result, Some("3.0.0-alpha.1".to_owned()));
    }

    #[test]
    fn test_latest_prerelease_current_picks_higher_stable_major() {
        // Current: 2.0.0-rc.37. Registry: 1.1.20, 2.0.0-rc.37, 3.0.0 (higher stable major).
        // Expected: 3.0.0 — stable is ALWAYS preferred over staying on a prerelease,
        // regardless of "train". Only prerelease candidates are train-gated.
        let latest = "3.0.0".to_owned();
        let versions = make_versions(&["1.1.20", "2.0.0-rc.37", "3.0.0"]);
        let result = select_version("2.0.0-rc.37", Some(&latest), &versions, TargetLevel::Latest);
        assert_eq!(result, Some("3.0.0".to_owned()));
    }

    #[test]
    fn test_latest_prerelease_current_stable_wins_over_unrelated_prerelease() {
        // Current: 2.0.0-rc.37. Registry: 1.1.20, 2.0.0-rc.37, 3.0.0-alpha.1, 3.0.0.
        // The unrelated prerelease 3.0.0-alpha.1 must be skipped, but the
        // stable 3.0.0 (even from a different train) must be picked.
        let latest = "3.0.0".to_owned();
        let versions = make_versions(&["1.1.20", "2.0.0-rc.37", "3.0.0-alpha.1", "3.0.0"]);
        let result = select_version("2.0.0-rc.37", Some(&latest), &versions, TargetLevel::Latest);
        assert_eq!(result, Some("3.0.0".to_owned()));
    }

    #[test]
    fn test_latest_prerelease_tail_jumps_to_stable() {
        // Current: 2.0.0-rc.37. When 2.0.0 stable is available, pick it
        // (stable > prerelease of same M.m.p in semver ordering).
        let latest = "2.0.0".to_owned();
        let versions = make_versions(&["1.1.20", "2.0.0-rc.37", "2.0.0-rc.40", "2.0.0"]);
        let result = select_version("2.0.0-rc.37", Some(&latest), &versions, TargetLevel::Latest);
        assert_eq!(result, Some("2.0.0".to_owned()));
    }

    #[test]
    fn test_latest_sea_orm_regression() {
        // End-to-end regression for the sea-orm 2.0.0-rc.37 -> 1.1.20 bug.
        // select_version will naturally pick 2.0.0-rc.37 (self) as the only
        // eligible candidate in the prerelease train, and compute_updates
        // (CLI layer) then skips it as "not newer".
        let latest = "1.1.20".to_owned();
        let versions = make_versions(&["1.1.20", "2.0.0-rc.37"]);
        let result = select_version("2.0.0-rc.37", Some(&latest), &versions, TargetLevel::Latest);
        // NOT 1.1.20 (that would be a downgrade).
        assert_ne!(result, Some("1.1.20".to_owned()));
        // Self is acceptable; the CLI's safety net filters equal-or-lower.
        assert_eq!(result, Some("2.0.0-rc.37".to_owned()));
    }

    #[test]
    fn test_latest_stable_current_excludes_prerelease() {
        // When current is stable, latest must NOT pick prereleases
        // (preserves existing behavior for stable users).
        let latest = "1.0.0".to_owned();
        let versions = make_versions(&["1.0.0", "2.0.0-rc.1"]);
        let result = select_version("1.0.0", Some(&latest), &versions, TargetLevel::Latest);
        assert_eq!(result, Some("1.0.0".to_owned()));
    }

    #[tokio::test]
    async fn test_resolve_version_latest() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        install_tls_provider();
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/crates/serde/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "versions": [
                    {"num": "2.0.0", "yanked": false},
                    {"num": "1.5.0", "yanked": false},
                    {"num": "1.0.0", "yanked": false},
                    {"num": "0.9.0", "yanked": true}
                ]
            })))
            .mount(&mock_server)
            .await;

        let registry = CratesIoRegistry::with_base_url(&mock_server.uri());
        let dep = DependencySpec {
            name: "serde".to_owned(),
            current_req: "^1.0.0".to_owned(),
            section: dependency_check_updates_core::DependencySection::Dependencies,
        };
        let result = registry
            .resolve_version(&dep, TargetLevel::Latest)
            .await
            .unwrap();
        assert_eq!(result.latest, Some("2.0.0".to_owned()));
        assert_eq!(result.selected, Some("2.0.0".to_owned()));
    }

    #[tokio::test]
    async fn test_resolve_version_minor() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        install_tls_provider();
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/crates/serde/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "versions": [
                    {"num": "2.0.0", "yanked": false},
                    {"num": "1.5.0", "yanked": false},
                    {"num": "1.0.0", "yanked": false}
                ]
            })))
            .mount(&mock_server)
            .await;

        let registry = CratesIoRegistry::with_base_url(&mock_server.uri());
        let dep = DependencySpec {
            name: "serde".to_owned(),
            current_req: "=1.0.0".to_owned(),
            section: dependency_check_updates_core::DependencySection::Dependencies,
        };
        let result = registry
            .resolve_version(&dep, TargetLevel::Minor)
            .await
            .unwrap();
        // Minor: stays on same major (1.x), highest is 1.5.0
        assert_eq!(result.selected, Some("1.5.0".to_owned()));
    }

    #[tokio::test]
    async fn test_resolve_version_patch() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        install_tls_provider();
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/crates/serde/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "versions": [
                    {"num": "1.1.0", "yanked": false},
                    {"num": "1.0.5", "yanked": false},
                    {"num": "1.0.3", "yanked": false},
                    {"num": "1.0.0", "yanked": false},
                    {"num": "2.0.0", "yanked": false}
                ]
            })))
            .mount(&mock_server)
            .await;

        let registry = CratesIoRegistry::with_base_url(&mock_server.uri());
        let dep = DependencySpec {
            name: "serde".to_owned(),
            current_req: "=1.0.0".to_owned(),
            section: dependency_check_updates_core::DependencySection::Dependencies,
        };
        let result = registry
            .resolve_version(&dep, TargetLevel::Patch)
            .await
            .unwrap();
        // Patch: stays on same major.minor (1.0.x), highest is 1.0.5
        assert_eq!(result.selected, Some("1.0.5".to_owned()));
    }

    #[tokio::test]
    async fn test_resolve_version_skips_yanked() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        install_tls_provider();
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/crates/serde/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "versions": [
                    {"num": "2.0.0", "yanked": true},
                    {"num": "1.5.0", "yanked": false},
                    {"num": "1.0.0", "yanked": false}
                ]
            })))
            .mount(&mock_server)
            .await;

        let registry = CratesIoRegistry::with_base_url(&mock_server.uri());
        let dep = DependencySpec {
            name: "serde".to_owned(),
            current_req: "=1.0.0".to_owned(),
            section: dependency_check_updates_core::DependencySection::Dependencies,
        };
        let result = registry
            .resolve_version(&dep, TargetLevel::Latest)
            .await
            .unwrap();
        // 2.0.0 is yanked, so latest non-yanked is 1.5.0
        assert_eq!(result.latest, Some("1.5.0".to_owned()));
        assert_eq!(result.selected, Some("1.5.0".to_owned()));
    }

    #[tokio::test]
    async fn test_resolve_version_already_satisfied() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        install_tls_provider();
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/crates/serde/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "versions": [
                    {"num": "1.5.0", "yanked": false},
                    {"num": "1.0.0", "yanked": false}
                ]
            })))
            .mount(&mock_server)
            .await;

        let registry = CratesIoRegistry::with_base_url(&mock_server.uri());
        // current_req ^1.0.0 already satisfies 1.5.0 (caret allows minor/patch bumps)
        let dep = DependencySpec {
            name: "serde".to_owned(),
            current_req: "^1.0.0".to_owned(),
            section: dependency_check_updates_core::DependencySection::Dependencies,
        };
        let result = registry
            .resolve_version(&dep, TargetLevel::Latest)
            .await
            .unwrap();
        // ncu-style: always report latest; CLI's compute_updates decides if a spec bump is needed.
        assert_eq!(result.selected.as_deref(), Some("1.5.0"));
        assert_eq!(result.latest, Some("1.5.0".to_owned()));
    }

    #[tokio::test]
    async fn test_resolve_version_404() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

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
            section: dependency_check_updates_core::DependencySection::Dependencies,
        };
        let result = registry.resolve_version(&dep, TargetLevel::Latest).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, DcuError::RegistryLookup { .. }));
    }

    #[tokio::test]
    async fn test_resolve_batch() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        install_tls_provider();
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/crates/serde/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "versions": [
                    {"num": "2.0.0", "yanked": false},
                    {"num": "1.0.0", "yanked": false}
                ]
            })))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/crates/tokio/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "versions": [
                    {"num": "1.40.0", "yanked": false},
                    {"num": "1.0.0", "yanked": false}
                ]
            })))
            .mount(&mock_server)
            .await;

        let registry = CratesIoRegistry::with_base_url(&mock_server.uri());
        let deps = vec![
            DependencySpec {
                name: "serde".to_owned(),
                current_req: "^1.0.0".to_owned(),
                section: dependency_check_updates_core::DependencySection::Dependencies,
            },
            DependencySpec {
                name: "tokio".to_owned(),
                current_req: "^1.0.0".to_owned(),
                section: dependency_check_updates_core::DependencySection::Dependencies,
            },
        ];
        let results = registry.resolve_batch(&deps, TargetLevel::Latest).await;
        assert_eq!(results.len(), 2);
        // Results are sorted by index
        let (idx0, ref res0) = results[0];
        let (idx1, ref res1) = results[1];
        assert_eq!(idx0, 0);
        assert_eq!(idx1, 1);
        assert_eq!(res0.as_ref().unwrap().latest, Some("2.0.0".to_owned()));
        assert_eq!(res1.as_ref().unwrap().latest, Some("1.40.0".to_owned()));
    }

    #[tokio::test]
    async fn test_resolve_version_skips_prerelease() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        install_tls_provider();
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/crates/serde/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "versions": [
                    {"num": "2.0.0-alpha.1", "yanked": false},
                    {"num": "1.5.0-rc.1", "yanked": false},
                    {"num": "1.0.0", "yanked": false}
                ]
            })))
            .mount(&mock_server)
            .await;

        let registry = CratesIoRegistry::with_base_url(&mock_server.uri());
        let dep = DependencySpec {
            name: "serde".to_owned(),
            current_req: "^1.0.0".to_owned(),
            section: dependency_check_updates_core::DependencySection::Dependencies,
        };
        let result = registry
            .resolve_version(&dep, TargetLevel::Latest)
            .await
            .unwrap();
        // Pre-release versions should be skipped; only stable 1.0.0 is available
        assert_eq!(result.latest, Some("1.0.0".to_owned()));
        // ncu-style: resolve returns latest; bare-version comparison happens in compute_updates
        assert_eq!(result.selected.as_deref(), Some("1.0.0"));
    }

    #[tokio::test]
    async fn test_resolve_version_newest_by_date() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        install_tls_provider();
        let mock_server = MockServer::start().await;
        // 1.5.0 has the LATEST publish date despite 2.0.0 being the higher
        // version (a backport published after the new major). `newest` must
        // pick 1.5.0; `greatest`/`latest` would pick 2.0.0.
        Mock::given(method("GET"))
            .and(path("/crates/serde/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "versions": [
                    {"num": "2.0.0", "yanked": false, "created_at": "2023-01-01T00:00:00Z"},
                    {"num": "1.5.0", "yanked": false, "created_at": "2024-06-01T00:00:00Z"},
                    {"num": "1.0.0", "yanked": false, "created_at": "2022-01-01T00:00:00Z"}
                ]
            })))
            .mount(&mock_server)
            .await;

        let registry = CratesIoRegistry::with_base_url(&mock_server.uri());
        let dep = DependencySpec {
            name: "serde".to_owned(),
            current_req: "^1.0.0".to_owned(),
            section: dependency_check_updates_core::DependencySection::Dependencies,
        };
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

    #[test]
    fn test_new_creates_registry() {
        install_tls_provider();
        let _registry = CratesIoRegistry::new();
    }

    #[test]
    fn test_default_creates_registry() {
        install_tls_provider();
        let _registry = CratesIoRegistry::default();
    }

    #[test]
    fn test_select_version_empty_versions() {
        let latest = "1.0.0".to_owned();
        let versions: Vec<semver::Version> = vec![];
        let result = select_version("^1.0.0", Some(&latest), &versions, TargetLevel::Latest);
        assert_eq!(result, Some("1.0.0".to_owned()));
    }

    #[test]
    fn test_select_version_minor_unparseable_falls_back() {
        let latest = "2.0.0".to_owned();
        let versions = make_versions(&["1.0.0", "2.0.0"]);
        let result = select_version("*", Some(&latest), &versions, TargetLevel::Minor);
        assert_eq!(result, Some("2.0.0".to_owned()));
    }

    #[test]
    fn test_select_version_patch_unparseable_falls_back() {
        let latest = "2.0.0".to_owned();
        let versions = make_versions(&["1.0.0", "2.0.0"]);
        let result = select_version("*", Some(&latest), &versions, TargetLevel::Patch);
        assert_eq!(result, Some("2.0.0".to_owned()));
    }

    #[tokio::test]
    async fn test_resolve_version_with_tracing() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        install_tls_provider();

        // Install a trace-level subscriber so trace!() arguments are evaluated.
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_test_writer()
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let mock_server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/crates/serde/versions")).respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "versions": [{"num": "2.0.0", "yanked": false}, {"num": "1.0.0", "yanked": false}] }))).mount(&mock_server).await;

        let registry = CratesIoRegistry::with_base_url(&mock_server.uri());
        let dep = DependencySpec {
            name: "serde".to_owned(),
            current_req: "^1.0.0".to_owned(),
            section: dependency_check_updates_core::DependencySection::Dependencies,
        };
        let result = registry
            .resolve_version(&dep, TargetLevel::Latest)
            .await
            .unwrap();
        assert_eq!(result.latest, Some("2.0.0".to_owned()));
    }

}
