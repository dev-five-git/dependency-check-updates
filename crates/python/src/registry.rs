//! `PyPI` registry client for looking up Python package versions.

use std::str::FromStr;
use std::sync::Arc;

use reqwest::Client;
use serde::Deserialize;
use tokio::sync::Semaphore;
use tracing::debug;

use dependency_check_updates_core::{
    DEFAULT_MAX_CONCURRENT_REQUESTS, DcuError, DependencySpec, ResolvedVersion, TargetLevel,
    build_client, collect_task_results, select_version, strip_range_prefix,
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
        let latest = Some(info.info.version.clone());

        // (parsed PEP 440 version, max upload timestamp) for every release that
        // has at least one non-yanked file and parses cleanly.
        let mut candidates: Vec<(pep440_rs::Version, String)> = info
            .releases
            .iter()
            .filter_map(|(ver_str, files)| {
                if files.is_empty() || files.iter().all(|f| f.yanked) {
                    return None;
                }
                let parsed = pep440_rs::Version::from_str(ver_str).ok()?;
                let upload = files
                    .iter()
                    .map(|f| f.upload_time_iso_8601.clone())
                    .max()
                    .unwrap_or_default();
                Some((parsed, upload))
            })
            .collect();
        candidates.sort_by(|a, b| a.0.cmp(&b.0));

        let versions: Vec<pep440_rs::Version> =
            candidates.iter().map(|(v, _)| v.clone()).collect();

        let selected = if target == TargetLevel::Newest {
            // Most recently uploaded by date (ISO-8601 sorts chronologically),
            // which can differ from the highest version number.
            candidates
                .iter()
                .max_by(|a, b| a.1.cmp(&b.1))
                .map(|(v, _)| v.to_string())
                .or_else(|| versions.last().map(ToString::to_string))
        } else {
            let current = pep440_rs::Version::from_str(strip_range_prefix(&dep.current_req)).ok();
            select_version(current.as_ref(), &versions, target, latest.clone(), latest.clone())
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

    #[tokio::test]
    async fn test_resolve_version_patch_target() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        install_crypto_provider();

        let mock_server = MockServer::start().await;
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
            .mount(&mock_server)
            .await;

        let registry = PyPiRegistry::with_base_url(&mock_server.uri());
        let dep = dependency_check_updates_core::DependencySpec {
            name: "flask".to_owned(),
            current_req: ">=2.0.0".to_owned(),
            section: dependency_check_updates_core::DependencySection::ProjectDependencies,
        };
        // Patch: stay on 2.0.x → highest is 2.0.5 (no longer ignores target!).
        let patch = registry
            .resolve_version(&dep, dependency_check_updates_core::TargetLevel::Patch)
            .await
            .unwrap();
        assert_eq!(patch.selected.as_deref(), Some("2.0.5"));
        // Minor: stay on 2.x → highest is 2.1.0.
        let minor = registry
            .resolve_version(&dep, dependency_check_updates_core::TargetLevel::Minor)
            .await
            .unwrap();
        assert_eq!(minor.selected.as_deref(), Some("2.1.0"));
    }

    #[tokio::test]
    async fn test_resolve_version_newest_by_date() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        install_crypto_provider();

        let mock_server = MockServer::start().await;
        // 1.5.0 uploaded most recently though 2.0.0 is higher.
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
            .mount(&mock_server)
            .await;

        let registry = PyPiRegistry::with_base_url(&mock_server.uri());
        let dep = dependency_check_updates_core::DependencySpec {
            name: "requests".to_owned(),
            current_req: ">=1.0.0".to_owned(),
            section: dependency_check_updates_core::DependencySection::ProjectDependencies,
        };
        let newest = registry
            .resolve_version(&dep, dependency_check_updates_core::TargetLevel::Newest)
            .await
            .unwrap();
        assert_eq!(newest.selected.as_deref(), Some("1.5.0"));
        let greatest = registry
            .resolve_version(&dep, dependency_check_updates_core::TargetLevel::Greatest)
            .await
            .unwrap();
        assert_eq!(greatest.selected.as_deref(), Some("2.0.0"));
    }

    #[test]
    fn test_new_creates_registry() {
        install_crypto_provider();
        let _registry = PyPiRegistry::new();
    }

    #[test]
    fn test_default_creates_registry() {
        install_crypto_provider();
        let _registry = PyPiRegistry::default();
    }

}
