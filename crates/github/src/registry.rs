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

use dependency_check_updates_core::{
    DcuError, DependencySpec, ResolvedVersion, TargetLevel, build_client, pad_to_three_segments,
    split_numeric_head,
};

use crate::parser::is_version_ref;

/// Cap on parallel GitHub API calls. The unauthenticated rate limit is
/// 60 req/hr; keeping concurrency modest avoids burst-rejection during deep
/// scans of multi-workflow repos.
const MAX_CONCURRENT_REQUESTS: usize = 5;

/// Tags page size — GitHub API max is 100, which comfortably covers every
/// mainstream action; spanning multiple pages is future work.
const TAGS_PER_PAGE: u32 = 100;

/// One tag entry from the GitHub API.
#[derive(Debug, Deserialize, Clone)]
struct Tag {
    name: String,
}

/// Per-repo tag list with its parse + sort + highest-stable lookup AND the
/// `pick_existing_ref` numeric-prefix set already done once. Built once per
/// unique `owner/repo` in `resolve_batch` so each duplicate-repo dep reuses
/// the same prepared data instead of re-parsing and re-sorting the same
/// 100-entry tag list — and instead of rebuilding the same `tag_numerics`
/// `HashSet` inside `pick_existing_ref` for every dep that shares the repo.
struct PreparedTags {
    sorted_versions: Vec<node_semver::Version>,
    highest_stable: Option<String>,
    /// Numeric tag prefixes (`v8.1.0` → `8.1.0`, `v8` → `8`) used by
    /// `pick_existing_ref` to check existence at a given precision. Owned
    /// `String`s let the set outlive the source `Vec<Tag>` so the tag list
    /// can be consumed once and dropped.
    tag_numerics: HashSet<String>,
}

impl PreparedTags {
    fn new(tags: Vec<Tag>) -> Self {
        // Single pass over the tag list: build `sorted_versions` and
        // `tag_numerics` together AND inline the per-tag upfront work so
        // `is_version_ref` and `strip_prefix('v')` each run exactly once
        // per tag. The previous form delegated to `normalize_tag` AND
        // `tag_numeric_str`, which each repeated `is_version_ref` + the
        // `v`-strip — paying that cost twice per tag. The two helpers
        // are kept intact for their other callers (`parse_current_ref`
        // and `pick_existing_ref` respectively).
        // Capture the length before consuming `tags` in the loop below so we
        // can pre-size both output collections and avoid grow-path reallocs.
        let capacity = tags.len();
        let mut sorted_versions: Vec<node_semver::Version> = Vec::with_capacity(capacity);
        let mut tag_numerics: HashSet<String> = HashSet::with_capacity(capacity);
        for tag in tags {
            // Mirror `normalize_tag` + `tag_numeric_str` semantics exactly,
            // but share the up-front work between them. Skip both pushes when
            // either `is_version_ref` is false OR `Version::parse` refuses
            // (e.g. 4+-segment numerics) — same as `normalize_tag = None`.
            if !is_version_ref(&tag.name) {
                continue;
            }
            let stripped = tag.name.strip_prefix('v').unwrap_or(&tag.name);
            let numeric_str = split_numeric_head(stripped).0.trim_end_matches('.');
            let padded = pad_to_three_segments(stripped);
            let Ok(version) = node_semver::Version::parse(&padded) else {
                continue;
            };
            sorted_versions.push(version);
            tag_numerics.insert(numeric_str.to_owned());
        }
        sorted_versions.sort();
        let highest_stable = sorted_versions
            .iter()
            .rev()
            .find(|v| v.pre_release.is_empty())
            .map(node_semver::Version::to_string);
        Self {
            sorted_versions,
            highest_stable,
            tag_numerics,
        }
    }
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
        Self {
            client: build_client(),
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
    ///
    /// # Panics
    ///
    /// Panics only if the internal tag cache is missing a key that
    /// `repo_key` produced — an invariant violation, not a user-input issue.
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
        // Build prepared tag data ONCE per unique repo (parse + sort +
        // highest-stable). A workflow that uses the same repo across N jobs
        // now pays this cost once instead of N times.
        let mut prepared_by_repo: HashMap<String, Result<PreparedTags, String>> = HashMap::new();
        for (repo, result) in fetched {
            prepared_by_repo.insert(repo, result.map(PreparedTags::new));
        }

        // Step 3: resolve each dep against the cached prepared data. Errors
        // are duplicated per-dep so each failing dep gets its own diagnostic.
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

            // Safe because `key` came from `repo_key(&dep.name)`, and every
            // such value was inserted into `unique_repos` (and therefore into
            // `prepared_by_repo`) above. Using `.expect()` documents the
            // invariant and keeps the code path linear for coverage.
            match prepared_by_repo
                .get(&key)
                .expect("tags cache must contain every unique repo key")
            {
                Ok(prepared) => {
                    let mut resolved = select_from_tags(prepared, &dep.current_req, target);
                    // Collapse the resolved full version to the shortest ref
                    // form that an actual tag backs (e.g. `v8` → `v8.1.0` when
                    // only the full tag was published), so the emitted ref
                    // never dangles. `compute_updates` skips its generic
                    // precision truncation for GitHub on the strength of this.
                    resolved.selected = resolved.selected.map(|sel| {
                        pick_existing_ref(&sel, &dep.current_req, &prepared.tag_numerics)
                    });
                    trace!(
                        action = %dep.name,
                        current = %dep.current_req,
                        selected = ?resolved.selected,
                        "resolved tag"
                    );
                    results.push((idx, Ok(resolved)));
                }
                Err(detail) => {
                    results.push((
                        idx,
                        Err(DcuError::RegistryLookup {
                            package: dep.name.clone(),
                            detail: detail.clone(),
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
    // `unwrap_or(ZERO)` collapses the (practically impossible) clock-before-1970
    // case into "treat as if now=0", which then falls through to the past-or-
    // present check below. Avoiding a separate early-return keeps coverage
    // honest without testing an unreachable branch.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
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
    // Pad the numeric `1.2.3` head to three segments, preserving any
    // `-pre+build` tail. Shared with the cli `compute_updates` safety net
    // so both crates pad identically.
    //
    // Byte-equivalence with the old inline logic: `is_version_ref` guarantees
    // `stripped` starts with a digit, so `find(non-digit-dot)` produces a
    // (numeric, suffix) split whose concatenation IS `stripped`. The shared
    // helper's `_ => v.to_owned()` therefore yields the same bytes as the old
    // `_ => format!("{numeric}{suffix}")` for >=3-segment inputs.
    let padded = pad_to_three_segments(stripped);

    node_semver::Version::parse(&padded).ok()
}

/// Parse the user's current ref so we can compare against tag versions.
fn parse_current_ref(req: &str) -> Option<node_semver::Version> {
    normalize_tag(req)
}

/// Select a tag for the dep based on the target level.
///
/// Parses + sorts the tag list, then delegates the target-match algorithm to
/// [`dependency_check_updates_core::select_version`]. GitHub-specific
/// behaviour is supplied via the fallbacks: the highest stable tag stands in
/// for `latest`, while an unparseable current ref yields `None` for
/// `Minor`/`Patch` (there is no major to stay on).
///
/// Note: `newest` resolves to the same result as `greatest` here. The Tags
/// API does not expose per-tag publish dates (that would need an extra commit
/// lookup per tag), so true publish-date ordering is intentionally not
/// attempted — unlike the npm/crates.io/PyPI registries, whose responses
/// already carry timestamps.
fn select_from_tags(
    prepared: &PreparedTags,
    current_req: &str,
    target: TargetLevel,
) -> ResolvedVersion {
    // Parsing + sorting + highest-stable extraction live on `PreparedTags`,
    // built once per unique repo by `resolve_batch`. This function is now
    // only the per-dep `current_req` parse + `select_version` dispatch.
    let current = parse_current_ref(current_req);

    let selected = dependency_check_updates_core::select_version(
        current.as_ref(),
        &prepared.sorted_versions,
        target,
        prepared.highest_stable.as_deref(),
        None,
    );

    ResolvedVersion {
        latest: prepared.highest_stable.clone(),
        selected,
    }
}

/// The bare numeric head of a tag (`v8.1.0` → `8.1.0`, `v8` → `8`,
/// `v7.6` → `7.6`). Returns `None` for non-version refs (`main`, SHAs).
fn tag_numeric_str(tag: &str) -> Option<&str> {
    // `is_version_ref` guarantees the post-`v` head starts with a digit, so the
    // numeric run below is always non-empty.
    if !is_version_ref(tag) {
        return None;
    }
    let stripped = tag.strip_prefix('v').unwrap_or(tag);
    Some(split_numeric_head(stripped).0.trim_end_matches('.'))
}

/// Count the segment precision of the user's current ref (`v7` → 1,
/// `v7.6` → 2, `v7.6.0` → 3). Always at least 1.
fn ref_precision(req: &str) -> usize {
    let stripped = req.strip_prefix('v').unwrap_or(req);
    split_numeric_head(stripped)
        .0
        .split('.')
        .filter(|s| !s.is_empty())
        .count()
        .max(1)
}

/// Collapse a resolved full version to the shortest tag form that an actual
/// tag backs, preferring the user's current pin precision or longer.
///
/// Actions usually publish a moving major tag (`v8`) next to `v8.1.0`, but not
/// always — when only `v8.1.0` exists, the naive major float `@v8` would 404.
/// This walks the user's precision upward (`v8` → `v8.1` → `v8.1.0`) and, only
/// if nothing at or above that precision is published, downward, returning the
/// first form an actual tag matches. Pre-release / build refs are published
/// only as exact full tags and are returned verbatim.
///
/// The ref the user currently pins is treated as known-to-exist (their
/// workflow runs on it right now). So when the selected version shares the
/// current pin's prefix at the pin precision, that very same ref is returned
/// without consulting the (capped) fetched tag window — otherwise a moving
/// major tag like `@v2` that sorts past the 100-tag fetch ceiling (e.g.
/// `taiki-e/install-action`, which publishes hundreds of `v2.x.y` patch tags)
/// would be wrongly escalated to `v2.81.6`, surfacing a spurious update even
/// though `@v2` already floats to that version.
fn pick_existing_ref(selected: &str, current_req: &str, tag_numerics: &HashSet<String>) -> String {
    let (numeric, suffix) = split_numeric_head(selected);
    if !suffix.is_empty() {
        return selected.to_owned();
    }

    let segments: Vec<&str> = numeric.split('.').filter(|s| !s.is_empty()).collect();
    let len = segments.len();
    let start = ref_precision(current_req).clamp(1, len.max(1));

    // If the selected version's prefix at the pin precision equals the ref the
    // user already pins, keep that form: it is provably backed by a tag (the
    // user is on it), so no fetch-window lookup is needed and no escalation
    // should happen. Crossing to a different prefix (e.g. a new major) falls
    // through to the existence-checked walk below.
    let current_prefix = segments[..start].join(".");
    if tag_numeric_str(current_req) == Some(current_prefix.as_str()) {
        return current_prefix;
    }

    // `tag_numerics` (the set of numeric tag prefixes such as `5.0.0`, `5`)
    // is precomputed once per unique repo in `PreparedTags::new`, so every
    // dep that shares the repo reuses the same HashSet instead of rebuilding
    // a per-call one — turning the per-dep O(tags) HashSet build into O(1).
    // Prefer the shortest form at or above the pin precision; otherwise the
    // longest shorter form. The resolved version always came from a real tag,
    // so some precision in this order always matches — the `expect` documents
    // that invariant and keeps the success line on the covered path.
    let chosen = (start..=len)
        .chain((1..start).rev())
        .find(|&p| {
            let candidate = segments[..p].join(".");
            tag_numerics.contains(candidate.as_str())
        })
        .expect("resolved version is always backed by at least one tag");
    segments[..chosen].join(".")
}

#[cfg(test)]
mod tests {
    use super::*;
    use dependency_check_updates_core::DependencySection;
    use rstest::rstest;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path as match_path},
    };

    /// Idempotent rustls provider install. Returning `Err` (already set) is
    /// the expected steady-state once any test has run — `let _ =` swallows it.
    ///
    /// Kept as a plain helper rather than an rstest `#[fixture]` because the
    /// idiomatic `#[from(...)] _setup: ()` pattern collides with clippy's
    /// `pedantic::used_underscore_binding` lint (rstest's expansion references
    /// the underscore-prefixed binding), and any non-underscore name would
    /// require boilerplate to silence `unused_variables`. Calling it inline
    /// at the top of each test reads cleanly and stays clippy-clean.
    fn install_crypto_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    fn make_tag(name: &str) -> Tag {
        Tag {
            name: name.to_owned(),
        }
    }

    fn make_tags(names: &[&str]) -> Vec<Tag> {
        names.iter().copied().map(make_tag).collect()
    }

    fn tags_response(names: &[&str]) -> serde_json::Value {
        let arr: Vec<_> = names
            .iter()
            .map(|n| serde_json::json!({ "name": n }))
            .collect();
        serde_json::Value::Array(arr)
    }

    fn dep(name: &str, current_req: &str) -> DependencySpec {
        DependencySpec {
            name: name.to_owned(),
            current_req: current_req.to_owned(),
            section: DependencySection::GitHubActions,
            path_version: None,
        }
    }

    #[rstest]
    // v-prefix tags pad to three segments.
    #[case::v_major("v5", Some("5.0.0"))]
    #[case::v_major_minor("v5.1", Some("5.1.0"))]
    #[case::v_major_minor_patch("v5.1.0", Some("5.1.0"))]
    // Pre-release suffix survives unchanged.
    #[case::prerelease("v1.0.0-beta.1", Some("1.0.0-beta.1"))]
    // Branch-like / slashed refs are not version-like.
    #[case::branch_main("main", None)]
    #[case::branch_release_slash("release/v5", None)]
    // 40-char SHA fails the hex+length heuristic.
    #[case::sha_40_char("8e5e7e5a3b4c1234abcdef0123456789abcdef01", None)]
    fn normalize_tag_cases(#[case] input: &str, #[case] expected: Option<&str>) {
        let actual = normalize_tag(input).map(|v| v.to_string());
        assert_eq!(actual.as_deref(), expected);
    }

    #[rstest]
    // Latest stable across a v4/v5 train.
    #[case::latest_basic(
        &["v4", "v4.1.0", "v5", "v5.0.0", "v5.1.0"],
        "v4",
        TargetLevel::Latest,
        Some("5.1.0"),
        Some("5.1.0")
    )]
    // Latest must skip prereleases when current is stable.
    #[case::latest_skips_prerelease(
        &["v4.0.0", "v5.0.0-beta.1"],
        "v4",
        TargetLevel::Latest,
        Some("4.0.0"),
        Some("4.0.0")
    )]
    // Greatest includes prereleases.
    #[case::greatest_includes_prerelease(
        &["v4.0.0", "v5.0.0-beta.1"],
        "v4",
        TargetLevel::Greatest,
        Some("5.0.0-beta.1"),
        Some("4.0.0")
    )]
    // Minor stays on the current major.
    #[case::minor_stays_same_major(
        &["v4.0.0", "v4.1.0", "v4.2.0", "v5.0.0"],
        "v4.0.0",
        TargetLevel::Minor,
        Some("4.2.0"),
        Some("5.0.0")
    )]
    // Patch stays on the current minor.
    #[case::patch_stays_same_minor(
        &["v4.0.0", "v4.0.1", "v4.0.2", "v4.1.0"],
        "v4.0.0",
        TargetLevel::Patch,
        Some("4.0.2"),
        Some("4.1.0")
    )]
    // Empty tag list yields None for both.
    #[case::empty_tag_list(
        &[],
        "v4",
        TargetLevel::Latest,
        None,
        None
    )]
    // Non-version tags interspersed are filtered out.
    #[case::ignores_non_version_tags(
        &["main", "v4.0.0", "release/v5", "v4.1.0"],
        "v4",
        TargetLevel::Latest,
        Some("4.1.0"),
        Some("4.1.0")
    )]
    // Minor happy-path: stable v4.1.0 wins over the in-between prerelease.
    #[case::minor_rejects_pre_when_current_is_stable_happy_path(
        &["v4.0.0", "v4.1.0-beta.1", "v4.1.0"],
        "v4.0.0",
        TargetLevel::Minor,
        Some("4.1.0"),
        Some("4.1.0")
    )]
    // Minor edge: prerelease sits above the only stable → reject it, fall back.
    #[case::minor_rejects_pre_when_no_higher_stable_exists(
        &["v4.0.0", "v4.1.0-beta.1"],
        "v4.0.0",
        TargetLevel::Minor,
        Some("4.0.0"),
        Some("4.0.0")
    )]
    // Minor with unparseable current → selected None, latest still surfaced.
    #[case::minor_with_unparseable_current_returns_none(
        &["v4.0.0", "v4.1.0"],
        "main",
        TargetLevel::Minor,
        None,
        Some("4.1.0")
    )]
    // Patch with unparseable current → selected None, latest still surfaced.
    #[case::patch_with_unparseable_current_returns_none(
        &["v4.0.0", "v4.0.1"],
        "main",
        TargetLevel::Patch,
        None,
        Some("4.0.1")
    )]
    // Prerelease train: pick the highest prerelease on the same train.
    #[case::prerelease_tail_picks_higher_prerelease(
        &["v3.5.0", "v4.0.0-beta.1", "v4.0.0-beta.3"],
        "v4.0.0-beta.1",
        TargetLevel::Latest,
        Some("4.0.0-beta.3"),
        Some("3.5.0")
    )]
    // Same-train stable beats the prerelease it came from.
    #[case::prerelease_to_stable_upgrade(
        &["v4.0.0-beta.1", "v4.0.0"],
        "v4.0.0-beta.1",
        TargetLevel::Latest,
        Some("4.0.0"),
        Some("4.0.0")
    )]
    fn select_from_tags_cases(
        #[case] tag_names: &[&str],
        #[case] current_req: &str,
        #[case] target: TargetLevel,
        #[case] expected_selected: Option<&str>,
        #[case] expected_latest: Option<&str>,
    ) {
        let prepared = PreparedTags::new(make_tags(tag_names));
        let r = select_from_tags(&prepared, current_req, target);
        assert_eq!(r.selected.as_deref(), expected_selected);
        assert_eq!(r.latest.as_deref(), expected_latest);
    }

    #[rstest]
    #[case::v_major("v8", Some("8"))]
    #[case::v_major_minor("v7.6", Some("7.6"))]
    #[case::v_full("v8.1.0", Some("8.1.0"))]
    #[case::no_v_prefix("5", Some("5"))]
    #[case::prerelease_head("v2.0.0-rc.1", Some("2.0.0"))]
    #[case::branch("main", None)]
    #[case::sha("8e5e7e5a3b4c1234abcdef0123456789abcdef01", None)]
    fn tag_numeric_str_cases(#[case] input: &str, #[case] expected: Option<&str>) {
        assert_eq!(tag_numeric_str(input), expected);
    }

    #[rstest]
    #[case::major("v7", 1)]
    #[case::major_minor("v7.6", 2)]
    #[case::major_minor_patch("v7.6.0", 3)]
    #[case::no_v("5", 1)]
    #[case::unparseable_falls_back_to_one("main", 1)]
    fn ref_precision_cases(#[case] input: &str, #[case] expected: usize) {
        assert_eq!(ref_precision(input), expected);
    }

    #[rstest]
    // selected full version, current pin, available tag names, expected ref form.
    // Major moving tag exists → preserve the major-pin precision.
    #[case::major_tag_exists("6.0.0", "v5", &["v5", "v6", "v6.0.0"], "6")]
    // `v8` moving tag missing → escalate v8 → v8.1 → v8.1.0 (the real tag).
    #[case::escalate_to_full("8.1.0", "v7", &["v7", "v8.0.0", "v8.1.0"], "8.1.0")]
    // `v8.1` short tag exists → stop escalating there.
    #[case::escalate_to_two_segment("8.1.0", "v7", &["v7", "v8.1", "v8.1.0"], "8.1")]
    // Full pin stays full when the full tag exists.
    #[case::full_pin_keeps_full("6.0.0", "v5.1.0", &["v6", "v6.0.0"], "6.0.0")]
    // Full pin but only a major moving tag exists → de-escalate to it.
    #[case::de_escalate_to_major("9.0.0", "v8.1.0", &["v8.1.0", "v9"], "9")]
    // Non-version tags in the list are ignored by the existence check.
    #[case::ignores_non_version_tags("8.1.0", "v7", &["main", "v8.1.0"], "8.1.0")]
    // Pre-release refs are returned verbatim (only ever exact full tags).
    #[case::prerelease_verbatim("2.0.0-rc.1", "v1", &["v2.0.0-rc.1"], "2.0.0-rc.1")]
    // Same-major float pin: `v2` resolves a higher patch in the SAME major, but
    // the `v2` moving tag is absent from the (capped) fetched window. The pin
    // the user is already on is known to exist, so we keep `2` instead of
    // escalating to the full `2.81.6` — no spurious update.
    #[case::same_major_float_kept_without_tag(
        "2.81.6",
        "v2",
        &["v2.81.6", "v2.81.5", "v2.80.0"],
        "2"
    )]
    // Same major.minor float pin, full tag absent from the window → keep `2.5`.
    #[case::same_major_minor_float_kept_without_tag(
        "2.5.9",
        "v2.5",
        &["v2.5.9", "v2.5.8"],
        "2.5"
    )]
    fn pick_existing_ref_cases(
        #[case] selected: &str,
        #[case] current: &str,
        #[case] tag_names: &[&str],
        #[case] expected: &str,
    ) {
        let prepared = PreparedTags::new(make_tags(tag_names));
        assert_eq!(
            pick_existing_ref(selected, current, &prepared.tag_numerics),
            expected,
        );
    }

    #[rstest]
    #[case::normal("actions/checkout", Some("actions/checkout"))]
    #[case::with_subdir("actions/checkout/sub/dir", Some("actions/checkout"))]
    #[case::single_segment("checkout", None)]
    #[case::empty_string("", None)]
    // `splitn(3, '/')` yields empty strings for `foo/` and `/foo`; the
    // empty-half guard must catch them to avoid `/repos/foo//tags` URLs.
    #[case::trailing_slash("foo/", None)]
    #[case::leading_slash("/foo", None)]
    #[case::just_slash("/", None)]
    fn repo_key_cases(#[case] input: &str, #[case] expected: Option<&str>) {
        assert_eq!(GitHubActionsRegistry::repo_key(input).as_deref(), expected);
    }

    #[rstest]
    // 25 minutes ahead → minute-level message.
    #[case::minutes(1500, "minute")]
    // 30 seconds ahead → second-level message.
    #[case::seconds_for_under_minute(30, "second")]
    fn format_reset_hint_unit_cases(#[case] delta_secs: u64, #[case] unit: &str) {
        let future = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + delta_secs;
        let hint = format_reset_hint(future);
        assert!(hint.contains("Resets in"));
        assert!(hint.contains(unit));
    }

    #[test]
    fn format_reset_hint_singular_form() {
        // Exactly 1 minute ahead.
        let future = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 60;
        let hint = format_reset_hint(future);
        assert!(hint.contains("1 minute."), "got: {hint}");
        assert!(!hint.contains("minutes"));
    }

    #[test]
    fn format_reset_hint_past_returns_empty() {
        // Reset already happened — no hint to give.
        assert_eq!(format_reset_hint(0), "");
    }

    #[test]
    fn new_default_construct() {
        install_crypto_provider();
        let _ = GitHubActionsRegistry::new();
        let _ = GitHubActionsRegistry::default();
    }

    #[tokio::test]
    async fn resolve_batch_fetches_each_repo_once() {
        install_crypto_provider();
        let mock = MockServer::start().await;

        // The mock expects exactly ONE call to `/repos/actions/checkout/tags`
        // even though two deps reference the same repo (one direct, one
        // sub-action which collapses to the same key).
        Mock::given(method("GET"))
            .and(match_path("/repos/actions/checkout/tags"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(tags_response(&["v4", "v4.0.0", "v5", "v5.0.0"])),
            )
            .expect(1)
            .mount(&mock)
            .await;

        let reg = GitHubActionsRegistry::with_base_url(&mock.uri());
        let deps = vec![
            dep("actions/checkout", "v4"),
            dep("actions/checkout/sub", "v4"),
        ];
        let results = reg.resolve_batch(&deps, TargetLevel::Latest).await;
        assert_eq!(results.len(), 2);
        for (_, result) in &results {
            assert!(result.is_ok());
            // `v4` is a major pin and a `v5` moving tag exists, so the resolved
            // ref collapses to the major form `5` (→ `v5`), not the full
            // `5.0.0`. This is `pick_existing_ref` preserving the pin precision.
            assert_eq!(result.as_ref().unwrap().selected.as_deref(), Some("5"));
        }
    }

    #[tokio::test]
    async fn resolve_batch_keeps_major_float_when_tag_past_window() {
        install_crypto_provider();
        let mock = MockServer::start().await;

        // Mirrors `taiki-e/install-action`: a high-velocity action whose moving
        // `v2` major tag sorts past the 100-tag fetch ceiling, so only the
        // full `v2.81.x` patch tags are visible. The resolved ref must stay at
        // the `v2` major-float form the user already pins — NOT escalate to the
        // full `2.81.6` (which `compute_updates` would then surface as a bogus
        // `v2 -> v2.81.6` update).
        Mock::given(method("GET"))
            .and(match_path("/repos/taiki-e/install-action/tags"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(tags_response(&["v2.81.6", "v2.81.5", "v2.80.0"])),
            )
            .expect(1)
            .mount(&mock)
            .await;

        let reg = GitHubActionsRegistry::with_base_url(&mock.uri());
        let deps = vec![dep("taiki-e/install-action", "v2")];
        let results = reg.resolve_batch(&deps, TargetLevel::Latest).await;
        assert_eq!(results.len(), 1);
        let (_, ref result) = results[0];
        assert!(result.is_ok());
        assert_eq!(result.as_ref().unwrap().selected.as_deref(), Some("2"));
    }

    #[tokio::test]
    async fn resolve_batch_404_per_dep() {
        install_crypto_provider();
        let mock = MockServer::start().await;

        Mock::given(method("GET"))
            .and(match_path("/repos/does-not/exist/tags"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock)
            .await;

        let reg = GitHubActionsRegistry::with_base_url(&mock.uri());
        let deps = vec![dep("does-not/exist", "v1")];
        let results = reg.resolve_batch(&deps, TargetLevel::Latest).await;
        assert_eq!(results.len(), 1);
        let (_, ref result) = results[0];
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn resolve_batch_invalid_name_errors() {
        install_crypto_provider();
        // No `/` in the name → `repo_key` returns None → per-dep error
        // without ever issuing an HTTP call.
        let reg = GitHubActionsRegistry::with_base_url("http://127.0.0.1:1");
        let deps = vec![dep("no-slash", "v1")];
        let results = reg.resolve_batch(&deps, TargetLevel::Latest).await;
        assert_eq!(results.len(), 1);
        let (_, ref result) = results[0];
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn token_sends_authorization_header() {
        install_crypto_provider();
        let mock = MockServer::start().await;

        // Mount only matches requests that include the exact bearer header,
        // so a missing Authorization header → 404 from wiremock → test failure.
        Mock::given(method("GET"))
            .and(match_path("/repos/actions/checkout/tags"))
            .and(header("authorization", "Bearer secret-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(tags_response(&["v5.0.0"])))
            .expect(1)
            .mount(&mock)
            .await;

        let reg = GitHubActionsRegistry::with_base_url_and_token(&mock.uri(), Some("secret-token"));
        let deps = vec![dep("actions/checkout", "v4")];
        let results = reg.resolve_batch(&deps, TargetLevel::Latest).await;
        let (_, ref result) = results[0];
        assert!(result.is_ok(), "auth header should have matched");
        assert_eq!(result.as_ref().unwrap().selected.as_deref(), Some("5.0.0"));
    }

    /// 403-handling matrix.
    ///
    /// Three near-identical mock setups — only the response headers, the
    /// token presence, and the substrings we expect in the error message
    /// differ. Each case parametrizes (path, token, X-RateLimit-Remaining,
    /// reset-header included?, dep name, must-contain[], must-not-contain[]).
    ///
    /// The detailed `detail` string lives in the source chain of
    /// `DcuError::RegistryLookup`, so we inspect it via `Debug` of the error.
    #[rstest]
    // Rate-limited without a token → message must point at GITHUB_TOKEN.
    #[case::rate_limit_no_token(
        "/repos/actions/checkout/tags",
        None,
        Some("0"),
        true,
        "actions/checkout",
        &["rate limit", "GITHUB_TOKEN"],
        &[]
    )]
    // Rate-limited WITH a token → message must NOT suggest setting one;
    // the user is already past the unauthenticated tier.
    #[case::rate_limit_with_token_omits_set_token_hint(
        "/repos/actions/checkout/tags",
        Some("present-token"),
        Some("0"),
        true,
        "actions/checkout",
        &["rate limit", "Resets in"],
        &["GITHUB_TOKEN"]
    )]
    // 403 with `Remaining: 42` is a permission issue (e.g. private repo) —
    // NOT a rate limit. Keep the generic "HTTP 403" message.
    #[case::generic_403_keeps_status_code(
        "/repos/private/repo/tags",
        None,
        Some("42"),
        false,
        "private/repo",
        &["HTTP 403"],
        &["rate limit"]
    )]
    #[tokio::test]
    async fn http_403_handling(
        #[case] api_path: &str,
        #[case] token: Option<&str>,
        #[case] x_ratelimit_remaining: Option<&str>,
        #[case] include_reset_header: bool,
        #[case] dep_name: &str,
        #[case] must_contain: &[&str],
        #[case] must_not_contain: &[&str],
    ) {
        install_crypto_provider();
        let mock = MockServer::start().await;

        let mut response = ResponseTemplate::new(403);
        if let Some(rl) = x_ratelimit_remaining {
            response = response.insert_header("X-RateLimit-Remaining", rl);
        }
        if include_reset_header {
            // Future reset timestamp so the hint formatter produces output.
            let reset = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
                + 600;
            response = response.insert_header("X-RateLimit-Reset", reset.to_string().as_str());
        }

        Mock::given(method("GET"))
            .and(match_path(api_path))
            .respond_with(response)
            .mount(&mock)
            .await;

        let reg = GitHubActionsRegistry::with_base_url_and_token(&mock.uri(), token);
        let deps = vec![dep(dep_name, "v1")];
        let results = reg.resolve_batch(&deps, TargetLevel::Latest).await;
        assert_eq!(results.len(), 1);
        let (_, ref result) = results[0];
        let err = result.as_ref().expect_err("expected 403 error");
        let detail = format!("{err:?}");
        for needle in must_contain {
            assert!(
                detail.contains(needle),
                "expected `{needle}` in error: {detail}"
            );
        }
        for needle in must_not_contain {
            assert!(
                !detail.contains(needle),
                "expected `{needle}` NOT in error: {detail}"
            );
        }
    }
}
