//! crates.io registry client for looking up Rust crate versions.

use std::sync::Arc;

use reqwest::Client;
use serde::Deserialize;
use tokio::sync::Semaphore;
use tracing::{debug, trace, warn};

use dependency_check_updates_core::{DcuError, DependencySpec, ResolvedVersion, TargetLevel};

const MAX_CONCURRENT_REQUESTS: usize = 10;
const REQUEST_TIMEOUT_SECS: u64 = 30;

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

        let selected = select_version(&dep.current_req, latest.as_ref(), &versions, target);

        // Filter: if selected satisfies the current requirement, no change needed
        let selected = selected.filter(|v| !satisfies_req(&dep.current_req, v));

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

        let mut results = Vec::with_capacity(deps.len());
        for handle in handles {
            match handle.await {
                Ok(result) => results.push(result),
                Err(e) => warn!("task join error: {e}"),
            }
        }

        results.sort_unstable_by_key(|(idx, _)| *idx);
        results
    }
}

impl Default for CratesIoRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn select_version(
    current_req_str: &str,
    latest: Option<&String>,
    all_versions: &[semver::Version],
    target: TargetLevel,
) -> Option<String> {
    if all_versions.is_empty() {
        return latest.cloned();
    }

    let current = parse_base_version(current_req_str);

    match target {
        TargetLevel::Latest | TargetLevel::Greatest | TargetLevel::Newest => all_versions
            .iter()
            .rev()
            .find(|v| v.pre.is_empty())
            .map(std::string::ToString::to_string),
        TargetLevel::Minor => {
            let Some(cur) = &current else {
                return latest.cloned();
            };
            all_versions
                .iter()
                .rev()
                .find(|v| v.major == cur.major && v.pre.is_empty())
                .map(std::string::ToString::to_string)
        }
        TargetLevel::Patch => {
            let Some(cur) = &current else {
                return latest.cloned();
            };
            all_versions
                .iter()
                .rev()
                .find(|v| v.major == cur.major && v.minor == cur.minor && v.pre.is_empty())
                .map(std::string::ToString::to_string)
        }
    }
}

/// Check if a version satisfies a Cargo version requirement.
fn satisfies_req(req_str: &str, version_str: &str) -> bool {
    let Ok(req) = semver::VersionReq::parse(req_str) else {
        return false;
    };
    let Ok(version) = semver::Version::parse(version_str) else {
        return false;
    };
    req.matches(&version)
}

fn parse_base_version(req_str: &str) -> Option<semver::Version> {
    let cleaned = req_str.trim_start_matches(|c: char| !c.is_ascii_digit());
    semver::Version::parse(cleaned).ok()
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
        // 1.5.0 satisfies ^1.0.0, so selected should be None
        assert_eq!(result.selected, None);
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
        // 1.0.0 satisfies ^1.0.0, so selected is None
        assert_eq!(result.selected, None);
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

    #[test]
    fn test_satisfies_req_invalid_req() {
        assert!(!satisfies_req("not valid!!!", "1.0.0"));
    }

    #[test]
    fn test_satisfies_req_invalid_version() {
        assert!(!satisfies_req("^1.0.0", "not.valid"));
    }
}
