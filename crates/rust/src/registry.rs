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
pub struct CratesIoRegistry {
    client: Client,
    semaphore: Arc<Semaphore>,
    base_url: String,
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
            base_url: base_url.trim_end_matches('/').to_owned(),
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
        versions.sort();

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
            let client = self.client.clone();
            let semaphore = self.semaphore.clone();
            let base_url = self.base_url.clone();

            let handle = tokio::spawn(async move {
                let registry = CratesIoRegistry {
                    client,
                    semaphore,
                    base_url,
                };
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

        results.sort_by_key(|(idx, _)| *idx);
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
}
