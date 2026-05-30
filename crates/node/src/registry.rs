//! npm registry client for looking up package versions.

use std::sync::Arc;

use reqwest::Client;
use serde::Deserialize;
use tokio::sync::Semaphore;
use tracing::{debug, trace};

use dependency_check_updates_core::{
    DEFAULT_MAX_CONCURRENT_REQUESTS, DcuError, DependencySpec, ResolvedVersion, TargetLevel,
    build_client, collect_task_results, strip_range_prefix,
};

/// npm registry client for looking up package versions.
#[derive(Clone)]
pub struct NpmRegistry {
    client: Client,
    semaphore: Arc<Semaphore>,
    base_url: Arc<str>,
}

/// Abbreviated npm package metadata response.
#[derive(Debug, Deserialize)]
struct NpmPackageInfo {
    #[serde(rename = "dist-tags")]
    dist_tags: Option<DistTags>,
    versions: Option<serde_json::Map<String, serde_json::Value>>,
    /// Map of version → ISO-8601 publish time. Only present in the *full*
    /// packument (the abbreviated `install-v1` format omits it), so it is
    /// fetched on demand for `--target newest`.
    time: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
struct DistTags {
    latest: Option<String>,
}

impl NpmRegistry {
    /// Create a new npm registry client with the default registry URL.
    #[must_use]
    pub fn new() -> Self {
        Self::with_base_url("https://registry.npmjs.org")
    }

    /// Create a client with a custom base URL (useful for testing or private registries).
    ///
    /// # Panics
    ///
    /// Panics if the HTTP client cannot be built (should never happen with default settings).
    #[must_use]
    pub fn with_base_url(base_url: &str) -> Self {
        Self {
            client: build_client(),
            semaphore: Arc::new(Semaphore::new(DEFAULT_MAX_CONCURRENT_REQUESTS)),
            base_url: Arc::from(base_url.trim_end_matches('/')),
        }
    }

    /// Encode a package name for the registry URL.
    ///
    /// Scoped packages like `@scope/name` need the `/` encoded as `%2F`.
    #[must_use]
    pub fn encode_package_name(name: &str) -> String {
        if name.starts_with('@') {
            name.replacen('/', "%2F", 1)
        } else {
            name.to_owned()
        }
    }

    /// Fetch package info from the npm registry.
    ///
    /// When `full` is true the *full* packument is requested
    /// (`Accept: application/json`) so the `time` map is present — needed for
    /// `--target newest`. Otherwise the cheaper abbreviated `install-v1`
    /// format is used, which omits `time`.
    async fn fetch_package_info(&self, name: &str, full: bool) -> Result<NpmPackageInfo, DcuError> {
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|e| DcuError::RegistryLookup {
                package: name.to_owned(),
                detail: format!("semaphore error: {e}"),
            })?;

        let encoded = Self::encode_package_name(name);
        let url = format!("{}/{encoded}", self.base_url);

        debug!(package = name, %url, full, "fetching package info");

        let accept = if full {
            "application/json"
        } else {
            "application/vnd.npm.install-v1+json; q=1.0, application/json; q=0.8, */*"
        };

        let response = self
            .client
            .get(&url)
            .header("Accept", accept)
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

    /// Resolve the target version for a single dependency.
    ///
    /// # Errors
    ///
    /// Returns an error if the registry lookup fails.
    pub async fn resolve_version(
        &self,
        dep: &DependencySpec,
        target: TargetLevel,
    ) -> Result<ResolvedVersion, DcuError> {
        // `newest` needs publish timestamps, which only the full packument
        // carries; every other target uses the cheaper abbreviated format.
        let info = self
            .fetch_package_info(&dep.name, target == TargetLevel::Newest)
            .await?;

        let latest = info.dist_tags.as_ref().and_then(|dt| dt.latest.clone());

        // Detect if the user's current requirement is a prerelease. When it is,
        // we cannot use the dist-tags.latest fast path because the user may be
        // ahead of the stable `latest` tag (e.g. `2.0.0-rc.37` while
        // dist-tags.latest points at `1.1.20`), and we must consider the full
        // sorted version list to preserve the "prerelease tail" policy.
        let current_is_prerelease =
            parse_base_version(&dep.current_req).is_some_and(|v| !v.pre_release.is_empty());

        // Fast path: Latest + current is stable → return dist-tags.latest directly.
        let selected = if target == TargetLevel::Latest && !current_is_prerelease {
            trace!(
                package = %dep.name,
                latest = ?latest,
                "fast path: using dist-tags.latest directly"
            );
            latest.clone()
        } else {
            let all_versions = extract_sorted_versions(&info);
            trace!(
                package = %dep.name,
                version_count = all_versions.len(),
                latest = ?latest,
                current_is_prerelease,
                "fetched version list"
            );
            if target == TargetLevel::Newest {
                // Most recently published by date (from the `time` map), which
                // can differ from the highest version number.
                newest_by_date(&info, &all_versions)
                    .or_else(|| all_versions.last().map(ToString::to_string))
            } else {
                select_version(&dep.current_req, latest.as_ref(), &all_versions, target)
            }
        };

        // NOTE: we do NOT filter out versions that satisfy the current range.
        // ncu-style behavior updates the manifest spec itself (e.g., ^18.0.0 → ^18.2.0)
        // even though ^18.0.0 semver-covers 18.2.0. The bare-version comparison in
        // `compute_updates` is the single source of truth for "already up to date".

        debug!(
            package = %dep.name,
            current = %dep.current_req,
            selected = ?selected,
            target = %target,
            "resolved version"
        );

        Ok(ResolvedVersion { latest, selected })
    }

    /// Resolve versions for a batch of dependencies concurrently.
    ///
    /// Returns `(index, result)` pairs preserving the original ordering.
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

impl Default for NpmRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Pick the most-recently-published version using the packument `time` map.
///
/// `time` maps each version string to an ISO-8601 timestamp (plus the meta
/// keys `created`/`modified`, which never match a parsed version and are thus
/// ignored). ISO-8601 sorts lexicographically in chronological order. Returns
/// `None` when `time` is absent (abbreviated packument) so the caller can fall
/// back to the highest version.
fn newest_by_date(info: &NpmPackageInfo, all_versions: &[node_semver::Version]) -> Option<String> {
    let times = info.time.as_ref()?;
    all_versions
        .iter()
        .filter_map(|v| {
            let s = v.to_string();
            times
                .get(&s)
                .and_then(serde_json::Value::as_str)
                .map(|t| (t.to_owned(), s))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, s)| s)
}

/// Extract and sort all version strings from the packument (pre-releases
/// included — filtering happens later in version selection).
fn extract_sorted_versions(info: &NpmPackageInfo) -> Vec<node_semver::Version> {
    let Some(versions) = &info.versions else {
        return Vec::new();
    };

    let mut parsed: Vec<node_semver::Version> = versions
        .keys()
        .filter_map(|v| node_semver::Version::parse(v).ok())
        .collect();

    parsed.sort_unstable();
    parsed
}

/// Select the appropriate version based on target level.
///
/// Thin wrapper over [`dependency_check_updates_core::select_version`]: parses
/// the current requirement and supplies npm's fallbacks (the dist-tags latest
/// for both the stable-`Latest` and unparseable-`Minor`/`Patch` cases).
fn select_version(
    current_req_str: &str,
    latest: Option<&String>,
    all_versions: &[node_semver::Version],
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

/// Parse a base version from a requirement string.
///
/// Strips leading range operators: `^1.2.3` -> `1.2.3`, `~2.0.0` -> `2.0.0`,
/// `>=1.0.0` -> `1.0.0`.
fn parse_base_version(req_str: &str) -> Option<node_semver::Version> {
    node_semver::Version::parse(strip_range_prefix(req_str)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_package_name_regular() {
        assert_eq!(NpmRegistry::encode_package_name("react"), "react");
    }

    #[test]
    fn test_encode_package_name_scoped() {
        assert_eq!(
            NpmRegistry::encode_package_name("@types/react"),
            "@types%2Freact"
        );
    }

    #[test]
    fn test_encode_package_name_scoped_babel() {
        assert_eq!(
            NpmRegistry::encode_package_name("@babel/core"),
            "@babel%2Fcore"
        );
    }

    #[test]
    fn test_parse_base_version_caret() {
        let v = parse_base_version("^1.2.3").unwrap();
        assert_eq!(v.major, 1);
        assert_eq!(v.minor, 2);
        assert_eq!(v.patch, 3);
    }

    #[test]
    fn test_parse_base_version_tilde() {
        let v = parse_base_version("~1.2.3").unwrap();
        assert_eq!(v.major, 1);
        assert_eq!(v.minor, 2);
        assert_eq!(v.patch, 3);
    }

    #[test]
    fn test_parse_base_version_gte() {
        let v = parse_base_version(">=1.0.0").unwrap();
        assert_eq!(v.major, 1);
        assert_eq!(v.minor, 0);
        assert_eq!(v.patch, 0);
    }

    #[test]
    fn test_parse_base_version_bare() {
        let v = parse_base_version("1.2.3").unwrap();
        assert_eq!(v.major, 1);
        assert_eq!(v.minor, 2);
        assert_eq!(v.patch, 3);
    }

    #[test]
    fn test_parse_base_version_star() {
        // `*` has no digits to parse
        assert!(parse_base_version("*").is_none());
    }

    fn make_versions(vers: &[&str]) -> Vec<node_semver::Version> {
        let mut v: Vec<_> = vers
            .iter()
            .filter_map(|s| node_semver::Version::parse(s).ok())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn test_select_version_latest() {
        let latest = "18.2.0".to_owned();
        let versions = make_versions(&["18.0.0", "18.1.0", "18.2.0"]);
        let result = select_version("^18.0.0", Some(&latest), &versions, TargetLevel::Latest);
        assert_eq!(result, Some("18.2.0".to_owned()));
    }

    #[test]
    fn test_select_version_minor() {
        let latest = "19.0.0".to_owned();
        let versions = make_versions(&["18.0.0", "18.1.0", "18.2.0", "19.0.0"]);
        let result = select_version("^18.0.0", Some(&latest), &versions, TargetLevel::Minor);
        assert_eq!(result, Some("18.2.0".to_owned()));
    }

    #[test]
    fn test_select_version_patch() {
        let latest = "18.2.0".to_owned();
        let versions = make_versions(&["18.0.0", "18.0.1", "18.0.2", "18.1.0", "18.2.0"]);
        let result = select_version("^18.0.0", Some(&latest), &versions, TargetLevel::Patch);
        assert_eq!(result, Some("18.0.2".to_owned()));
    }

    #[test]
    fn test_select_version_greatest() {
        let latest = "18.2.0".to_owned();
        let versions = make_versions(&["17.0.0", "18.0.0", "18.2.0", "19.0.0"]);
        let result = select_version("^17.0.0", Some(&latest), &versions, TargetLevel::Greatest);
        assert_eq!(result, Some("19.0.0".to_owned()));
    }

    #[test]
    fn test_select_version_greatest_includes_prerelease() {
        // Greatest: README says "Highest version number, INCLUDING prereleases".
        // Previously this filtered prereleases out (bug). Now it must include them.
        let latest = "18.2.0".to_owned();
        let versions = make_versions(&["18.0.0", "18.2.0", "19.0.0-beta.1"]);
        let result = select_version("^18.0.0", Some(&latest), &versions, TargetLevel::Greatest);
        assert_eq!(result, Some("19.0.0-beta.1".to_owned()));
    }

    #[test]
    fn test_select_newest_includes_prerelease() {
        // Newest is MVP-aliased to Greatest. Must include prereleases.
        let latest = "18.2.0".to_owned();
        let versions = make_versions(&["18.0.0", "18.2.0", "19.0.0-beta.1"]);
        let result = select_version("^18.0.0", Some(&latest), &versions, TargetLevel::Newest);
        assert_eq!(result, Some("19.0.0-beta.1".to_owned()));
    }

    #[test]
    fn test_latest_stable_current_excludes_prerelease() {
        // When current is stable, Latest returns dist-tags.latest (which is
        // stable by npm convention). Prereleases in the version list are
        // irrelevant here.
        let latest = "18.2.0".to_owned();
        let versions = make_versions(&["18.0.0", "18.2.0", "19.0.0-beta.1"]);
        let result = select_version("^18.0.0", Some(&latest), &versions, TargetLevel::Latest);
        assert_eq!(result, Some("18.2.0".to_owned()));
    }

    #[test]
    fn test_latest_prerelease_tail_same_train() {
        // Current on prerelease: Latest allows higher prereleases of the
        // same major.minor.patch train, bypassing dist-tags.latest.
        let latest = "3.5.0".to_owned(); // dist-tags.latest is older stable
        let versions = make_versions(&["3.5.0", "4.0.0-beta.1", "4.0.0-beta.3"]);
        let result = select_version(
            "4.0.0-beta.1",
            Some(&latest),
            &versions,
            TargetLevel::Latest,
        );
        assert_eq!(result, Some("4.0.0-beta.3".to_owned()));
    }

    #[test]
    fn test_latest_prerelease_tail_jumps_to_stable() {
        // Current prerelease → stable of same train is preferred.
        let latest = "3.5.0".to_owned();
        let versions = make_versions(&["3.5.0", "4.0.0-beta.3", "4.0.0"]);
        let result = select_version(
            "4.0.0-beta.1",
            Some(&latest),
            &versions,
            TargetLevel::Latest,
        );
        assert_eq!(result, Some("4.0.0".to_owned()));
    }

    #[test]
    fn test_latest_prerelease_tail_ignores_unrelated_prereleases() {
        // Current: 4.0.0-beta.1. Unrelated 5.0.0-alpha.1 must NOT be selected.
        let latest = "3.5.0".to_owned();
        let versions = make_versions(&["3.5.0", "4.0.0-beta.1", "5.0.0-alpha.1"]);
        let result = select_version(
            "4.0.0-beta.1",
            Some(&latest),
            &versions,
            TargetLevel::Latest,
        );
        assert_ne!(result, Some("5.0.0-alpha.1".to_owned()));
    }

    #[test]
    fn test_latest_prerelease_current_picks_higher_stable_major() {
        // Current: 4.0.0-beta.1. Registry has higher-major stable 5.0.0.
        // Expected: 5.0.0 — stable is ALWAYS preferred over staying on a
        // prerelease, regardless of "train" membership.
        let latest = "5.0.0".to_owned();
        let versions = make_versions(&["3.5.0", "4.0.0-beta.1", "5.0.0"]);
        let result = select_version(
            "4.0.0-beta.1",
            Some(&latest),
            &versions,
            TargetLevel::Latest,
        );
        assert_eq!(result, Some("5.0.0".to_owned()));
    }

    #[test]
    fn test_latest_prerelease_current_stable_wins_over_unrelated_prerelease() {
        // 5.0.0-alpha.1 must be skipped (unrelated prerelease), but 5.0.0
        // stable MUST be picked.
        let latest = "5.0.0".to_owned();
        let versions = make_versions(&["3.5.0", "4.0.0-beta.1", "5.0.0-alpha.1", "5.0.0"]);
        let result = select_version(
            "4.0.0-beta.1",
            Some(&latest),
            &versions,
            TargetLevel::Latest,
        );
        assert_eq!(result, Some("5.0.0".to_owned()));
    }

    #[test]
    fn test_select_version_empty_versions_falls_back_to_latest() {
        let latest = "1.0.0".to_owned();
        let versions: Vec<node_semver::Version> = vec![];
        let result = select_version("^1.0.0", Some(&latest), &versions, TargetLevel::Greatest);
        assert_eq!(result, Some("1.0.0".to_owned()));
    }

    #[test]
    fn test_extract_sorted_versions() {
        let info = NpmPackageInfo {
            dist_tags: None,
            versions: Some({
                let mut map = serde_json::Map::new();
                map.insert(
                    "2.0.0".to_owned(),
                    serde_json::Value::Object(serde_json::Map::new()),
                );
                map.insert(
                    "1.0.0".to_owned(),
                    serde_json::Value::Object(serde_json::Map::new()),
                );
                map.insert(
                    "1.5.0".to_owned(),
                    serde_json::Value::Object(serde_json::Map::new()),
                );
                map
            }),
            time: None,
        };
        let versions = extract_sorted_versions(&info);
        assert_eq!(versions.len(), 3);
        assert_eq!(versions[0].to_string(), "1.0.0");
        assert_eq!(versions[1].to_string(), "1.5.0");
        assert_eq!(versions[2].to_string(), "2.0.0");
    }

    #[test]
    fn test_extract_sorted_versions_none() {
        let info = NpmPackageInfo {
            dist_tags: None,
            versions: None,
            time: None,
        };
        let versions = extract_sorted_versions(&info);
        assert!(versions.is_empty());
    }

    #[test]
    fn test_select_version_minor_no_match() {
        // Current is major 5, no version with major 5 exists
        let latest = "6.0.0".to_owned();
        let versions = make_versions(&["6.0.0", "7.0.0"]);
        let result = select_version("^5.0.0", Some(&latest), &versions, TargetLevel::Minor);
        // No major=5 version found, so None
        assert_eq!(result, None);
    }

    /// Install the rustls ring provider once per process so reqwest (rustls-no-provider) works.
    fn install_crypto_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    fn npm_response_body(latest: &str, versions: &[&str]) -> serde_json::Value {
        let mut vers_map = serde_json::Map::new();
        for v in versions {
            vers_map.insert(
                (*v).to_owned(),
                serde_json::Value::Object(serde_json::Map::new()),
            );
        }
        serde_json::json!({
            "dist-tags": { "latest": latest },
            "versions": vers_map
        })
    }

    #[tokio::test]
    async fn test_resolve_version_latest() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        install_crypto_provider();
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/react"))
            .respond_with(ResponseTemplate::new(200).set_body_json(npm_response_body(
                "18.2.0",
                &["17.0.0", "18.0.0", "18.2.0", "19.0.0-beta.1"],
            )))
            .mount(&mock_server)
            .await;

        let registry = NpmRegistry::with_base_url(&mock_server.uri());
        let dep = DependencySpec {
            name: "react".to_owned(),
            current_req: "^17.0.0".to_owned(),
            section: dependency_check_updates_core::DependencySection::Dependencies,
        };
        let result = registry
            .resolve_version(&dep, TargetLevel::Latest)
            .await
            .unwrap();
        assert_eq!(result.latest.as_deref(), Some("18.2.0"));
        // ^17.0.0 does not satisfy 18.2.0, so selected should be Some
        assert_eq!(result.selected.as_deref(), Some("18.2.0"));
    }

    #[tokio::test]
    async fn test_resolve_version_minor() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        install_crypto_provider();
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/react"))
            .respond_with(ResponseTemplate::new(200).set_body_json(npm_response_body(
                "19.0.0",
                &["17.0.0", "17.1.0", "17.2.0", "18.0.0", "18.2.0", "19.0.0"],
            )))
            .mount(&mock_server)
            .await;

        let registry = NpmRegistry::with_base_url(&mock_server.uri());
        let dep = DependencySpec {
            name: "react".to_owned(),
            // ~17.0.0 means >=17.0.0 <17.1.0, so 17.2.0 won't satisfy → update needed
            current_req: "~17.0.0".to_owned(),
            section: dependency_check_updates_core::DependencySection::Dependencies,
        };
        let result = registry
            .resolve_version(&dep, TargetLevel::Minor)
            .await
            .unwrap();
        // Minor stays on same major (17), picks highest: 17.2.0; outside ~17.0.0 range
        assert_eq!(result.selected.as_deref(), Some("17.2.0"));
    }

    #[tokio::test]
    async fn test_resolve_version_patch() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        install_crypto_provider();
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/react"))
            .respond_with(ResponseTemplate::new(200).set_body_json(npm_response_body(
                "18.2.0",
                &["17.0.0", "17.0.1", "17.0.2", "17.1.0", "18.0.0", "18.2.0"],
            )))
            .mount(&mock_server)
            .await;

        let registry = NpmRegistry::with_base_url(&mock_server.uri());
        let dep = DependencySpec {
            name: "react".to_owned(),
            // =17.0.0 is exact pin, so 17.0.2 won't satisfy → update needed
            current_req: "=17.0.0".to_owned(),
            section: dependency_check_updates_core::DependencySection::Dependencies,
        };
        let result = registry
            .resolve_version(&dep, TargetLevel::Patch)
            .await
            .unwrap();
        // Patch stays on same major.minor (17.0), picks highest patch: 17.0.2
        assert_eq!(result.selected.as_deref(), Some("17.0.2"));
    }

    #[tokio::test]
    async fn test_resolve_version_already_satisfied() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        install_crypto_provider();
        let mock_server = MockServer::start().await;
        // latest is 17.0.1 which satisfies ^17.0.0
        Mock::given(method("GET"))
            .and(path("/react"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(npm_response_body("17.0.1", &["17.0.0", "17.0.1"])),
            )
            .mount(&mock_server)
            .await;

        let registry = NpmRegistry::with_base_url(&mock_server.uri());
        let dep = DependencySpec {
            name: "react".to_owned(),
            current_req: "^17.0.0".to_owned(),
            section: dependency_check_updates_core::DependencySection::Dependencies,
        };
        let result = registry
            .resolve_version(&dep, TargetLevel::Latest)
            .await
            .unwrap();
        // ncu-style: always reports latest so CLI can bump the spec (^17.0.0 → ^17.0.1).
        // Whether an actual manifest update is needed is decided later by compute_updates.
        assert_eq!(result.selected.as_deref(), Some("17.0.1"));
    }

    #[tokio::test]
    async fn test_resolve_version_404() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        install_crypto_provider();
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/nonexistent-pkg"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        let registry = NpmRegistry::with_base_url(&mock_server.uri());
        let dep = DependencySpec {
            name: "nonexistent-pkg".to_owned(),
            current_req: "^1.0.0".to_owned(),
            section: dependency_check_updates_core::DependencySection::Dependencies,
        };
        let result = registry.resolve_version(&dep, TargetLevel::Latest).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        let err_str = err.to_string();
        assert!(
            err_str.contains("404") || err_str.contains("nonexistent-pkg"),
            "unexpected error: {err_str}"
        );
    }

    #[tokio::test]
    async fn test_resolve_batch_concurrent() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        install_crypto_provider();
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/react"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(npm_response_body("18.2.0", &["17.0.0", "18.0.0", "18.2.0"])),
            )
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/lodash"))
            .respond_with(ResponseTemplate::new(200).set_body_json(npm_response_body(
                "4.17.21",
                &["4.0.0", "4.17.0", "4.17.21"],
            )))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/axios"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(npm_response_body("1.6.0", &["0.27.0", "1.0.0", "1.6.0"])),
            )
            .mount(&mock_server)
            .await;

        let registry = NpmRegistry::with_base_url(&mock_server.uri());
        let deps = vec![
            DependencySpec {
                name: "react".to_owned(),
                current_req: "^17.0.0".to_owned(),
                section: dependency_check_updates_core::DependencySection::Dependencies,
            },
            DependencySpec {
                name: "lodash".to_owned(),
                current_req: "^4.0.0".to_owned(),
                section: dependency_check_updates_core::DependencySection::Dependencies,
            },
            DependencySpec {
                name: "axios".to_owned(),
                current_req: "^0.27.0".to_owned(),
                section: dependency_check_updates_core::DependencySection::Dependencies,
            },
        ];

        let results = registry.resolve_batch(&deps, TargetLevel::Latest).await;
        assert_eq!(results.len(), 3);

        // Results are ordered by original index
        let (idx0, ref res0) = results[0];
        let (idx1, ref res1) = results[1];
        let (idx2, ref res2) = results[2];
        assert_eq!(idx0, 0);
        assert_eq!(idx1, 1);
        assert_eq!(idx2, 2);

        assert_eq!(res0.as_ref().unwrap().latest.as_deref(), Some("18.2.0"));
        assert_eq!(res1.as_ref().unwrap().latest.as_deref(), Some("4.17.21"));
        assert_eq!(res2.as_ref().unwrap().latest.as_deref(), Some("1.6.0"));
    }

    #[tokio::test]
    async fn test_resolve_version_scoped_package() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        install_crypto_provider();
        let mock_server = MockServer::start().await;
        // Scoped package @types/react -> URL path /@types%2Freact
        Mock::given(method("GET"))
            .and(path("/@types%2Freact"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(npm_response_body("18.2.0", &["17.0.0", "18.0.0", "18.2.0"])),
            )
            .mount(&mock_server)
            .await;

        let registry = NpmRegistry::with_base_url(&mock_server.uri());
        let dep = DependencySpec {
            name: "@types/react".to_owned(),
            current_req: "^17.0.0".to_owned(),
            section: dependency_check_updates_core::DependencySection::Dependencies,
        };
        let result = registry
            .resolve_version(&dep, TargetLevel::Latest)
            .await
            .unwrap();
        assert_eq!(result.latest.as_deref(), Some("18.2.0"));
        assert_eq!(result.selected.as_deref(), Some("18.2.0"));
    }

    #[tokio::test]
    async fn test_resolve_version_latest_fast_path() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        install_crypto_provider();
        let mock_server = MockServer::start().await;
        // Provide versions that include pre-releases; Latest fast path should return
        // dist-tags.latest directly without filtering pre-releases from version list.
        Mock::given(method("GET"))
            .and(path("/react"))
            .respond_with(ResponseTemplate::new(200).set_body_json(npm_response_body(
                "18.2.0",
                &["18.0.0", "18.2.0", "19.0.0-beta.1", "19.0.0-rc.1"],
            )))
            .mount(&mock_server)
            .await;

        let registry = NpmRegistry::with_base_url(&mock_server.uri());
        let dep = DependencySpec {
            name: "react".to_owned(),
            current_req: "^17.0.0".to_owned(),
            section: dependency_check_updates_core::DependencySection::Dependencies,
        };
        let result = registry
            .resolve_version(&dep, TargetLevel::Latest)
            .await
            .unwrap();
        // Fast path: should return exactly dist-tags.latest, not the highest version
        assert_eq!(result.latest.as_deref(), Some("18.2.0"));
        assert_eq!(result.selected.as_deref(), Some("18.2.0"));
        // Confirm it did NOT pick 19.0.0-rc.1 (which would happen if fast path was skipped)
        assert_ne!(result.selected.as_deref(), Some("19.0.0-rc.1"));
    }

    #[tokio::test]
    async fn test_resolve_version_newest_by_date() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        install_crypto_provider();
        let mock_server = MockServer::start().await;
        // 1.5.0 was published most recently even though 2.0.0 is higher. The
        // `time` map (full packument) drives `newest`; `greatest` would differ.
        Mock::given(method("GET"))
            .and(path("/pkg"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "dist-tags": { "latest": "2.0.0" },
                "versions": { "1.0.0": {}, "1.5.0": {}, "2.0.0": {} },
                "time": {
                    "created": "2021-01-01T00:00:00Z",
                    "1.0.0": "2022-01-01T00:00:00Z",
                    "2.0.0": "2023-01-01T00:00:00Z",
                    "1.5.0": "2024-06-01T00:00:00Z"
                }
            })))
            .mount(&mock_server)
            .await;

        let registry = NpmRegistry::with_base_url(&mock_server.uri());
        let dep = DependencySpec {
            name: "pkg".to_owned(),
            current_req: "^1.0.0".to_owned(),
            section: dependency_check_updates_core::DependencySection::Dependencies,
        };
        let result = registry
            .resolve_version(&dep, TargetLevel::Newest)
            .await
            .unwrap();
        assert_eq!(result.selected.as_deref(), Some("1.5.0"));
    }

    #[test]
    fn test_new_creates_registry() {
        install_crypto_provider();
        let _registry = NpmRegistry::new();
        // Just verifying it doesn't panic
    }

    #[test]
    fn test_default_creates_registry() {
        install_crypto_provider();
        let _registry = NpmRegistry::default();
    }

    #[test]
    fn test_select_version_minor_unparseable_falls_back_to_latest() {
        let latest = "2.0.0".to_owned();
        let versions = make_versions(&["1.0.0", "2.0.0"]);
        let result = select_version("*", Some(&latest), &versions, TargetLevel::Minor);
        assert_eq!(result, Some("2.0.0".to_owned()));
    }

    #[test]
    fn test_select_version_patch_unparseable_falls_back_to_latest() {
        let latest = "2.0.0".to_owned();
        let versions = make_versions(&["1.0.0", "2.0.0"]);
        let result = select_version("*", Some(&latest), &versions, TargetLevel::Patch);
        assert_eq!(result, Some("2.0.0".to_owned()));
    }

    #[test]
    fn test_select_version_minor_skips_prerelease_when_current_stable() {
        // Exercises accept_pre_aware: stable current + prerelease candidate in same
        // major → prerelease must be rejected (covers the `!current_is_prerelease`
        // early-return branch inside accept_pre_aware).
        let latest = "18.2.0".to_owned();
        let versions = make_versions(&["18.0.0", "18.1.0", "18.2.0", "18.3.0-beta.1"]);
        let result = select_version("^18.0.0", Some(&latest), &versions, TargetLevel::Minor);
        // Stable 18.2.0 wins over 18.3.0-beta.1 because current is stable.
        assert_eq!(result, Some("18.2.0".to_owned()));
    }

    #[test]
    fn test_select_version_patch_skips_prerelease_when_current_stable() {
        // Same as above, but on the Patch branch, so the iterator walks a
        // prerelease candidate on the same major.minor and must reject it.
        let latest = "18.0.5".to_owned();
        let versions = make_versions(&["18.0.0", "18.0.1", "18.0.5", "18.0.6-rc.1"]);
        let result = select_version("=18.0.0", Some(&latest), &versions, TargetLevel::Patch);
        assert_eq!(result, Some("18.0.5".to_owned()));
    }

    #[tokio::test]
    async fn test_resolve_version_non_latest_with_tracing() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        install_crypto_provider();

        // Install a trace-level subscriber so trace!() arguments are evaluated.
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_test_writer()
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/react"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(npm_response_body("19.0.0", &["17.0.0", "18.0.0", "19.0.0"])),
            )
            .mount(&mock_server)
            .await;

        let registry = NpmRegistry::with_base_url(&mock_server.uri());
        let dep = DependencySpec {
            name: "react".to_owned(),
            current_req: "~17.0.0".to_owned(),
            section: dependency_check_updates_core::DependencySection::Dependencies,
        };
        let result = registry
            .resolve_version(&dep, TargetLevel::Greatest)
            .await
            .unwrap();
        assert_eq!(result.selected.as_deref(), Some("19.0.0"));
    }

}
