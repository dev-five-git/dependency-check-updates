//! npm registry client for looking up package versions.

use std::sync::Arc;

use reqwest::Client;
use serde::Deserialize;
use tokio::sync::Semaphore;
use tracing::{debug, trace, warn};

use dependency_check_updates_core::{DcuError, DependencySpec, ResolvedVersion, TargetLevel};

/// Maximum concurrent registry requests.
const MAX_CONCURRENT_REQUESTS: usize = 10;

/// Request timeout in seconds.
const REQUEST_TIMEOUT_SECS: u64 = 30;

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
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .user_agent(concat!(
                "dependency-check-updates/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .expect("failed to create HTTP client");

        Self {
            client,
            semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS)),
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
    async fn fetch_package_info(&self, name: &str) -> Result<NpmPackageInfo, DcuError> {
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

        debug!(package = name, %url, "fetching package info");

        let response = self
            .client
            .get(&url)
            .header(
                "Accept",
                "application/vnd.npm.install-v1+json; q=1.0, application/json; q=0.8, */*",
            )
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
        let info = self.fetch_package_info(&dep.name).await?;

        let latest = info.dist_tags.as_ref().and_then(|dt| dt.latest.clone());

        // Fast path: when target is Latest, skip expensive version parsing/sorting
        let selected = if target == TargetLevel::Latest {
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
                "fetched version list"
            );
            select_version(&dep.current_req, latest.as_ref(), &all_versions, target)
        };

        // Filter out false positives: if the selected version already satisfies
        // the current range, there's no manifest change needed.
        let selected = selected.filter(|v| !satisfies_range(&dep.current_req, v));

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

/// Collect results from spawned tasks, logging any `JoinError`s (e.g. panics).
async fn collect_task_results<T>(handles: Vec<tokio::task::JoinHandle<T>>) -> Vec<T> {
    let mut results = Vec::with_capacity(handles.len());
    for handle in handles {
        match handle.await {
            Ok(result) => results.push(result),
            Err(e) => warn!("task join error: {e}"),
        }
    }
    results
}

impl Default for NpmRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract and sort all version strings from package info, excluding pre-releases.
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
fn select_version(
    current_req_str: &str,
    latest: Option<&String>,
    all_versions: &[node_semver::Version],
    target: TargetLevel,
) -> Option<String> {
    if all_versions.is_empty() {
        return latest.cloned();
    }

    let current_version = parse_base_version(current_req_str);

    match target {
        TargetLevel::Latest => latest.cloned(),
        TargetLevel::Greatest | TargetLevel::Newest => {
            // Highest non-prerelease version.
            // (Newest would ideally use publish timestamps, but for MVP same as greatest.)
            all_versions
                .iter()
                .rev()
                .find(|v| v.pre_release.is_empty())
                .map(std::string::ToString::to_string)
        }
        TargetLevel::Minor => {
            let Some(current) = &current_version else {
                return latest.cloned();
            };
            all_versions
                .iter()
                .rev()
                .find(|v| v.major == current.major && v.pre_release.is_empty())
                .map(std::string::ToString::to_string)
        }
        TargetLevel::Patch => {
            let Some(current) = &current_version else {
                return latest.cloned();
            };
            all_versions
                .iter()
                .rev()
                .find(|v| {
                    v.major == current.major && v.minor == current.minor && v.pre_release.is_empty()
                })
                .map(std::string::ToString::to_string)
        }
    }
}

/// Check if a version satisfies an npm semver range.
///
/// Returns `true` if the version is within the range (no update needed).
fn satisfies_range(range_str: &str, version_str: &str) -> bool {
    let Ok(range) = node_semver::Range::parse(range_str) else {
        return false;
    };
    let Ok(version) = node_semver::Version::parse(version_str) else {
        return false;
    };
    range.satisfies(&version)
}

/// Parse a base version from a requirement string.
///
/// Strips leading range operators: `^1.2.3` -> `1.2.3`, `~2.0.0` -> `2.0.0`,
/// `>=1.0.0` -> `1.0.0`.
fn parse_base_version(req_str: &str) -> Option<node_semver::Version> {
    let cleaned = req_str.trim_start_matches(|c: char| !c.is_ascii_digit());
    node_semver::Version::parse(cleaned).ok()
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
    fn test_select_version_skips_prerelease() {
        let latest = "18.2.0".to_owned();
        let versions = make_versions(&["18.0.0", "18.2.0", "19.0.0-beta.1"]);
        let result = select_version("^18.0.0", Some(&latest), &versions, TargetLevel::Greatest);
        assert_eq!(result, Some("18.2.0".to_owned()));
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
        // 17.0.1 satisfies ^17.0.0, so selected should be None (no update needed)
        assert_eq!(result.selected, None);
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
    fn test_satisfies_range_invalid_range() {
        assert!(!satisfies_range("not a range!!!", "1.0.0"));
    }

    #[test]
    fn test_satisfies_range_invalid_version() {
        assert!(!satisfies_range("^1.0.0", "not.a"));
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

    #[tokio::test]
    async fn test_collect_task_results_join_error() {
        // Suppress panic output from the intentionally-panicking task.
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));

        let handles: Vec<tokio::task::JoinHandle<(usize, Result<ResolvedVersion, DcuError>)>> = vec![
            tokio::spawn(async {
                (
                    0,
                    Ok(ResolvedVersion {
                        latest: Some("1.0.0".into()),
                        selected: None,
                    }),
                )
            }),
            tokio::spawn(async { panic!("simulated join error") }),
            tokio::spawn(async {
                (
                    2,
                    Ok(ResolvedVersion {
                        latest: Some("2.0.0".into()),
                        selected: None,
                    }),
                )
            }),
        ];
        let results = collect_task_results(handles).await;

        std::panic::set_hook(prev_hook);

        // The panicking task is dropped; only 2 results survive.
        assert_eq!(results.len(), 2);
    }
}
