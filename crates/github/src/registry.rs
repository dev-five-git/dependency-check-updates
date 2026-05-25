//! GitHub Tags API client for resolving action versions.
//!
//! Fetches `GET /repos/{owner}/{repo}/tags?per_page=100`, parses tag names as
//! semver (after stripping a leading `v` and padding to 3 segments), and picks
//! the highest tag honoring the configured [`TargetLevel`].
//!
//! Multiple deps that share `owner/repo` (e.g. `actions/checkout` appearing in
//! 12 different jobs) result in exactly ONE HTTP call per batch — the tag list
//! is fetched once and cached for the lifetime of a `resolve_batch` call.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::Client;
use serde::Deserialize;
use tokio::sync::Semaphore;
use tracing::{debug, trace};

use dependency_check_updates_core::{DcuError, DependencySpec, ResolvedVersion, TargetLevel};

use crate::parser::is_version_ref;

/// Cap on parallel GitHub API calls. The unauthenticated rate limit is
/// 60 req/hr; keeping concurrency modest avoids burst-rejection during deep
/// scans of multi-workflow repos.
const MAX_CONCURRENT_REQUESTS: usize = 5;

/// HTTP request timeout. GitHub API is normally <1s; 30s leaves room for
/// network hiccups without freezing a CI job for minutes.
const REQUEST_TIMEOUT_SECS: u64 = 30;

/// Tags page size — GitHub API max is 100, which comfortably covers every
/// mainstream action; spanning multiple pages is future work.
const TAGS_PER_PAGE: u32 = 100;

/// One tag entry from the GitHub API.
#[derive(Debug, Deserialize, Clone)]
struct Tag {
    name: String,
}

/// GitHub Tags API client.
#[derive(Clone)]
pub struct GitHubActionsRegistry {
    client: Client,
    semaphore: Arc<Semaphore>,
    base_url: Arc<str>,
    token: Option<Arc<str>>,
}

impl GitHubActionsRegistry {
    /// Construct with the default `https://api.github.com` base.
    #[must_use]
    pub fn new() -> Self {
        Self::with_base_url("https://api.github.com")
    }

    /// Construct against a custom base URL (used by tests via `wiremock`).
    ///
    /// Reads `GITHUB_TOKEN` (or, falling back, `GH_TOKEN`) at construction
    /// time. The token raises the rate limit from 60 → 5000 req/hr; without
    /// it, large monorepos can run out of quota in a single deep scan.
    ///
    /// # Panics
    ///
    /// Panics only if the underlying `reqwest::Client` cannot be built — that
    /// never happens with the default config used here.
    #[must_use]
    pub fn with_base_url(base_url: &str) -> Self {
        Self::build(base_url, token_from_env())
    }

    /// Construct with an explicit token, bypassing env-var lookup. Used by
    /// tests to keep behaviour deterministic regardless of the developer's
    /// shell environment (touching `GITHUB_TOKEN` would require `unsafe` in
    /// Rust 2024 because env mutation is not thread-safe).
    #[cfg(test)]
    fn with_base_url_and_token(base_url: &str, token: Option<&str>) -> Self {
        Self::build(base_url, token.map(Arc::from))
    }

    fn build(base_url: &str, token: Option<Arc<str>>) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
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
            token,
        }
    }

    /// Extract the `owner/repo` API target from an action ref name.
    ///
    /// `actions/checkout` → `Some("actions/checkout")`
    /// `actions/checkout/sub/path` → `Some("actions/checkout")` (sub-action;
    ///   tags still live on the parent repo)
    /// `not-a-valid-name` → `None`
    fn repo_key(name: &str) -> Option<String> {
        let mut parts = name.splitn(3, '/');
        let owner = parts.next()?;
        let repo = parts.next()?;
        if owner.is_empty() || repo.is_empty() {
            return None;
        }
        Some(format!("{owner}/{repo}"))
    }

    /// Fetch tags for a single repo.
    async fn fetch_tags(&self, repo_key: &str) -> Result<Vec<Tag>, String> {
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|e| format!("semaphore error: {e}"))?;

        let url = format!(
            "{}/repos/{repo_key}/tags?per_page={TAGS_PER_PAGE}",
            self.base_url
        );
        debug!(repo = repo_key, %url, "fetching tags");

        let mut req = self
            .client
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");

        if let Some(token) = &self.token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }

        let response = req.send().await.map_err(|e| e.to_string())?;

        if !response.status().is_success() {
            let status = response.status();

            // GitHub signals "rate limit exhausted" with HTTP 403 + the header
            // `X-RateLimit-Remaining: 0`. The header check is essential because
            // a private repo also returns 403 (but with remaining > 0) — we
            // must not mis-classify "no access" as "rate limited".
            if status == reqwest::StatusCode::FORBIDDEN
                && response
                    .headers()
                    .get("x-ratelimit-remaining")
                    .and_then(|v| v.to_str().ok())
                    == Some("0")
            {
                let reset_hint = response
                    .headers()
                    .get("x-ratelimit-reset")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .map(format_reset_hint)
                    .unwrap_or_default();

                let token_hint = if self.token.is_some() {
                    // User already has a token but still hit the quota — this
                    // means the authenticated 5000/hr ceiling was reached, so
                    // no token suggestion would help.
                    ""
                } else {
                    " Set the GITHUB_TOKEN (or GH_TOKEN) environment variable \
                     to raise the limit from 60 to 5000 requests/hour."
                };

                return Err(format!(
                    "GitHub API rate limit exhausted.{reset_hint}{token_hint}"
                ));
            }

            return Err(format!("HTTP {status}"));
        }

        response.json().await.map_err(|e| format!("parse: {e}"))
    }

    /// Resolve every dep in `deps` by fetching each unique `owner/repo`
    /// exactly once and re-using the cached tag list across deps.
    pub async fn resolve_batch(
        &self,
        deps: &[DependencySpec],
        target: TargetLevel,
    ) -> Vec<(usize, Result<ResolvedVersion, DcuError>)> {
        // Step 1: collect unique repos. Sub-actions (`owner/repo/sub`) collapse
        // to the same key as `owner/repo`.
        let mut unique_repos: HashSet<String> = HashSet::new();
        for dep in deps {
            if let Some(key) = Self::repo_key(&dep.name) {
                unique_repos.insert(key);
            }
        }

        // Step 2: fan out fetches in parallel.
        let mut fetch_futures = Vec::with_capacity(unique_repos.len());
        for repo in &unique_repos {
            let repo = repo.clone();
            let me = self.clone();
            fetch_futures.push(async move {
                let result = me.fetch_tags(&repo).await;
                (repo, result)
            });
        }

        let fetched = futures::future::join_all(fetch_futures).await;
        let mut tags_by_repo: HashMap<String, Result<Vec<Tag>, String>> = HashMap::new();
        for (repo, result) in fetched {
            tags_by_repo.insert(repo, result);
        }

        // Step 3: resolve each dep against the cached tag list. Errors are
        // duplicated per-dep so each failing dep gets its own diagnostic.
        let mut results = Vec::with_capacity(deps.len());
        for (idx, dep) in deps.iter().enumerate() {
            let Some(key) = Self::repo_key(&dep.name) else {
                results.push((
                    idx,
                    Err(DcuError::RegistryLookup {
                        package: dep.name.clone(),
                        detail: "action name must be owner/repo[/...]".to_owned(),
                    }),
                ));
                continue;
            };

            match tags_by_repo.get(&key) {
                Some(Ok(tags)) => {
                    let resolved = select_from_tags(tags, &dep.current_req, target);
                    trace!(
                        action = %dep.name,
                        current = %dep.current_req,
                        selected = ?resolved.selected,
                        "resolved tag"
                    );
                    results.push((idx, Ok(resolved)));
                }
                Some(Err(detail)) => {
                    results.push((
                        idx,
                        Err(DcuError::RegistryLookup {
                            package: dep.name.clone(),
                            detail: detail.clone(),
                        }),
                    ));
                }
                None => {
                    // Defensive — should be unreachable since we populated the
                    // map from the same `unique_repos` set used here.
                    results.push((
                        idx,
                        Err(DcuError::RegistryLookup {
                            package: dep.name.clone(),
                            detail: "internal: tag cache miss".to_owned(),
                        }),
                    ));
                }
            }
        }

        results
    }
}

impl Default for GitHubActionsRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Read the GitHub auth token from environment, preferring `GITHUB_TOKEN`
/// over `GH_TOKEN` (matches the precedence used by the `gh` CLI). Returns
/// `None` for empty or missing values.
fn token_from_env() -> Option<Arc<str>> {
    std::env::var("GITHUB_TOKEN")
        .or_else(|_| std::env::var("GH_TOKEN"))
        .ok()
        .filter(|s| !s.is_empty())
        .map(Arc::from)
}

/// Format a `X-RateLimit-Reset` Unix timestamp as a human-friendly hint like
/// `" Resets in 23 minutes."`. Returns an empty string if the timestamp is in
/// the past, in the future by < 1 second, or the host clock cannot be read —
/// keeping the surrounding error message readable even when the hint is missing.
fn format_reset_hint(reset_unix: u64) -> String {
    let Ok(now_dur) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return String::new();
    };
    let now = now_dur.as_secs();
    if reset_unix <= now {
        return String::new();
    }
    let secs = reset_unix - now;
    if secs >= 60 {
        let minutes = secs / 60;
        let plural = if minutes == 1 { "" } else { "s" };
        format!(" Resets in {minutes} minute{plural}.")
    } else {
        let plural = if secs == 1 { "" } else { "s" };
        format!(" Resets in {secs} second{plural}.")
    }
}

/// Parse and pad a tag name into a node-semver Version.
///
/// Returns `None` for non-version refs (`main`, SHAs) or tags that fail
/// semver parsing after padding (e.g. `release-x`).
///
/// `v5` → `5.0.0`
/// `v5.1` → `5.1.0`
/// `v5.1.0` → `5.1.0`
/// `1.0.0-beta.1` → `1.0.0-beta.1`
fn normalize_tag(tag: &str) -> Option<node_semver::Version> {
    if !is_version_ref(tag) {
        return None;
    }
    let stripped = tag.strip_prefix('v').unwrap_or(tag);
    // Separate the numeric `1.2.3` head from a `-pre+build` tail.
    let (numeric, suffix) = stripped
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .map_or((stripped, ""), |i| stripped.split_at(i));

    let parts: Vec<&str> = numeric.split('.').filter(|s| !s.is_empty()).collect();
    let padded = match parts.len() {
        1 => format!("{}.0.0{}", parts[0], suffix),
        2 => format!("{}.{}.0{}", parts[0], parts[1], suffix),
        3.. => format!("{numeric}{suffix}"),
        0 => return None,
    };

    node_semver::Version::parse(&padded).ok()
}

/// Parse the user's current ref so we can compare against tag versions.
fn parse_current_ref(req: &str) -> Option<node_semver::Version> {
    normalize_tag(req)
}

/// Select a tag for the dep based on the target level.
fn select_from_tags(tags: &[Tag], current_req: &str, target: TargetLevel) -> ResolvedVersion {
    // Parse + sort ascending by semver.
    let mut versions: Vec<node_semver::Version> =
        tags.iter().filter_map(|t| normalize_tag(&t.name)).collect();
    versions.sort();

    let highest_stable = versions
        .iter()
        .rev()
        .find(|v| v.pre_release.is_empty())
        .map(node_semver::Version::to_string);

    let current = parse_current_ref(current_req);
    let current_is_pre = current.as_ref().is_some_and(|v| !v.pre_release.is_empty());

    // Pre-release tail policy (matches the npm registry): if user is on a
    // pre-release of X.Y.Z, accept further pre-releases on the same X.Y.Z
    // train AND any stable; never jump to an unrelated pre-release.
    let accept = |v: &&node_semver::Version| -> bool {
        if v.pre_release.is_empty() {
            return true;
        }
        let Some(cur) = current.as_ref() else {
            return false;
        };
        if !current_is_pre {
            return false;
        }
        v.major == cur.major && v.minor == cur.minor && v.patch == cur.patch
    };

    let selected = match target {
        TargetLevel::Latest => {
            if current_is_pre {
                versions
                    .iter()
                    .rev()
                    .find(accept)
                    .map(node_semver::Version::to_string)
            } else {
                highest_stable.clone()
            }
        }
        TargetLevel::Greatest | TargetLevel::Newest => {
            versions.last().map(node_semver::Version::to_string)
        }
        TargetLevel::Minor => {
            let Some(cur) = current.as_ref() else {
                return ResolvedVersion {
                    latest: highest_stable,
                    selected: None,
                };
            };
            versions
                .iter()
                .rev()
                .find(|v| v.major == cur.major && accept(v))
                .map(node_semver::Version::to_string)
        }
        TargetLevel::Patch => {
            let Some(cur) = current.as_ref() else {
                return ResolvedVersion {
                    latest: highest_stable,
                    selected: None,
                };
            };
            versions
                .iter()
                .rev()
                .find(|v| v.major == cur.major && v.minor == cur.minor && accept(v))
                .map(node_semver::Version::to_string)
        }
    };

    ResolvedVersion {
        latest: highest_stable,
        selected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install_crypto_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    fn make_tag(name: &str) -> Tag {
        Tag {
            name: name.to_owned(),
        }
    }

    #[test]
    fn test_normalize_tag_v_prefix() {
        assert_eq!(normalize_tag("v5").unwrap().to_string(), "5.0.0");
        assert_eq!(normalize_tag("v5.1").unwrap().to_string(), "5.1.0");
        assert_eq!(normalize_tag("v5.1.0").unwrap().to_string(), "5.1.0");
    }

    #[test]
    fn test_normalize_tag_prerelease() {
        let v = normalize_tag("v1.0.0-beta.1").unwrap();
        assert_eq!(v.to_string(), "1.0.0-beta.1");
    }

    #[test]
    fn test_normalize_tag_rejects_branch() {
        assert!(normalize_tag("main").is_none());
        assert!(normalize_tag("release/v5").is_none());
    }

    #[test]
    fn test_normalize_tag_rejects_sha() {
        assert!(normalize_tag("8e5e7e5a3b4c1234abcdef0123456789abcdef01").is_none());
    }

    #[test]
    fn test_select_latest_basic() {
        let tags = vec![
            make_tag("v4"),
            make_tag("v4.1.0"),
            make_tag("v5"),
            make_tag("v5.0.0"),
            make_tag("v5.1.0"),
        ];
        let r = select_from_tags(&tags, "v4", TargetLevel::Latest);
        assert_eq!(r.selected.as_deref(), Some("5.1.0"));
        assert_eq!(r.latest.as_deref(), Some("5.1.0"));
    }

    #[test]
    fn test_select_latest_skips_prerelease() {
        let tags = vec![make_tag("v4.0.0"), make_tag("v5.0.0-beta.1")];
        let r = select_from_tags(&tags, "v4", TargetLevel::Latest);
        assert_eq!(r.selected.as_deref(), Some("4.0.0"));
    }

    #[test]
    fn test_select_greatest_includes_prerelease() {
        let tags = vec![make_tag("v4.0.0"), make_tag("v5.0.0-beta.1")];
        let r = select_from_tags(&tags, "v4", TargetLevel::Greatest);
        assert_eq!(r.selected.as_deref(), Some("5.0.0-beta.1"));
    }

    #[test]
    fn test_select_minor_stays_same_major() {
        let tags = vec![
            make_tag("v4.0.0"),
            make_tag("v4.1.0"),
            make_tag("v4.2.0"),
            make_tag("v5.0.0"),
        ];
        let r = select_from_tags(&tags, "v4.0.0", TargetLevel::Minor);
        assert_eq!(r.selected.as_deref(), Some("4.2.0"));
    }

    #[test]
    fn test_select_patch_stays_same_minor() {
        let tags = vec![
            make_tag("v4.0.0"),
            make_tag("v4.0.1"),
            make_tag("v4.0.2"),
            make_tag("v4.1.0"),
        ];
        let r = select_from_tags(&tags, "v4.0.0", TargetLevel::Patch);
        assert_eq!(r.selected.as_deref(), Some("4.0.2"));
    }

    #[test]
    fn test_select_handles_empty_tag_list() {
        let r = select_from_tags(&[], "v4", TargetLevel::Latest);
        assert_eq!(r.selected, None);
        assert_eq!(r.latest, None);
    }

    #[test]
    fn test_select_ignores_non_version_tags() {
        let tags = vec![
            make_tag("main"),
            make_tag("v4.0.0"),
            make_tag("release/v5"),
            make_tag("v4.1.0"),
        ];
        let r = select_from_tags(&tags, "v4", TargetLevel::Latest);
        assert_eq!(r.selected.as_deref(), Some("4.1.0"));
    }

    #[test]
    fn test_repo_key_normal() {
        assert_eq!(
            GitHubActionsRegistry::repo_key("actions/checkout"),
            Some("actions/checkout".to_owned())
        );
    }

    #[test]
    fn test_repo_key_with_subdir() {
        assert_eq!(
            GitHubActionsRegistry::repo_key("actions/checkout/sub/dir"),
            Some("actions/checkout".to_owned())
        );
    }

    #[test]
    fn test_repo_key_invalid() {
        assert_eq!(GitHubActionsRegistry::repo_key("checkout"), None);
        assert_eq!(GitHubActionsRegistry::repo_key(""), None);
    }

    #[test]
    fn test_select_prerelease_tail_picks_higher_prerelease() {
        let tags = vec![
            make_tag("v3.5.0"),
            make_tag("v4.0.0-beta.1"),
            make_tag("v4.0.0-beta.3"),
        ];
        let r = select_from_tags(&tags, "v4.0.0-beta.1", TargetLevel::Latest);
        assert_eq!(r.selected.as_deref(), Some("4.0.0-beta.3"));
    }

    #[test]
    fn test_select_prerelease_to_stable_upgrade() {
        let tags = vec![make_tag("v4.0.0-beta.1"), make_tag("v4.0.0")];
        let r = select_from_tags(&tags, "v4.0.0-beta.1", TargetLevel::Latest);
        // Same train stable wins over the prerelease.
        assert_eq!(r.selected.as_deref(), Some("4.0.0"));
    }

    fn tags_response(names: &[&str]) -> serde_json::Value {
        let arr: Vec<_> = names
            .iter()
            .map(|n| serde_json::json!({ "name": n }))
            .collect();
        serde_json::Value::Array(arr)
    }

    #[tokio::test]
    async fn test_resolve_batch_fetches_each_repo_once() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        install_crypto_provider();
        let mock = MockServer::start().await;

        // The mock expects exactly ONE call to `/repos/actions/checkout/tags`
        // even though two deps reference the same repo.
        Mock::given(method("GET"))
            .and(path("/repos/actions/checkout/tags"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(tags_response(&["v4", "v4.0.0", "v5", "v5.0.0"])),
            )
            .expect(1)
            .mount(&mock)
            .await;

        let reg = GitHubActionsRegistry::with_base_url(&mock.uri());
        let deps = vec![
            DependencySpec {
                name: "actions/checkout".to_owned(),
                current_req: "v4".to_owned(),
                section: dependency_check_updates_core::DependencySection::GitHubActions,
            },
            DependencySpec {
                name: "actions/checkout/sub".to_owned(),
                current_req: "v4".to_owned(),
                section: dependency_check_updates_core::DependencySection::GitHubActions,
            },
        ];
        let results = reg.resolve_batch(&deps, TargetLevel::Latest).await;
        assert_eq!(results.len(), 2);
        for (_, result) in &results {
            assert!(result.is_ok());
            assert_eq!(result.as_ref().unwrap().selected.as_deref(), Some("5.0.0"));
        }
    }

    #[tokio::test]
    async fn test_resolve_batch_404_per_dep() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        install_crypto_provider();
        let mock = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/repos/does-not/exist/tags"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock)
            .await;

        let reg = GitHubActionsRegistry::with_base_url(&mock.uri());
        let deps = vec![DependencySpec {
            name: "does-not/exist".to_owned(),
            current_req: "v1".to_owned(),
            section: dependency_check_updates_core::DependencySection::GitHubActions,
        }];
        let results = reg.resolve_batch(&deps, TargetLevel::Latest).await;
        assert_eq!(results.len(), 1);
        let (_, ref result) = results[0];
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_resolve_batch_invalid_name_errors() {
        install_crypto_provider();
        let reg = GitHubActionsRegistry::with_base_url("http://127.0.0.1:1");
        let deps = vec![DependencySpec {
            name: "no-slash".to_owned(),
            current_req: "v1".to_owned(),
            section: dependency_check_updates_core::DependencySection::GitHubActions,
        }];
        let results = reg.resolve_batch(&deps, TargetLevel::Latest).await;
        assert_eq!(results.len(), 1);
        let (_, ref result) = results[0];
        assert!(result.is_err());
    }

    #[test]
    fn test_new_default_construct() {
        install_crypto_provider();
        let _ = GitHubActionsRegistry::new();
        let _ = GitHubActionsRegistry::default();
    }

    #[test]
    fn test_format_reset_hint_minutes() {
        let future = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 1500; // 25 minutes ahead
        let hint = format_reset_hint(future);
        assert!(hint.contains("Resets in"));
        assert!(hint.contains("minute"));
    }

    #[test]
    fn test_format_reset_hint_seconds_for_under_minute() {
        let future = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 30;
        let hint = format_reset_hint(future);
        assert!(hint.contains("Resets in"));
        assert!(hint.contains("second"));
    }

    #[test]
    fn test_format_reset_hint_singular_form() {
        let future = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 60; // exactly 1 minute
        let hint = format_reset_hint(future);
        assert!(hint.contains("1 minute."), "got: {hint}");
        assert!(!hint.contains("minutes"));
    }

    #[test]
    fn test_format_reset_hint_past_returns_empty() {
        // Reset already happened — no hint to give.
        let hint = format_reset_hint(0);
        assert_eq!(hint, "");
    }

    #[tokio::test]
    async fn test_rate_limit_403_produces_helpful_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        install_crypto_provider();
        let mock = MockServer::start().await;

        // Future reset timestamp so the hint formatter produces output.
        let reset = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 300;

        Mock::given(method("GET"))
            .and(path("/repos/actions/checkout/tags"))
            .respond_with(
                ResponseTemplate::new(403)
                    .insert_header("X-RateLimit-Remaining", "0")
                    .insert_header("X-RateLimit-Reset", reset.to_string().as_str())
                    .set_body_string("rate limited"),
            )
            .mount(&mock)
            .await;

        // Build with an explicit `None` token so the assertion below for
        // "GITHUB_TOKEN" in the error hint is deterministic regardless of
        // whatever the developer's shell has exported.
        let reg = GitHubActionsRegistry::with_base_url_and_token(&mock.uri(), None);

        let deps = vec![DependencySpec {
            name: "actions/checkout".to_owned(),
            current_req: "v5".to_owned(),
            section: dependency_check_updates_core::DependencySection::GitHubActions,
        }];
        let results = reg.resolve_batch(&deps, TargetLevel::Latest).await;
        assert_eq!(results.len(), 1);
        let (_, ref result) = results[0];
        let err = result.as_ref().expect_err("expected rate-limit error");
        let msg = err.to_string();
        // The DcuError::RegistryLookup variant prints only the package name;
        // the detailed `detail` lives in the Display source chain.
        // We can pull it back out via the source().
        let detail = format!("{err:?}");
        assert!(
            detail.contains("rate limit") && detail.contains("GITHUB_TOKEN"),
            "expected helpful rate-limit message, got: {msg} / debug: {detail}"
        );
    }

    #[tokio::test]
    async fn test_403_without_rate_limit_header_remains_generic() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        install_crypto_provider();
        let mock = MockServer::start().await;

        // 403 with `X-RateLimit-Remaining: 42` indicates a private repo or other
        // permission issue — NOT a rate limit. We must keep the generic
        // "HTTP 403" message in that case.
        Mock::given(method("GET"))
            .and(path("/repos/private/repo/tags"))
            .respond_with(ResponseTemplate::new(403).insert_header("X-RateLimit-Remaining", "42"))
            .mount(&mock)
            .await;

        let reg = GitHubActionsRegistry::with_base_url(&mock.uri());
        let deps = vec![DependencySpec {
            name: "private/repo".to_owned(),
            current_req: "v1".to_owned(),
            section: dependency_check_updates_core::DependencySection::GitHubActions,
        }];
        let results = reg.resolve_batch(&deps, TargetLevel::Latest).await;
        let (_, ref result) = results[0];
        let err = result.as_ref().expect_err("expected error");
        let detail = format!("{err:?}");
        assert!(
            detail.contains("HTTP 403") && !detail.contains("rate limit"),
            "private-repo 403 must keep generic message, got: {detail}"
        );
    }
}
