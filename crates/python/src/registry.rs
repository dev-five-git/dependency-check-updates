//! `PyPI` registry client for looking up Python package versions.

use std::str::FromStr;
use std::sync::Arc;

use reqwest::Client;
use serde::Deserialize;
use tokio::sync::Semaphore;
use tracing::{debug, trace};

use dependency_check_updates_core::{
    DEFAULT_MAX_CONCURRENT_REQUESTS, DcuError, DependencySpec, ResolvedVersion, TargetLevel,
    build_client, parse_and_select, strip_range_prefix,
};

/// `PyPI` registry client.
#[derive(Clone)]
pub struct PyPiRegistry {
    client: Client,
    semaphore: Arc<Semaphore>,
    base_url: Arc<str>,
}

#[derive(Debug, Deserialize)]
struct PyPiResponse {
    info: PyPiInfo,
    /// Map of version string → list of distribution files. Used to enumerate
    /// every published release (for `--target` resolution) and their upload
    /// times (for `newest`). Absent/partial responses default to empty, in
    /// which case resolution falls back to `info.version`.
    #[serde(default)]
    releases: std::collections::HashMap<String, Vec<PyPiFile>>,
}

#[derive(Debug, Deserialize)]
struct PyPiInfo {
    version: String,
}

#[derive(Debug, Deserialize)]
struct PyPiFile {
    #[serde(default)]
    upload_time_iso_8601: String,
    #[serde(default)]
    yanked: bool,
}

/// Predicate for a release with at least one non-yanked file.
///
/// Centralises the filter used by both the `Newest` arm and the
/// version-sorted arm of [`PyPiRegistry::resolve_version`]'s slow path so
/// the two stay in sync without re-introducing the wasted
/// `(Version, &str)` tuple `Vec` the old code paid for on every non-Newest
/// lookup.
fn is_usable_release(files: &[PyPiFile]) -> bool {
    files.iter().any(|f| !f.yanked)
}

impl PyPiRegistry {
    /// Create a new `PyPI` registry client.
    #[must_use]
    pub fn new() -> Self {
        Self::with_base_url("https://pypi.org/pypi")
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

    /// Fetch package info from `PyPI`.
    async fn fetch_package_info(&self, name: &str) -> Result<PyPiResponse, DcuError> {
        // `acquire` only errors once the semaphore is closed; this registry
        // never closes its semaphore, so success is the sole reachable path
        // (mirrors `build_client`'s infallible-by-construction `expect`).
        let _permit = self
            .semaphore
            .acquire()
            .await
            .expect("semaphore is never closed");

        // PyPI uses normalized names (lowercase, hyphens)
        let normalized = name.to_lowercase().replace('_', "-");
        let url = format!("{}/{normalized}/json", self.base_url);
        debug!(package = name, %url, "fetching PyPI package info");

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

        response.json().await.map_err(|e| DcuError::RegistryLookup {
            package: name.to_owned(),
            detail: format!("failed to parse response: {e}"),
        })
    }

    /// Resolve the target version for a dependency.
    ///
    /// Enumerates every published release from the `PyPI` `releases` map,
    /// parses them with PEP 440 ordering, and applies the requested
    /// [`TargetLevel`] via the shared selection algorithm. `newest` uses the
    /// per-file `upload_time_iso_8601` timestamps. When `releases` is empty
    /// (a minimal/legacy response), it falls back to the `PyPI`-reported
    /// `info.version`.
    ///
    /// # Errors
    ///
    /// Returns an error if the registry lookup fails.
    pub async fn resolve_version(
        &self,
        dep: &DependencySpec,
        target: TargetLevel,
    ) -> Result<ResolvedVersion, DcuError> {
        let info = self.fetch_package_info(&dep.name).await?;

        // PyPI's `info.version` is the canonical latest stable; it doubles as
        // the fallback for `Latest`/empty-list and unparseable `Minor`/`Patch`.
        let latest = Some(info.info.version);

        // Detect if the user's current requirement is a prerelease. When it
        // is, we cannot use the `info.version` fast path because the user may
        // be ahead of the canonical stable version (e.g. `2.0.0rc1` while
        // `info.version` points at `1.1.20`), and we must consider the full
        // release list to preserve the prerelease-tail policy that
        // `parse_and_select` encodes. An unparseable requirement (e.g. `"*"`)
        // is treated as stable, matching the slow path's `current = None`
        // branch which also routes through `latest_for_stable = info.version`.
        let current_is_prerelease =
            pep440_rs::Version::from_str(strip_range_prefix(&dep.current_req))
                .is_ok_and(|v| v.any_prerelease());

        // Fast path: Latest + current is stable → return PyPI's canonical
        // `info.version` directly. The slow path's `parse_and_select` arm for
        // (`Latest`, stable current) is documented to fall back to
        // `latest_for_stable` (= `info.version`), so this is byte-equivalent —
        // it just avoids enumerating + parsing + sorting every release.
        // Mirrors the `dist-tags.latest` fast path in
        // `crates/node/src/registry.rs::resolve_version`.
        let selected = if target == TargetLevel::Latest && !current_is_prerelease {
            trace!(
                package = %dep.name,
                latest = ?latest,
                "fast path: using PyPI info.version directly"
            );
            latest.clone()
        } else if target == TargetLevel::Newest {
            // Most recently uploaded by date (ISO-8601 sorts chronologically),
            // which can differ from the highest version number. Stream the
            // releases straight into `max_by` — no intermediate `Vec`, no
            // sort, since the version ordering the old slow path computed
            // was thrown away in this arm anyway. The upload `&str` is
            // borrowed out of `info.releases` and dies with the iterator.
            // `max_by` returns `None` only on an empty iterator, which
            // already means there is nothing to fall back to.
            info.releases
                .iter()
                .filter_map(|(ver_str, files)| {
                    if !is_usable_release(files) {
                        return None;
                    }
                    let parsed = pep440_rs::Version::from_str(ver_str).ok()?;
                    let upload = files
                        .iter()
                        .map(|f| f.upload_time_iso_8601.as_str())
                        .max()
                        .unwrap_or("");
                    Some((parsed, upload))
                })
                .max_by(|a, b| a.1.cmp(b.1))
                .map(|(v, _)| v.to_string())
        } else {
            // `parse_and_select` still wants an ascending list, so we sort
            // — but we sort plain `pep440_rs::Version`s instead of
            // `(Version, &str)` tuples whose `&str` half this arm never
            // reads, dropping one allocation pass and a wider comparator.
            // Shared strip→parse→select sequence centralised in `core`;
            // PyPI's `info.version` (canonical latest stable) doubles as
            // the fallback for the stable-`Latest` and unparseable-
            // `Minor`/`Patch` cases.
            let mut versions: Vec<pep440_rs::Version> = Vec::with_capacity(info.releases.len());
            versions.extend(info.releases.iter().filter_map(|(ver_str, files)| {
                if !is_usable_release(files) {
                    return None;
                }
                pep440_rs::Version::from_str(ver_str).ok()
            }));
            // Unstable sort matches the cargo/npm registry convention for
            // these final, already-unique version lists; `pdqsort` skips
            // `Timsort`'s auxiliary buffer for the same observable ordering.
            // See 0007-analyze.md F2.
            versions.sort_unstable();
            parse_and_select(&dep.current_req, &versions, target, latest.as_deref())
        };

        debug!(
            package = %dep.name,
            current = %dep.current_req,
            selected = ?selected,
            target = %target,
            "resolved PyPI version"
        );

        Ok(ResolvedVersion { latest, selected })
    }

    /// Resolve versions for a batch of dependencies concurrently.
    ///
    /// Delegates the `join_all` pipeline to
    /// [`dependency_check_updates_core::resolve_batch_concurrent`] so the
    /// concurrency model — no `tokio::spawn`, no per-dep `JoinHandle` /
    /// `DependencySpec` clones, source-order preserved — lives in exactly one
    /// place across every per-dep registry.
    pub async fn resolve_batch(
        &self,
        deps: &[DependencySpec],
        target: TargetLevel,
    ) -> Vec<(usize, Result<ResolvedVersion, DcuError>)> {
        dependency_check_updates_core::resolve_batch_concurrent(deps, |dep| {
            self.resolve_version(dep, target)
        })
        .await
    }
}

impl Default for PyPiRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::PyPiRegistry;

    use dependency_check_updates_core::{DependencySection, DependencySpec, TargetLevel};
    use rstest::{fixture, rstest};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Install the rustls ring provider once per process so reqwest
    /// (rustls-no-provider) works. Safe to call repeatedly: subsequent installs
    /// no-op because a default provider is already set.
    fn install_crypto_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    /// Async fixture: install the crypto provider, then start a fresh
    /// `wiremock::MockServer` for the test. Each test gets its own server so
    /// mounted mocks never leak across cases.
    #[fixture]
    async fn mock_server() -> MockServer {
        install_crypto_provider();
        MockServer::start().await
    }

    /// Build a project-dependency spec — keeps individual `#[case]` rows short.
    fn make_dep(name: &str, current_req: &str) -> DependencySpec {
        DependencySpec {
            name: name.to_owned(),
            current_req: current_req.to_owned(),
            section: DependencySection::ProjectDependencies,
            path_version: None,
        }
    }

    #[test]
    fn normalized_name_converts_underscores_and_case() {
        // PyPI normalizes names: underscores -> hyphens, lowercase
        let name = "My_Package";
        let normalized = name.to_lowercase().replace('_', "-");
        assert_eq!(normalized, "my-package");
    }

    #[rstest]
    #[case::new_ctor(|| PyPiRegistry::new())]
    #[case::default_ctor(|| PyPiRegistry::default())]
    fn registry_constructors_succeed(#[case] make: fn() -> PyPiRegistry) {
        install_crypto_provider();
        let _registry = make();
    }

    /// `Latest`-target lookups with a minimal `info.version` body. Covers the
    /// basic and normalized-name (`My_Package` → `/my-package/json`) paths.
    #[rstest]
    #[case::basic_latest("/requests/json", "requests", "2.31.0")]
    #[case::normalized_name_lookup("/my-package/json", "My_Package", "1.2.3")]
    #[tokio::test]
    async fn resolve_version_latest_cases(
        #[future] mock_server: MockServer,
        #[case] mock_path: &str,
        #[case] dep_name: &str,
        #[case] expected_version: &str,
    ) {
        let server = mock_server.await;
        Mock::given(method("GET"))
            .and(path(mock_path))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "info": {"version": expected_version}
            })))
            .mount(&server)
            .await;

        let registry = PyPiRegistry::with_base_url(&server.uri());
        let dep = make_dep(dep_name, ">=1.0.0");
        let result = registry
            .resolve_version(&dep, TargetLevel::Latest)
            .await
            .expect("resolve_version should succeed");
        assert_eq!(result.selected, Some(expected_version.to_owned()));
    }

    #[rstest]
    #[tokio::test]
    async fn resolve_version_404_returns_error(#[future] mock_server: MockServer) {
        let server = mock_server.await;
        Mock::given(method("GET"))
            .and(path("/nonexistent-package/json"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let registry = PyPiRegistry::with_base_url(&server.uri());
        let dep = make_dep("nonexistent-package", ">=1.0.0");
        let result = registry.resolve_version(&dep, TargetLevel::Latest).await;
        assert!(result.is_err(), "expected error for 404 response");
    }

    #[rstest]
    #[tokio::test]
    async fn resolve_batch_preserves_order_and_results(#[future] mock_server: MockServer) {
        let server = mock_server.await;
        Mock::given(method("GET"))
            .and(path("/requests/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "info": {"version": "2.31.0"}
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/flask/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "info": {"version": "3.0.0"}
            })))
            .mount(&server)
            .await;

        let registry = PyPiRegistry::with_base_url(&server.uri());
        let deps = vec![
            make_dep("requests", ">=2.28.0"),
            make_dep("flask", ">=2.0.0"),
        ];

        let results = registry.resolve_batch(&deps, TargetLevel::Latest).await;

        assert_eq!(results.len(), 2);
        let (idx0, ref res0) = results[0];
        let (idx1, ref res1) = results[1];
        assert_eq!(idx0, 0);
        assert_eq!(idx1, 1);
        assert_eq!(
            res0.as_ref().expect("requests should resolve").selected,
            Some("2.31.0".to_owned())
        );
        assert_eq!(
            res1.as_ref().expect("flask should resolve").selected,
            Some("3.0.0".to_owned())
        );
    }

    /// `Patch`/`Minor` targets against a 4-release Flask response: each target
    /// must clamp to the highest version inside its band.
    #[rstest]
    #[case::patch_stays_in_2_0_x(TargetLevel::Patch, "2.0.5")]
    #[case::minor_stays_in_2_x(TargetLevel::Minor, "2.1.0")]
    #[tokio::test]
    async fn resolve_version_patch_minor_cases(
        #[future] mock_server: MockServer,
        #[case] target: TargetLevel,
        #[case] expected: &str,
    ) {
        let server = mock_server.await;
        Mock::given(method("GET"))
            .and(path("/flask/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "info": {"version": "3.0.0"},
                "releases": {
                    "2.0.0": [{"upload_time_iso_8601": "2021-01-01T00:00:00Z", "yanked": false}],
                    "2.0.5": [{"upload_time_iso_8601": "2021-06-01T00:00:00Z", "yanked": false}],
                    "2.1.0": [{"upload_time_iso_8601": "2021-09-01T00:00:00Z", "yanked": false}],
                    "3.0.0": [{"upload_time_iso_8601": "2022-01-01T00:00:00Z", "yanked": false}]
                }
            })))
            .mount(&server)
            .await;

        let registry = PyPiRegistry::with_base_url(&server.uri());
        let dep = make_dep("flask", ">=2.0.0");
        let result = registry.resolve_version(&dep, target).await.unwrap();
        assert_eq!(result.selected.as_deref(), Some(expected));
    }

    /// `Newest` picks by upload date (1.5.0 published most recently),
    /// `Greatest` picks by version (2.0.0 is the highest). Same fixture body.
    #[rstest]
    #[case::newest_by_upload_date(TargetLevel::Newest, "1.5.0")]
    #[case::greatest_by_version(TargetLevel::Greatest, "2.0.0")]
    #[tokio::test]
    async fn resolve_version_newest_vs_greatest(
        #[future] mock_server: MockServer,
        #[case] target: TargetLevel,
        #[case] expected: &str,
    ) {
        let server = mock_server.await;
        Mock::given(method("GET"))
            .and(path("/requests/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "info": {"version": "2.0.0"},
                "releases": {
                    "1.0.0": [{"upload_time_iso_8601": "2022-01-01T00:00:00Z", "yanked": false}],
                    "2.0.0": [{"upload_time_iso_8601": "2023-01-01T00:00:00Z", "yanked": false}],
                    "1.5.0": [{"upload_time_iso_8601": "2024-06-01T00:00:00Z", "yanked": false}]
                }
            })))
            .mount(&server)
            .await;

        let registry = PyPiRegistry::with_base_url(&server.uri());
        let dep = make_dep("requests", ">=1.0.0");
        let result = registry.resolve_version(&dep, target).await.unwrap();
        assert_eq!(result.selected.as_deref(), Some(expected));
    }

    /// Covers the `.json().await.map_err(...)` parse-error closure in
    /// `fetch_package_info` (registry.rs lines 104-105). The mock returns
    /// HTTP 200 with a body that is not valid JSON, so the deserializer
    /// fails and `resolve_version` must surface the error.
    #[rstest]
    #[tokio::test]
    async fn resolve_version_invalid_json_returns_error(#[future] mock_server: MockServer) {
        let server = mock_server.await;
        Mock::given(method("GET"))
            .and(path("/badjson/json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("this is not json {{{")
                    .insert_header("content-type", "application/json"),
            )
            .mount(&server)
            .await;

        let registry = PyPiRegistry::with_base_url(&server.uri());
        let dep = make_dep("badjson", ">=1.0.0");
        let result = registry.resolve_version(&dep, TargetLevel::Latest).await;
        assert!(result.is_err(), "expected JSON parse error to bubble up");
    }

    /// Covers the `.send().await.map_err(...)` network-error closure
    /// (registry.rs lines 91-92). Port 1 on loopback refuses connections,
    /// so reqwest returns a connect error before any HTTP response is
    /// produced.
    #[tokio::test]
    async fn resolve_version_network_error_returns_err() {
        install_crypto_provider();
        let registry = PyPiRegistry::with_base_url("http://127.0.0.1:1");
        let dep = make_dep("anything", ">=1.0.0");
        let result = registry.resolve_version(&dep, TargetLevel::Latest).await;
        assert!(result.is_err(), "expected network error from refused port");
    }

    /// Covers the `files.is_empty() || files.iter().all(|f| f.yanked)` early
    /// return inside the candidate `filter_map` (registry.rs line 139). The
    /// `1.9.0` release has every file marked yanked, so `Greatest` must
    /// pick `1.5.0` (the highest non-yanked release) instead.
    #[rstest]
    #[tokio::test]
    async fn resolve_version_skips_all_yanked_release(#[future] mock_server: MockServer) {
        let server = mock_server.await;
        Mock::given(method("GET"))
            .and(path("/yanky/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "info": {"version": "1.5.0"},
                "releases": {
                    "1.0.0": [{"upload_time_iso_8601": "2022-01-01T00:00:00Z", "yanked": false}],
                    "1.5.0": [{"upload_time_iso_8601": "2023-01-01T00:00:00Z", "yanked": false}],
                    "1.9.0": [
                        {"upload_time_iso_8601": "2024-01-01T00:00:00Z", "yanked": true},
                        {"upload_time_iso_8601": "2024-01-02T00:00:00Z", "yanked": true}
                    ]
                }
            })))
            .mount(&server)
            .await;

        let registry = PyPiRegistry::with_base_url(&server.uri());
        let dep = make_dep("yanky", ">=1.0.0");
        let result = registry
            .resolve_version(&dep, TargetLevel::Greatest)
            .await
            .expect("resolve_version should succeed");
        assert_eq!(
            result.selected.as_deref(),
            Some("1.5.0"),
            "all-yanked 1.9.0 must be excluded; Greatest should fall back to 1.5.0"
        );
    }

    /// `Latest` + stable current must short-circuit on `info.version` without
    /// consulting `releases`. The mock body deliberately omits the `releases`
    /// map; the fast path returns `info.version` regardless. Without the fast
    /// path the empty-releases slow path would still fall back to
    /// `info.version` via `parse_and_select`'s `latest_for_stable` slot, so
    /// the assertion holds in both worlds — but the absence of a populated
    /// `releases` block keeps this test pinned to the public behavior the
    /// fast path is required to preserve.
    #[rstest]
    #[tokio::test]
    async fn resolve_version_latest_fast_path_uses_info_version(#[future] mock_server: MockServer) {
        let server = mock_server.await;
        Mock::given(method("GET"))
            .and(path("/requests/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "info": {"version": "2.31.0"}
            })))
            .mount(&server)
            .await;

        let registry = PyPiRegistry::with_base_url(&server.uri());
        let dep = make_dep("requests", ">=2.28.0");
        let result = registry
            .resolve_version(&dep, TargetLevel::Latest)
            .await
            .expect("resolve_version should succeed");
        assert_eq!(result.latest.as_deref(), Some("2.31.0"));
        assert_eq!(result.selected.as_deref(), Some("2.31.0"));
    }
}
