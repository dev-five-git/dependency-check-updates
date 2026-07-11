//! npm registry client for looking up package versions.

use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;

use reqwest::Client;
use serde::Deserialize;
use serde::de::{IgnoredAny, MapAccess, Visitor};
use tokio::sync::Semaphore;
use tracing::{debug, trace};

use dependency_check_updates_core::{
    DEFAULT_MAX_CONCURRENT_REQUESTS, DcuError, DependencySpec, ResolvedVersion, TargetLevel,
    build_client, current_req_is_prerelease, send_checked,
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
    versions: Option<VersionKeys>,
    /// Map of version → ISO-8601 publish time. Only present in the *full*
    /// packument (the abbreviated `install-v1` format omits it), so it is
    /// fetched on demand for `--target newest`.
    time: Option<serde_json::Map<String, serde_json::Value>>,
}

/// List of version-string keys extracted from a packument `versions` JSON
/// object. Each value body (the nested per-version metadata: `dependencies`,
/// `peerDependencies`, `dist`, ...) is walked past with `IgnoredAny` instead
/// of being materialised into a `serde_json::Value` tree, since downstream
/// code only ever needs the keys. Saves the per-version `Value`-tree
/// allocation on every npm packument parse — popular packages publish
/// hundreds of versions, each with multi-KB nested bodies. The container is a
/// `Vec<String>` (not a `HashSet`) because JSON object keys are unique by
/// spec — the downstream consumer (`extract_sorted_versions`) only iterates
/// the keys and sorts them, never probing membership, so the per-key hashing
/// cost of a `HashSet` was pure overhead.
#[derive(Debug)]
struct VersionKeys(Vec<String>);

impl<'de> Deserialize<'de> for VersionKeys {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct VersionKeysVisitor;

        impl<'de> Visitor<'de> for VersionKeysVisitor {
            type Value = Vec<String>;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a JSON object whose keys are version strings")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut keys = Vec::with_capacity(map.size_hint().unwrap_or(0));
                while let Some(key) = map.next_key::<String>()? {
                    // Skip the value body without materialising it.
                    let _: IgnoredAny = map.next_value()?;
                    keys.push(key);
                }
                Ok(keys)
            }
        }

        deserializer.deserialize_map(VersionKeysVisitor).map(Self)
    }
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
    pub fn encode_package_name(name: &str) -> Cow<'_, str> {
        if name.starts_with('@') {
            Cow::Owned(name.replacen('/', "%2F", 1))
        } else {
            Cow::Borrowed(name)
        }
    }

    /// Fetch package info from the npm registry.
    ///
    /// When `full` is true the *full* packument is requested
    /// (`Accept: application/json`) so the `time` map is present — needed for
    /// `--target newest`. Otherwise the cheaper abbreviated `install-v1`
    /// format is used, which omits `time`.
    async fn fetch_package_info(&self, name: &str, full: bool) -> Result<NpmPackageInfo, DcuError> {
        // `acquire` only errors once the semaphore is closed; this registry
        // never closes its semaphore, so success is the sole reachable path
        // (mirrors `build_client`'s infallible-by-construction `expect`).
        let _permit = self
            .semaphore
            .acquire()
            .await
            .expect("semaphore is never closed");

        let encoded = Self::encode_package_name(name);
        let url = format!("{}/{encoded}", self.base_url);

        debug!(package = name, %url, full, "fetching package info");

        let accept = if full {
            "application/json"
        } else {
            "application/vnd.npm.install-v1+json; q=1.0, application/json; q=0.8, */*"
        };

        let request = self.client.get(&url).header("Accept", accept);
        let response = send_checked(request, name).await?;

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
        let mut info = self
            .fetch_package_info(&dep.name, target == TargetLevel::Newest)
            .await?;

        let latest = info.dist_tags.take().and_then(|dt| dt.latest);

        // Detect if the user's current requirement is a prerelease. When it is,
        // we cannot use the dist-tags.latest fast path because the user may be
        // ahead of the stable `latest` tag (e.g. `2.0.0-rc.37` while
        // dist-tags.latest points at `1.1.20`), and we must consider the full
        // sorted version list to preserve the "prerelease tail" policy.
        let current_is_prerelease =
            current_req_is_prerelease::<node_semver::Version>(&dep.current_req);

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
                // Shared strip→parse→select sequence centralised in `core`;
                // npm's `latest` (dist-tags) doubles as the fallback for the
                // stable-`Latest` and unparseable-`Minor`/`Patch` cases.
                dependency_check_updates_core::parse_and_select(
                    &dep.current_req,
                    &all_versions,
                    target,
                    latest.as_deref(),
                )
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
            let t = times.get(&s).and_then(serde_json::Value::as_str)?;
            Some((t, s))
        })
        .max_by(|a, b| a.0.cmp(b.0))
        .map(|(_, s)| s)
}

/// Extract and sort all version strings from the packument (pre-releases
/// included — filtering happens later in version selection).
fn extract_sorted_versions(info: &NpmPackageInfo) -> Vec<node_semver::Version> {
    let Some(versions) = &info.versions else {
        return Vec::new();
    };

    let mut parsed: Vec<node_semver::Version> = Vec::with_capacity(versions.0.len());
    parsed.extend(
        versions
            .0
            .iter()
            .filter_map(|v| node_semver::Version::parse(v).ok()),
    );

    parsed.sort_unstable();
    parsed
}

#[cfg(test)]
// rstest's `#[from(crypto_provider)] _crypto: ()` parameter resolves the
// `crypto_provider` fixture in the macro-expanded body. The underscore is
// required to avoid `unused_variables` on the test-side binding, but pedantic
// clippy then flags the rstest-generated use as `used_underscore_binding`.
// Allow it module-wide to keep the fixture pattern clean.
#[allow(clippy::used_underscore_binding)]
mod tests {
    use super::*;
    use rstest::{fixture, rstest};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use dependency_check_updates_core::DependencySection;

    /// Install the rustls ring provider so reqwest (rustls-no-provider) works.
    /// Wrapped in an rstest fixture so every test that needs TLS can request
    /// it via a parameter; `install_default()` itself is idempotent (returns
    /// `Err` once a provider is already installed, which we swallow).
    #[fixture]
    fn crypto_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    /// Build a sorted version list from the supplied semver strings.
    fn make_versions(vers: &[&str]) -> Vec<node_semver::Version> {
        let mut v: Vec<_> = vers
            .iter()
            .filter_map(|s| node_semver::Version::parse(s).ok())
            .collect();
        v.sort();
        v
    }

    /// Build the abbreviated npm packument body returned by the mock server.
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

    /// Build a single-dep spec in the `Dependencies` section.
    fn dep(name: &str, current_req: &str) -> DependencySpec {
        DependencySpec {
            name: name.to_owned(),
            current_req: current_req.to_owned(),
            section: DependencySection::Dependencies,
            path_version: None,
        }
    }

    /// Mount a single mock route returning `body` for `GET <mock_path>`.
    async fn mount_get(server: &MockServer, mock_path: &str, body: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path(mock_path))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    #[rstest]
    #[case::regular("react", "react")]
    #[case::scoped_types("@types/react", "@types%2Freact")]
    #[case::scoped_babel("@babel/core", "@babel%2Fcore")]
    fn encode_package_name_cases(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(NpmRegistry::encode_package_name(input).as_ref(), expected);
    }

    #[rstest]
    // current_req, dist-tags.latest, available versions, target, expected selection.
    // Every case asserts `result == expected` against `select_version`.
    #[case::latest("^18.0.0", "18.2.0", &["18.0.0", "18.1.0", "18.2.0"], TargetLevel::Latest, Some("18.2.0"))]
    #[case::minor("^18.0.0", "19.0.0", &["18.0.0", "18.1.0", "18.2.0", "19.0.0"], TargetLevel::Minor, Some("18.2.0"))]
    #[case::patch("^18.0.0", "18.2.0", &["18.0.0", "18.0.1", "18.0.2", "18.1.0", "18.2.0"], TargetLevel::Patch, Some("18.0.2"))]
    #[case::greatest("^17.0.0", "18.2.0", &["17.0.0", "18.0.0", "18.2.0", "19.0.0"], TargetLevel::Greatest, Some("19.0.0"))]
    // README guarantees `greatest` (and aliased `newest`) include prereleases.
    #[case::greatest_includes_prerelease("^18.0.0", "18.2.0", &["18.0.0", "18.2.0", "19.0.0-beta.1"], TargetLevel::Greatest, Some("19.0.0-beta.1"))]
    #[case::newest_includes_prerelease("^18.0.0", "18.2.0", &["18.0.0", "18.2.0", "19.0.0-beta.1"], TargetLevel::Newest, Some("19.0.0-beta.1"))]
    // Stable current → Latest excludes prereleases by routing through dist-tags.latest.
    #[case::latest_stable_current_excludes_prerelease("^18.0.0", "18.2.0", &["18.0.0", "18.2.0", "19.0.0-beta.1"], TargetLevel::Latest, Some("18.2.0"))]
    // Prerelease tail: higher prerelease of the same train wins.
    #[case::latest_prerelease_tail_same_train("4.0.0-beta.1", "3.5.0", &["3.5.0", "4.0.0-beta.1", "4.0.0-beta.3"], TargetLevel::Latest, Some("4.0.0-beta.3"))]
    // Prerelease tail: same-train stable wins over the prerelease.
    #[case::latest_prerelease_tail_jumps_to_stable("4.0.0-beta.1", "3.5.0", &["3.5.0", "4.0.0-beta.3", "4.0.0"], TargetLevel::Latest, Some("4.0.0"))]
    // Prerelease tail: higher-major stable beats the prerelease, regardless of train.
    #[case::latest_prerelease_picks_higher_stable_major("4.0.0-beta.1", "5.0.0", &["3.5.0", "4.0.0-beta.1", "5.0.0"], TargetLevel::Latest, Some("5.0.0"))]
    // Prerelease tail: stable still wins over an unrelated prerelease at the same major.
    #[case::latest_prerelease_stable_wins_over_unrelated_prerelease("4.0.0-beta.1", "5.0.0", &["3.5.0", "4.0.0-beta.1", "5.0.0-alpha.1", "5.0.0"], TargetLevel::Latest, Some("5.0.0"))]
    // Empty version list → fall back to dist-tags.latest.
    #[case::empty_versions_falls_back_to_latest("^1.0.0", "1.0.0", &[], TargetLevel::Greatest, Some("1.0.0"))]
    // Minor + no major-5 version → None.
    #[case::minor_no_match("^5.0.0", "6.0.0", &["6.0.0", "7.0.0"], TargetLevel::Minor, None)]
    // Unparseable current (`*`) falls back to dist-tags.latest on Minor & Patch.
    #[case::minor_unparseable_falls_back_to_latest("*", "2.0.0", &["1.0.0", "2.0.0"], TargetLevel::Minor, Some("2.0.0"))]
    #[case::patch_unparseable_falls_back_to_latest("*", "2.0.0", &["1.0.0", "2.0.0"], TargetLevel::Patch, Some("2.0.0"))]
    // Stable current must reject prerelease candidates on Minor & Patch.
    #[case::minor_skips_prerelease_when_current_stable("^18.0.0", "18.2.0", &["18.0.0", "18.1.0", "18.2.0", "18.3.0-beta.1"], TargetLevel::Minor, Some("18.2.0"))]
    #[case::patch_skips_prerelease_when_current_stable("=18.0.0", "18.0.5", &["18.0.0", "18.0.1", "18.0.5", "18.0.6-rc.1"], TargetLevel::Patch, Some("18.0.5"))]
    fn select_version_cases(
        #[case] current_req: &str,
        #[case] latest_str: &str,
        #[case] versions: &[&str],
        #[case] target: TargetLevel,
        #[case] expected: Option<&str>,
    ) {
        let versions = make_versions(versions);
        // Drives the same algorithm the registry now calls directly: the
        // ecosystem-agnostic helper in `core` that fuses strip→parse→select.
        let got = dependency_check_updates_core::parse_and_select::<node_semver::Version>(
            current_req,
            &versions,
            target,
            Some(latest_str),
        );
        assert_eq!(got, expected.map(ToOwned::to_owned));
    }

    #[test]
    fn test_latest_prerelease_tail_ignores_unrelated_prereleases() {
        // Current: 4.0.0-beta.1. Unrelated 5.0.0-alpha.1 must NOT be selected.
        // Kept separate because its assertion is `assert_ne!`, not `assert_eq!`,
        // and rstest parametrization would obscure that distinction.
        let versions = make_versions(&["3.5.0", "4.0.0-beta.1", "5.0.0-alpha.1"]);
        let result = dependency_check_updates_core::parse_and_select::<node_semver::Version>(
            "4.0.0-beta.1",
            &versions,
            TargetLevel::Latest,
            Some("3.5.0"),
        );
        assert_ne!(result, Some("5.0.0-alpha.1".to_owned()));
    }

    #[test]
    fn test_extract_sorted_versions() {
        let info = NpmPackageInfo {
            dist_tags: None,
            versions: Some(VersionKeys(vec![
                "2.0.0".to_owned(),
                "1.0.0".to_owned(),
                "1.5.0".to_owned(),
            ])),
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

    #[rstest]
    // Both `NpmRegistry::new()` and `NpmRegistry::default()` must construct
    // a usable client without panicking. `_crypto` ensures the rustls provider
    // is installed first; `#[allow(unused)]` since the body just verifies
    // construction.
    fn registry_constructors_do_not_panic(#[from(crypto_provider)] _crypto: ()) {
        let _new = NpmRegistry::new();
        let _default = NpmRegistry::default();
    }

    /// Outcome of a `resolve_version` call against the wiremock server. Keeps
    /// `clippy::type_complexity` happy on the `#[rstest]` row tuples.
    type ExpectedResolution = (Option<&'static str>, Option<&'static str>);

    #[rstest]
    // Each row mounts a single `GET <mock_path>` → npm packument body, then
    // calls `resolve_version` with the given current_req+target and asserts
    // on `(latest, selected)`. `None` in either slot means "skip that assert".
    #[case::latest(
        "react",
        "/react",
        npm_response_body("18.2.0", &["17.0.0", "18.0.0", "18.2.0", "19.0.0-beta.1"]),
        "^17.0.0",
        TargetLevel::Latest,
        (Some("18.2.0"), Some("18.2.0")),
    )]
    #[case::minor(
        "react",
        "/react",
        npm_response_body("19.0.0", &["17.0.0", "17.1.0", "17.2.0", "18.0.0", "18.2.0", "19.0.0"]),
        "~17.0.0",
        TargetLevel::Minor,
        (None, Some("17.2.0")),
    )]
    #[case::patch(
        "react",
        "/react",
        npm_response_body("18.2.0", &["17.0.0", "17.0.1", "17.0.2", "17.1.0", "18.0.0", "18.2.0"]),
        "=17.0.0",
        TargetLevel::Patch,
        (None, Some("17.0.2")),
    )]
    #[case::already_satisfied(
        "react",
        "/react",
        npm_response_body("17.0.1", &["17.0.0", "17.0.1"]),
        "^17.0.0",
        TargetLevel::Latest,
        (None, Some("17.0.1")),
    )]
    #[case::scoped_package(
        "@types/react",
        "/@types%2Freact",
        npm_response_body("18.2.0", &["17.0.0", "18.0.0", "18.2.0"]),
        "^17.0.0",
        TargetLevel::Latest,
        (Some("18.2.0"), Some("18.2.0")),
    )]
    #[tokio::test]
    async fn resolve_version_against_mock(
        #[from(crypto_provider)] _crypto: (),
        #[case] dep_name: &str,
        #[case] mock_path: &str,
        #[case] body: serde_json::Value,
        #[case] current_req: &str,
        #[case] target: TargetLevel,
        #[case] expected: ExpectedResolution,
    ) {
        let mock_server = MockServer::start().await;
        mount_get(&mock_server, mock_path, body).await;

        let registry = NpmRegistry::with_base_url(&mock_server.uri());
        let result = registry
            .resolve_version(&dep(dep_name, current_req), target)
            .await
            .unwrap();

        let (expected_latest, expected_selected) = expected;
        if let Some(latest) = expected_latest {
            assert_eq!(result.latest.as_deref(), Some(latest));
        }
        if let Some(selected) = expected_selected {
            assert_eq!(result.selected.as_deref(), Some(selected));
        }
    }

    #[rstest]
    #[tokio::test]
    async fn test_resolve_version_invalid_json_body(#[from(crypto_provider)] _crypto: ()) {
        // 200 OK with a non-JSON body forces the `.json().await` parse closure
        // in `fetch_package_info` to fire — covers the parse-error branch.
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/pkg"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&mock_server)
            .await;

        let registry = NpmRegistry::with_base_url(&mock_server.uri());
        let result = registry
            .resolve_version(&dep("pkg", "^1.0.0"), TargetLevel::Latest)
            .await;
        assert!(result.is_err());
        let err_str = result.unwrap_err().to_string();
        assert!(
            err_str.contains("failed to parse response") || err_str.contains("pkg"),
            "unexpected error: {err_str}"
        );
    }

    #[rstest]
    #[tokio::test]
    async fn test_resolve_version_network_error(#[from(crypto_provider)] _crypto: ()) {
        // Port 1 is reserved (tcpmux) and effectively always refuses connections
        // on a developer machine — forces the `.send().await` network-error
        // closure in `fetch_package_info` to fire.
        let registry = NpmRegistry::with_base_url("http://127.0.0.1:1");
        let result = registry
            .resolve_version(&dep("pkg", "^1.0.0"), TargetLevel::Latest)
            .await;
        assert!(result.is_err());
        let err_str = result.unwrap_err().to_string();
        assert!(
            err_str.contains("pkg"),
            "expected package name in error: {err_str}"
        );
    }

    #[rstest]
    #[tokio::test]
    async fn test_resolve_version_404(#[from(crypto_provider)] _crypto: ()) {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/nonexistent-pkg"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        let registry = NpmRegistry::with_base_url(&mock_server.uri());
        let result = registry
            .resolve_version(&dep("nonexistent-pkg", "^1.0.0"), TargetLevel::Latest)
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        let err_str = err.to_string();
        assert!(
            err_str.contains("404") || err_str.contains("nonexistent-pkg"),
            "unexpected error: {err_str}"
        );
    }

    #[rstest]
    #[tokio::test]
    async fn test_resolve_batch_concurrent(#[from(crypto_provider)] _crypto: ()) {
        let mock_server = MockServer::start().await;

        mount_get(
            &mock_server,
            "/react",
            npm_response_body("18.2.0", &["17.0.0", "18.0.0", "18.2.0"]),
        )
        .await;
        mount_get(
            &mock_server,
            "/lodash",
            npm_response_body("4.17.21", &["4.0.0", "4.17.0", "4.17.21"]),
        )
        .await;
        mount_get(
            &mock_server,
            "/axios",
            npm_response_body("1.6.0", &["0.27.0", "1.0.0", "1.6.0"]),
        )
        .await;

        let registry = NpmRegistry::with_base_url(&mock_server.uri());
        let deps = vec![
            dep("react", "^17.0.0"),
            dep("lodash", "^4.0.0"),
            dep("axios", "^0.27.0"),
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

    #[rstest]
    #[tokio::test]
    async fn test_resolve_version_latest_fast_path(#[from(crypto_provider)] _crypto: ()) {
        let mock_server = MockServer::start().await;
        // Provide versions that include pre-releases; Latest fast path should return
        // dist-tags.latest directly without filtering pre-releases from version list.
        mount_get(
            &mock_server,
            "/react",
            npm_response_body(
                "18.2.0",
                &["18.0.0", "18.2.0", "19.0.0-beta.1", "19.0.0-rc.1"],
            ),
        )
        .await;

        let registry = NpmRegistry::with_base_url(&mock_server.uri());
        let result = registry
            .resolve_version(&dep("react", "^17.0.0"), TargetLevel::Latest)
            .await
            .unwrap();
        // Fast path: should return exactly dist-tags.latest, not the highest version
        assert_eq!(result.latest.as_deref(), Some("18.2.0"));
        assert_eq!(result.selected.as_deref(), Some("18.2.0"));
        // Confirm it did NOT pick 19.0.0-rc.1 (which would happen if fast path was skipped)
        assert_ne!(result.selected.as_deref(), Some("19.0.0-rc.1"));
    }

    #[rstest]
    #[tokio::test]
    async fn test_resolve_version_newest_by_date(#[from(crypto_provider)] _crypto: ()) {
        let mock_server = MockServer::start().await;
        // 1.5.0 was published most recently even though 2.0.0 is higher. The
        // `time` map (full packument) drives `newest`; `greatest` would differ.
        mount_get(
            &mock_server,
            "/pkg",
            serde_json::json!({
                "dist-tags": { "latest": "2.0.0" },
                "versions": { "1.0.0": {}, "1.5.0": {}, "2.0.0": {} },
                "time": {
                    "created": "2021-01-01T00:00:00Z",
                    "1.0.0": "2022-01-01T00:00:00Z",
                    "2.0.0": "2023-01-01T00:00:00Z",
                    "1.5.0": "2024-06-01T00:00:00Z"
                }
            }),
        )
        .await;

        let registry = NpmRegistry::with_base_url(&mock_server.uri());
        let result = registry
            .resolve_version(&dep("pkg", "^1.0.0"), TargetLevel::Newest)
            .await
            .unwrap();
        assert_eq!(result.selected.as_deref(), Some("1.5.0"));
    }

    #[rstest]
    #[tokio::test]
    async fn test_resolve_version_non_latest_with_tracing(#[from(crypto_provider)] _crypto: ()) {
        // Install a trace-level subscriber so trace!() arguments are evaluated.
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_test_writer()
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let mock_server = MockServer::start().await;
        mount_get(
            &mock_server,
            "/react",
            npm_response_body("19.0.0", &["17.0.0", "18.0.0", "19.0.0"]),
        )
        .await;

        let registry = NpmRegistry::with_base_url(&mock_server.uri());
        let result = registry
            .resolve_version(&dep("react", "~17.0.0"), TargetLevel::Greatest)
            .await
            .unwrap();
        assert_eq!(result.selected.as_deref(), Some("19.0.0"));
    }
}
