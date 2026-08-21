//! OCI Distribution (Registry HTTP API v2) client for resolving image tags.
//!
//! Fetches `GET https://<host>/v2/<repository>/tags/list` and groups the
//! returned names by build variant (see [`crate::tag`]). One code path serves
//! every registry — Docker Hub, `ghcr.io`, `quay.io`, `mcr.microsoft.com`,
//! `public.ecr.aws`, a self-hosted `localhost:5000` — because they all
//! implement the same specification.
//!
//! ## Anonymous authentication
//!
//! Public images still require a token. The registry answers an unauthenticated
//! request with `401` plus a `WWW-Authenticate: Bearer realm=…,service=…`
//! challenge; fetching that realm with a `repository:<repo>:pull` scope yields
//! a short-lived token that grants read access without any credentials. This
//! client performs that exchange transparently and retries once.
//!
//! ## Batching
//!
//! Every unique repository is fetched exactly once per batch, so a Compose file
//! referencing `postgres:16` and `postgres:16-alpine` costs one round-trip
//! (plus its token exchange), not two.
//!
//! ## Pagination
//!
//! `tags/list` is requested without an `n` parameter, which every mainstream
//! registry answers with the complete tag list in one response. Deliberately
//! not following `Link` headers keeps request counts predictable during deep
//! scans, at the cost of missing tags on registries that impose a default page
//! size — the same trade-off the GitHub Tags API client makes.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use reqwest::{Client, StatusCode};
use serde::Deserialize;
use tokio::sync::Semaphore;
use tracing::{debug, trace};

use dependency_check_updates_core::{
    DEFAULT_MAX_CONCURRENT_REQUESTS, DcuError, DependencySpec, ResolvedVersion, TargetLevel,
    build_client,
};

use crate::image::ImageRef;
use crate::tag::PreparedTags;

/// Response body of `GET /v2/<repository>/tags/list`.
#[derive(Debug, Deserialize)]
struct TagsResponse {
    /// Registries return `null` rather than `[]` for a repository that exists
    /// but has no tags.
    tags: Option<Vec<String>>,
}

/// Response body of a token-realm exchange.
///
/// The specification names the field `token`; Docker Hub and several others
/// also emit the `OAuth2` spelling `access_token`. Accepting both keeps the
/// client working across every registry seen in practice.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
}

impl TokenResponse {
    fn into_token(self) -> Option<String> {
        self.token.or(self.access_token)
    }
}

/// A parsed `WWW-Authenticate: Bearer …` challenge.
#[derive(Debug, PartialEq, Eq)]
struct BearerChallenge {
    realm: String,
    service: Option<String>,
    scope: Option<String>,
}

/// OCI Distribution registry client.
#[derive(Clone)]
pub struct DockerRegistry {
    client: Client,
    semaphore: Arc<Semaphore>,
    /// When set, every request targets this base instead of the `https://<host>`
    /// derived from the image reference. Tests point it at a mock server.
    base_url_override: Option<Arc<str>>,
}

impl DockerRegistry {
    /// Construct a client that talks to whichever registry each image names.
    #[must_use]
    pub fn new() -> Self {
        Self::build(None)
    }

    /// Construct a client that routes every request to `base_url`, ignoring the
    /// host encoded in the image reference. Used by tests via `wiremock`.
    #[must_use]
    pub fn with_base_url(base_url: &str) -> Self {
        Self::build(Some(Arc::from(base_url.trim_end_matches('/'))))
    }

    fn build(base_url_override: Option<Arc<str>>) -> Self {
        Self {
            client: build_client(),
            semaphore: Arc::new(Semaphore::new(DEFAULT_MAX_CONCURRENT_REQUESTS)),
            base_url_override,
        }
    }

    /// The scheme + authority to send registry requests to.
    ///
    /// A bare `localhost` / `127.0.0.1` registry is assumed to be plain HTTP,
    /// matching the Docker daemon's own default insecure-registry rule;
    /// everything else is HTTPS.
    fn endpoint(&self, host: &str) -> String {
        if let Some(base) = &self.base_url_override {
            return base.to_string();
        }
        let scheme = if host == "localhost"
            || host.starts_with("localhost:")
            || host.starts_with("127.0.0.1")
        {
            "http"
        } else {
            "https"
        };
        format!("{scheme}://{host}")
    }

    /// Fetch every published tag of `repository` on `host`.
    async fn fetch_tags(&self, host: &str, repository: &str) -> Result<Vec<String>, String> {
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|e| format!("semaphore error: {e}"))?;

        let url = format!("{}/v2/{repository}/tags/list", self.endpoint(host));
        debug!(host, repository, %url, "fetching tags");

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        // Public images still need a token; the registry says so with a 401
        // plus the challenge describing where to get one.
        let response = if response.status() == StatusCode::UNAUTHORIZED {
            let challenge = response
                .headers()
                .get(reqwest::header::WWW_AUTHENTICATE)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_bearer_challenge)
                .ok_or_else(|| {
                    "HTTP 401 without a usable Bearer challenge — the registry requires \
                     credentials this tool cannot supply anonymously."
                        .to_owned()
                })?;

            let token = self.fetch_token(&challenge, repository).await?;
            self.client
                .get(&url)
                .bearer_auth(token)
                .send()
                .await
                .map_err(|e| e.to_string())?
        } else {
            response
        };

        let status = response.status();
        if !status.is_success() {
            return Err(describe_failure(status));
        }

        let body: TagsResponse = response.json().await.map_err(|e| format!("parse: {e}"))?;
        Ok(body.tags.unwrap_or_default())
    }

    /// Exchange a Bearer challenge for a pull-scoped token.
    async fn fetch_token(
        &self,
        challenge: &BearerChallenge,
        repository: &str,
    ) -> Result<String, String> {
        // Registries echo the exact scope they want in the challenge; fall back
        // to the read-only scope the specification defines when they do not.
        let scope = challenge
            .scope
            .clone()
            .unwrap_or_else(|| format!("repository:{repository}:pull"));

        let url = token_url(&challenge.realm, &scope, challenge.service.as_deref());
        trace!(%url, "exchanging bearer challenge for a token");

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("token request failed: HTTP {status}"));
        }

        response
            .json::<TokenResponse>()
            .await
            .map_err(|e| format!("token parse: {e}"))?
            .into_token()
            .ok_or_else(|| "token response carried neither `token` nor `access_token`".to_owned())
    }

    /// Resolve every dep in `deps`, fetching each unique repository once.
    pub async fn resolve_batch(
        &self,
        deps: &[DependencySpec],
        target: TargetLevel,
    ) -> Vec<(usize, Result<ResolvedVersion, DcuError>)> {
        // Step 1: collect the unique (host, repository) pairs. Two deps that
        // differ only by variant (`postgres:16` / `postgres:16-alpine`) share
        // one repository and therefore one fetch.
        let mut seen: HashSet<String> = HashSet::with_capacity(deps.len());
        let mut unique: Vec<(String, String)> = Vec::with_capacity(deps.len());
        for dep in deps {
            let Some((host, repository)) = registry_target_of(&dep.name) else {
                continue;
            };
            let key = cache_key(&host, &repository);
            if seen.insert(key) {
                unique.push((host, repository));
            }
        }

        // Step 2: fan out the fetches.
        let fetched =
            futures::future::join_all(unique.into_iter().map(|(host, repository)| async move {
                let result = self.fetch_tags(&host, &repository).await;
                (cache_key(&host, &repository), result)
            }))
            .await;

        let mut prepared: HashMap<String, Result<PreparedTags, String>> =
            HashMap::with_capacity(fetched.len());
        for (key, result) in fetched {
            prepared.insert(key, result.map(|tags| PreparedTags::new(&tags)));
        }

        // Step 3: resolve each dep against its repository's prepared tag groups.
        let mut results = Vec::with_capacity(deps.len());
        for (idx, dep) in deps.iter().enumerate() {
            let resolved = match registry_target_of(&dep.name) {
                None => Err(DcuError::RegistryLookup {
                    package: dep.name.clone(),
                    detail: "not a valid image reference".to_owned(),
                }),
                // Safe by construction: every parseable name produced a key in
                // step 1, and step 2 inserted one entry per key.
                Some((host, repository)) => match prepared.get(&cache_key(&host, &repository)) {
                    None => Err(DcuError::RegistryLookup {
                        package: dep.name.clone(),
                        detail: "tag cache miss".to_owned(),
                    }),
                    Some(Err(detail)) => Err(DcuError::RegistryLookup {
                        package: dep.name.clone(),
                        detail: detail.clone(),
                    }),
                    Some(Ok(tags)) => {
                        let resolved = tags.select(&dep.current_req, target);
                        trace!(
                            image = %dep.name,
                            current = %dep.current_req,
                            selected = ?resolved.selected,
                            "resolved tag"
                        );
                        Ok(resolved)
                    }
                },
            };
            results.push((idx, resolved));
        }

        results
    }
}

impl Default for DockerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve an image name to the `(host, repository)` pair to query.
fn registry_target_of(name: &str) -> Option<(String, String)> {
    let parsed = ImageRef::parse(name)?;
    let (host, repository) = parsed.registry_target();
    Some((host.to_owned(), repository))
}

/// Cache key for one repository on one registry.
fn cache_key(host: &str, repository: &str) -> String {
    format!("{host}/{repository}")
}

/// Build the token-realm URL for a pull-scoped exchange.
///
/// The parameter values are appended verbatim. Every byte that appears in a
/// registry scope (`repository:library/node:pull,push`) or a service name is
/// already legal in a query component per RFC 3986 — `:`, `/`, and `,` are all
/// `pchar` / sub-delims — which is why the Docker CLI sends them unescaped
/// too. A realm that already carries its own query string is respected.
fn token_url(realm: &str, scope: &str, service: Option<&str>) -> String {
    let separator = if realm.contains('?') { '&' } else { '?' };
    let mut url = format!("{realm}{separator}scope={scope}");
    if let Some(service) = service {
        url.push_str("&service=");
        url.push_str(service);
    }
    url
}

/// Turn a non-success status into a message that names the fix where one
/// exists.
///
/// Docker Hub throttles anonymous clients aggressively enough that a deep scan
/// of a large monorepo can hit the ceiling, and the bare status code gives the
/// user nothing to act on.
fn describe_failure(status: StatusCode) -> String {
    match status {
        StatusCode::TOO_MANY_REQUESTS => "registry rate limit exceeded — retry later, or \
             authenticate with the registry to raise the anonymous quota."
            .to_owned(),
        StatusCode::NOT_FOUND => {
            "repository not found — private registries and locally built images \
             cannot be resolved anonymously."
                .to_owned()
        }
        other => format!("HTTP {other}"),
    }
}

/// Parse a `WWW-Authenticate` header into its Bearer challenge parameters.
///
/// Returns `None` for a non-Bearer scheme or a challenge with no `realm`,
/// since neither can be turned into a token request.
fn parse_bearer_challenge(header: &str) -> Option<BearerChallenge> {
    let params = header.strip_prefix("Bearer ").or_else(|| {
        // The scheme token is case-insensitive per RFC 7235.
        header
            .split_once(char::is_whitespace)
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"))
            .map(|(_, rest)| rest)
    })?;

    let mut realm = None;
    let mut service = None;
    let mut scope = None;
    for (key, value) in parse_auth_params(params) {
        match key {
            "realm" => realm = Some(value.to_owned()),
            "service" => service = Some(value.to_owned()),
            "scope" => scope = Some(value.to_owned()),
            _ => {}
        }
    }

    Some(BearerChallenge {
        realm: realm?,
        service,
        scope,
    })
}

/// Split `key="value"` / `key=value` auth parameters, honouring quotes.
///
/// A naive `split(',')` is wrong here: a scope legitimately contains commas
/// (`scope="repository:app:pull,push"`), and they sit inside the quoted value.
fn parse_auth_params(params: &str) -> Vec<(&str, &str)> {
    let bytes = params.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        while i < bytes.len() && (bytes[i] == b',' || bytes[i].is_ascii_whitespace()) {
            i += 1;
        }
        let key_start = i;
        while i < bytes.len() && bytes[i] != b'=' && bytes[i] != b',' {
            i += 1;
        }
        // A trailing token with no `=` is not a parameter.
        if i >= bytes.len() || bytes[i] != b'=' {
            break;
        }
        let key = params[key_start..i].trim();
        i += 1;

        let value = if bytes.get(i) == Some(&b'"') {
            i += 1;
            let value_start = i;
            while i < bytes.len() && bytes[i] != b'"' {
                i += 1;
            }
            let value = &params[value_start..i];
            // Step past the closing quote when one is present.
            i = (i + 1).min(bytes.len());
            value
        } else {
            let value_start = i;
            while i < bytes.len() && bytes[i] != b',' {
                i += 1;
            }
            params[value_start..i].trim()
        };

        out.push((key, value));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use dependency_check_updates_core::DependencySection;
    use rstest::rstest;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path as match_path, query_param},
    };

    /// Idempotent rustls provider install. Returning `Err` (already set) is the
    /// expected steady state once any test has run — `let _ =` swallows it.
    fn install_crypto_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    fn dep(name: &str, tag: &str) -> DependencySpec {
        DependencySpec {
            name: name.to_owned(),
            current_req: tag.to_owned(),
            section: DependencySection::DockerImage,
            path_version: None,
        }
    }

    fn tags_body(names: &[&str]) -> serde_json::Value {
        serde_json::json!({ "name": "test", "tags": names })
    }

    #[rstest]
    // The canonical Docker Hub challenge.
    #[case::docker_hub(
        r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io""#,
        Some(("https://auth.docker.io/token", Some("registry.docker.io"), None))
    )]
    // ghcr.io echoes the scope it wants.
    #[case::with_scope(
        r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:org/app:pull""#,
        Some((
            "https://ghcr.io/token",
            Some("ghcr.io"),
            Some("repository:org/app:pull")
        ))
    )]
    // A scope containing a comma must survive intact — the reason the param
    // splitter is quote-aware rather than a `split(',')`.
    #[case::comma_inside_quoted_scope(
        r#"Bearer realm="https://auth.example.com/token",scope="repository:app:pull,push""#,
        Some((
            "https://auth.example.com/token",
            None,
            Some("repository:app:pull,push")
        ))
    )]
    // RFC 7235 makes the scheme token case-insensitive.
    #[case::lowercase_scheme(
        r#"bearer realm="https://auth.example.com/token""#,
        Some(("https://auth.example.com/token", None, None))
    )]
    // Unusable challenges.
    #[case::basic_scheme(r#"Basic realm="registry""#, None)]
    #[case::missing_realm(r#"Bearer service="registry.docker.io""#, None)]
    #[case::empty("", None)]
    fn parse_bearer_challenge_cases(
        #[case] header: &str,
        #[case] expected: Option<(&str, Option<&str>, Option<&str>)>,
    ) {
        let parsed = parse_bearer_challenge(header);
        assert_eq!(
            parsed,
            expected.map(|(realm, service, scope)| BearerChallenge {
                realm: realm.to_owned(),
                service: service.map(ToOwned::to_owned),
                scope: scope.map(ToOwned::to_owned),
            })
        );
    }

    #[rstest]
    #[case::unquoted("a=1,b=2", &[("a", "1"), ("b", "2")])]
    #[case::spaced("a = 1, b = 2", &[("a", "1"), ("b", "2")])]
    #[case::unterminated_quote(r#"a="1"#, &[("a", "1")])]
    #[case::trailing_token_without_equals("a=1,junk", &[("a", "1")])]
    #[case::empty("", &[])]
    fn parse_auth_params_cases(#[case] input: &str, #[case] expected: &[(&str, &str)]) {
        assert_eq!(parse_auth_params(input), expected);
    }

    #[rstest]
    // Scope characters (`:`, `/`, `,`) are legal query bytes and travel raw,
    // exactly as the Docker CLI sends them.
    #[case::basic(
        "https://auth.docker.io/token",
        "repository:library/node:pull",
        Some("registry.docker.io"),
        "https://auth.docker.io/token?scope=repository:library/node:pull&service=registry.docker.io"
    )]
    #[case::no_service(
        "https://auth.example.com/token",
        "repository:app:pull,push",
        None,
        "https://auth.example.com/token?scope=repository:app:pull,push"
    )]
    // A realm that already carries a query string keeps it.
    #[case::realm_with_existing_query(
        "https://auth.example.com/token?account=ci",
        "repository:app:pull",
        None,
        "https://auth.example.com/token?account=ci&scope=repository:app:pull"
    )]
    fn token_url_cases(
        #[case] realm: &str,
        #[case] scope: &str,
        #[case] service: Option<&str>,
        #[case] expected: &str,
    ) {
        assert_eq!(token_url(realm, scope, service), expected);
    }

    #[rstest]
    #[case::hub("node", "registry-1.docker.io/library/node")]
    #[case::namespaced("grafana/grafana", "registry-1.docker.io/grafana/grafana")]
    #[case::ghcr("ghcr.io/org/app", "ghcr.io/org/app")]
    fn registry_target_of_cases(#[case] name: &str, #[case] expected_key: &str) {
        let (host, repository) = registry_target_of(name).expect("name must resolve");
        assert_eq!(cache_key(&host, &repository), expected_key);
    }

    #[test]
    fn endpoint_uses_plain_http_for_local_registries_only() {
        let registry = DockerRegistry::new();
        assert_eq!(registry.endpoint("localhost:5000"), "http://localhost:5000");
        assert_eq!(registry.endpoint("127.0.0.1:5000"), "http://127.0.0.1:5000");
        assert_eq!(registry.endpoint("ghcr.io"), "https://ghcr.io");
    }

    #[test]
    fn new_and_default_construct() {
        install_crypto_provider();
        let _ = DockerRegistry::new();
        let _ = DockerRegistry::default();
    }

    #[tokio::test]
    async fn resolve_batch_fetches_each_repository_once() {
        install_crypto_provider();
        let mock = MockServer::start().await;

        // Two deps share `library/node`, differing only by variant — exactly
        // one HTTP call must be issued for them.
        Mock::given(method("GET"))
            .and(match_path("/v2/library/node/tags/list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(tags_body(&[
                "20",
                "20-alpine",
                "22",
                "22-alpine",
            ])))
            .expect(1)
            .mount(&mock)
            .await;

        let registry = DockerRegistry::with_base_url(&mock.uri());
        let deps = vec![dep("node", "20"), dep("node", "20-alpine")];
        let results = registry.resolve_batch(&deps, TargetLevel::Latest).await;

        assert_eq!(results.len(), 2);
        // Each dep resolves within its OWN variant group.
        assert_eq!(
            results[0].1.as_ref().unwrap().selected.as_deref(),
            Some("22")
        );
        assert_eq!(
            results[1].1.as_ref().unwrap().selected.as_deref(),
            Some("22-alpine")
        );
    }

    #[tokio::test]
    async fn resolve_batch_performs_the_anonymous_token_exchange() {
        install_crypto_provider();
        let mock = MockServer::start().await;
        let realm = format!("{}/token", mock.uri());

        // Unauthenticated request → 401 with a Bearer challenge pointing at the
        // mock's own token realm.
        Mock::given(method("GET"))
            .and(match_path("/v2/library/redis/tags/list"))
            .and(header("authorization", "Bearer issued-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(tags_body(&["7.2", "7.4"])))
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("GET"))
            .and(match_path("/token"))
            .and(query_param("scope", "repository:library/redis:pull"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "token": "issued-token" })),
            )
            .expect(1)
            .mount(&mock)
            .await;

        // Lowest priority: the unauthenticated first attempt.
        Mock::given(method("GET"))
            .and(match_path("/v2/library/redis/tags/list"))
            .respond_with(ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!(r#"Bearer realm="{realm}",service="mock""#).as_str(),
            ))
            .mount(&mock)
            .await;

        let registry = DockerRegistry::with_base_url(&mock.uri());
        let results = registry
            .resolve_batch(&[dep("redis", "7.2")], TargetLevel::Latest)
            .await;

        let resolved = results[0]
            .1
            .as_ref()
            .expect("token exchange should succeed");
        assert_eq!(resolved.selected.as_deref(), Some("7.4"));
    }

    #[tokio::test]
    async fn resolve_batch_accepts_the_oauth2_access_token_spelling() {
        install_crypto_provider();
        let mock = MockServer::start().await;
        let realm = format!("{}/token", mock.uri());

        Mock::given(method("GET"))
            .and(match_path("/v2/library/redis/tags/list"))
            .and(header("authorization", "Bearer oauth-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(tags_body(&["7.2", "7.4"])))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(match_path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "access_token": "oauth-token" })),
            )
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(match_path("/v2/library/redis/tags/list"))
            .respond_with(ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!(r#"Bearer realm="{realm}""#).as_str(),
            ))
            .mount(&mock)
            .await;

        let registry = DockerRegistry::with_base_url(&mock.uri());
        let results = registry
            .resolve_batch(&[dep("redis", "7.2")], TargetLevel::Latest)
            .await;
        assert_eq!(
            results[0].1.as_ref().unwrap().selected.as_deref(),
            Some("7.4")
        );
    }

    #[tokio::test]
    async fn resolve_batch_tolerates_a_null_tag_list() {
        install_crypto_provider();
        let mock = MockServer::start().await;

        // A repository that exists but has no tags returns `"tags": null`.
        Mock::given(method("GET"))
            .and(match_path("/v2/library/empty/tags/list"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "name": "empty", "tags": null })),
            )
            .mount(&mock)
            .await;

        let registry = DockerRegistry::with_base_url(&mock.uri());
        let results = registry
            .resolve_batch(&[dep("empty", "1.0")], TargetLevel::Latest)
            .await;
        let resolved = results[0]
            .1
            .as_ref()
            .expect("empty tag list is not an error");
        assert_eq!(resolved.selected, None);
    }

    #[rstest]
    // A 429 must name the fix rather than surfacing a bare status code.
    #[case::rate_limited(429, &["rate limit"])]
    // A 404 must explain why a private / locally built image cannot resolve.
    #[case::not_found(404, &["not found", "locally built"])]
    // Anything else keeps the status code.
    #[case::server_error(500, &["HTTP 500"])]
    #[tokio::test]
    async fn resolve_batch_reports_actionable_failures(
        #[case] status: u16,
        #[case] must_contain: &[&str],
    ) {
        install_crypto_provider();
        let mock = MockServer::start().await;

        Mock::given(method("GET"))
            .and(match_path("/v2/library/node/tags/list"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&mock)
            .await;

        let registry = DockerRegistry::with_base_url(&mock.uri());
        let results = registry
            .resolve_batch(&[dep("node", "20")], TargetLevel::Latest)
            .await;

        let error = results[0].1.as_ref().expect_err("non-2xx must be an error");
        let detail = format!("{error:?}");
        for needle in must_contain {
            assert!(
                detail.contains(needle),
                "expected `{needle}` in error: {detail}"
            );
        }
    }

    #[tokio::test]
    async fn resolve_batch_errors_when_a_401_carries_no_usable_challenge() {
        install_crypto_provider();
        let mock = MockServer::start().await;

        // A 401 with a Basic challenge cannot be satisfied anonymously.
        Mock::given(method("GET"))
            .and(match_path("/v2/library/node/tags/list"))
            .respond_with(
                ResponseTemplate::new(401)
                    .insert_header("WWW-Authenticate", r#"Basic realm="registry""#),
            )
            .mount(&mock)
            .await;

        let registry = DockerRegistry::with_base_url(&mock.uri());
        let results = registry
            .resolve_batch(&[dep("node", "20")], TargetLevel::Latest)
            .await;

        let error = results[0]
            .1
            .as_ref()
            .expect_err("unusable challenge is an error");
        assert!(format!("{error:?}").contains("credentials"));
    }

    #[tokio::test]
    async fn resolve_batch_errors_on_an_unparseable_image_name() {
        install_crypto_provider();
        // An empty name never reaches the network.
        let registry = DockerRegistry::with_base_url("http://127.0.0.1:1");
        let results = registry
            .resolve_batch(&[dep("", "20")], TargetLevel::Latest)
            .await;
        assert!(results[0].1.is_err());
    }
}
