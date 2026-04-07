//! `PyPI` registry client for looking up Python package versions.

use std::sync::Arc;

use reqwest::Client;
use serde::Deserialize;
use tokio::sync::Semaphore;
use tracing::{debug, warn};

use dependency_check_updates_core::{DcuError, DependencySpec, ResolvedVersion, TargetLevel};

const MAX_CONCURRENT_REQUESTS: usize = 10;
const REQUEST_TIMEOUT_SECS: u64 = 30;

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
}

#[derive(Debug, Deserialize)]
struct PyPiInfo {
    version: String,
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

    /// Fetch package info from `PyPI`.
    async fn fetch_package_info(&self, name: &str) -> Result<PyPiResponse, DcuError> {
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|e| DcuError::RegistryLookup {
                package: name.to_owned(),
                detail: format!("semaphore error: {e}"),
            })?;

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
    /// # Errors
    ///
    /// Returns an error if the registry lookup fails.
    pub async fn resolve_version(
        &self,
        dep: &DependencySpec,
        _target: TargetLevel,
    ) -> Result<ResolvedVersion, DcuError> {
        let info = self.fetch_package_info(&dep.name).await?;

        let latest = Some(info.info.version.clone());

        debug!(
            package = %dep.name,
            current = %dep.current_req,
            latest = ?latest,
            "resolved PyPI version"
        );

        // For Python, version resolution is simpler for MVP:
        // Just report the latest version. PEP 440 version ordering
        // is complex; for MVP we use the PyPI-reported latest.
        let selected = latest.clone();

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

impl Default for PyPiRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::PyPiRegistry;

    /// Install the rustls ring provider once per process so reqwest (rustls-no-provider) works.
    fn install_crypto_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    #[test]
    fn test_normalized_name() {
        // PyPI normalizes names: underscores -> hyphens, lowercase
        let name = "My_Package";
        let normalized = name.to_lowercase().replace('_', "-");
        assert_eq!(normalized, "my-package");
    }

    #[tokio::test]
    async fn test_resolve_version_latest() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        install_crypto_provider();

        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/requests/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "info": {"version": "2.31.0"}
            })))
            .mount(&mock_server)
            .await;

        let registry = PyPiRegistry::with_base_url(&mock_server.uri());
        let dep = dependency_check_updates_core::DependencySpec {
            name: "requests".to_owned(),
            current_req: ">=2.28.0".to_owned(),
            section: dependency_check_updates_core::DependencySection::ProjectDependencies,
        };

        let result = registry
            .resolve_version(&dep, dependency_check_updates_core::TargetLevel::Latest)
            .await
            .expect("resolve_version should succeed");

        assert_eq!(result.selected, Some("2.31.0".to_owned()));
    }

    #[tokio::test]
    async fn test_resolve_version_404() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        install_crypto_provider();

        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/nonexistent-package/json"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        let registry = PyPiRegistry::with_base_url(&mock_server.uri());
        let dep = dependency_check_updates_core::DependencySpec {
            name: "nonexistent-package".to_owned(),
            current_req: ">=1.0.0".to_owned(),
            section: dependency_check_updates_core::DependencySection::ProjectDependencies,
        };

        let result = registry
            .resolve_version(&dep, dependency_check_updates_core::TargetLevel::Latest)
            .await;

        assert!(result.is_err(), "expected error for 404 response");
    }

    #[tokio::test]
    async fn test_resolve_batch() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        install_crypto_provider();

        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/requests/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "info": {"version": "2.31.0"}
            })))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/flask/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "info": {"version": "3.0.0"}
            })))
            .mount(&mock_server)
            .await;

        let registry = PyPiRegistry::with_base_url(&mock_server.uri());
        let deps = vec![
            dependency_check_updates_core::DependencySpec {
                name: "requests".to_owned(),
                current_req: ">=2.28.0".to_owned(),
                section: dependency_check_updates_core::DependencySection::ProjectDependencies,
            },
            dependency_check_updates_core::DependencySpec {
                name: "flask".to_owned(),
                current_req: ">=2.0.0".to_owned(),
                section: dependency_check_updates_core::DependencySection::ProjectDependencies,
            },
        ];

        let results = registry
            .resolve_batch(&deps, dependency_check_updates_core::TargetLevel::Latest)
            .await;

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

    #[tokio::test]
    async fn test_resolve_version_normalized_name() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        install_crypto_provider();

        let mock_server = MockServer::start().await;
        // URL must use normalized name: My_Package -> my-package
        Mock::given(method("GET"))
            .and(path("/my-package/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "info": {"version": "1.2.3"}
            })))
            .mount(&mock_server)
            .await;

        let registry = PyPiRegistry::with_base_url(&mock_server.uri());
        let dep = dependency_check_updates_core::DependencySpec {
            name: "My_Package".to_owned(),
            current_req: ">=1.0.0".to_owned(),
            section: dependency_check_updates_core::DependencySection::ProjectDependencies,
        };

        let result = registry
            .resolve_version(&dep, dependency_check_updates_core::TargetLevel::Latest)
            .await
            .expect("resolve_version should succeed with normalized name");

        assert_eq!(result.selected, Some("1.2.3".to_owned()));
    }
}
