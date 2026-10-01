//! Bounded, run-scoped GET metadata cache with in-flight request sharing.
//! Keys include all explicitly supplied headers and response size policy. This
//! client has no auth defaults or cookie jar; credentials are never logged.
use crate::{DEFAULT_MAX_CONCURRENT_REQUESTS, DcuError, build_client};
use futures::future::{BoxFuture, FutureExt, Shared};
use reqwest::header::HeaderMap;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Semaphore;

const MAX_KEYS: usize = 512;
const MAX_CACHED_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Hash, PartialEq, Eq)]
struct Key {
    url: String,
    headers: Vec<(String, Vec<u8>)>,
    limit: usize,
}

type Response = Result<(Arc<[u8]>, bool), Arc<str>>;
type Request = Shared<BoxFuture<'static, Response>>;

struct Cached {
    id: u64,
    request: Request,
}

struct State {
    client: reqwest::Client,
    requests: Mutex<HashMap<Key, Cached>>,
    semaphore: Arc<Semaphore>,
    cached_bytes: Arc<AtomicUsize>,
    sequence: AtomicU64,
}

/// Share metadata/download responses within one execution, never on disk.
/// Clones share in-flight requests; a newly constructed cache is independent.
#[derive(Clone)]
pub struct MetadataCache {
    state: Arc<State>,
}

impl Default for MetadataCache {
    fn default() -> Self {
        Self::new()
    }
}

impl MetadataCache {
    /// Maximum accepted size of metadata responses (32 MiB).
    pub const METADATA_LIMIT: usize = 32 * 1024 * 1024;
    /// Maximum accepted size of integrity downloads (50 MiB).
    pub const INTEGRITY_LIMIT: usize = 50 * 1024 * 1024;

    /// Construct a cache with a shared client and ten concurrent GETs.
    #[must_use]
    pub fn new() -> Self {
        Self::from_client(build_client())
    }

    /// A bounded cache which refuses redirects, for explicitly authorized
    /// private endpoints. Credentials cannot follow a redirect to another URL.
    #[must_use]
    pub fn without_redirects() -> Self {
        Self::from_client(crate::http::build_client_with_redirects(
            reqwest::redirect::Policy::none(),
        ))
    }

    fn from_client(client: reqwest::Client) -> Self {
        Self {
            state: Arc::new(State {
                client,
                requests: Mutex::new(HashMap::new()),
                semaphore: Arc::new(Semaphore::new(DEFAULT_MAX_CONCURRENT_REQUESTS)),
                cached_bytes: Arc::new(AtomicUsize::new(0)),
                sequence: AtomicU64::new(0),
            }),
        }
    }

    /// Fetch bounded bytes, reusing an equivalent GET (URL, headers, limit).
    /// Failures are also shared for this execution and attributed to `name`.
    /// No version-selection result is cached, so targets/pins stay independent.
    ///
    /// # Errors
    /// Returns a registry diagnostic on HTTP/network failure or oversized data.
    pub async fn get(
        &self,
        url: &str,
        headers: HeaderMap,
        limit: usize,
        name: &str,
    ) -> Result<Arc<[u8]>, DcuError> {
        let mut header_key: Vec<_> = headers
            .iter()
            .map(|(name, v)| (name.as_str().to_owned(), v.as_bytes().to_vec()))
            .collect();
        header_key.sort();
        let key = Key {
            url: url.to_owned(),
            headers: header_key,
            limit,
        };
        let (id, request) = {
            let mut requests = self
                .state
                .requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cached) = requests.get(&key) {
                (cached.id, cached.request.clone())
            } else {
                let client = self.state.client.clone();
                let semaphore = self.state.semaphore.clone();
                let cached_bytes = self.state.cached_bytes.clone();
                let url = url.to_owned();
                let id = self.state.sequence.fetch_add(1, Ordering::Relaxed);
                let cacheable = requests.len() < MAX_KEYS;
                let request = async move {
                    fetch(
                        &client,
                        &semaphore,
                        &cached_bytes,
                        &url,
                        headers,
                        limit,
                        cacheable,
                    )
                    .await
                }
                .boxed()
                .shared();
                if cacheable {
                    requests.insert(
                        key.clone(),
                        Cached {
                            id,
                            request: request.clone(),
                        },
                    );
                }
                (id, request)
            }
        };
        let result = request.await;
        if result.as_ref().is_ok_and(|(_, retained)| !retained) {
            let mut requests = self
                .state
                .requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if requests.get(&key).is_some_and(|cached| cached.id == id) {
                requests.remove(&key);
            }
        }
        result
            .map(|(bytes, _)| bytes)
            .map_err(|detail| DcuError::RegistryLookup {
                package: name.to_owned(),
                detail: detail.to_string(),
            })
    }
}

async fn fetch(
    client: &reqwest::Client,
    semaphore: &Semaphore,
    cached_bytes: &AtomicUsize,
    url: &str,
    headers: HeaderMap,
    limit: usize,
    cacheable: bool,
) -> Response {
    let _permit = semaphore
        .acquire()
        .await
        .map_err(|e| Arc::<str>::from(e.to_string()))?;
    let mut response = client
        .get(url)
        .headers(headers)
        .send()
        .await
        .map_err(|e| Arc::<str>::from(e.without_url().to_string()))?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()).into());
    }
    if response.content_length().is_some_and(|n| n > limit as u64) {
        return Err("response exceeds size limit".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| Arc::<str>::from(e.without_url().to_string()))?
    {
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err("response exceeds size limit".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    let retained = cacheable
        && cached_bytes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(bytes.len())
                    .filter(|n| *n <= MAX_CACHED_BYTES)
            })
            .is_ok();
    Ok((bytes.into(), retained))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, path},
    };

    #[tokio::test]
    async fn shares_requests_but_separates_headers_urls_and_executions() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let server = MockServer::start().await;
        Mock::given(path("/metadata"))
            .and(header("accept", "application/json"))
            .respond_with(ResponseTemplate::new(200).set_body_string("body"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/metadata"))
            .and(header("accept", "text/xml"))
            .respond_with(ResponseTemplate::new(200).set_body_string("xml"))
            .expect(1)
            .mount(&server)
            .await;
        let cache = MetadataCache::new();
        let url = format!("{}/metadata", server.uri());
        let mut headers = HeaderMap::new();
        headers.insert("accept", "application/json".parse().unwrap());
        let results = futures::future::join_all(
            (0..20).map(|_| cache.get(&url, headers.clone(), 100, "one")),
        )
        .await;
        assert!(
            results
                .iter()
                .all(|r| r.as_ref().unwrap().as_ref() == b"body")
        );
        headers.insert("accept", "text/xml".parse().unwrap());
        assert_eq!(
            cache.get(&url, headers, 100, "two").await.unwrap().as_ref(),
            b"xml"
        );
        server.verify().await;
        server.reset().await;
        Mock::given(path("/metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_string("new"))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            MetadataCache::new()
                .get(&url, HeaderMap::new(), 100, "new")
                .await
                .unwrap()
                .as_ref(),
            b"new"
        );
    }

    #[tokio::test]
    async fn failed_requests_are_shared_and_errors_keep_each_consumers_name() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let server = MockServer::start().await;
        Mock::given(path("/failed"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;
        let cache = MetadataCache::new();
        let url = format!("{}/failed", server.uri());
        let (a, b) = futures::join!(
            cache.get(&url, HeaderMap::new(), 100, "first"),
            cache.get(&url, HeaderMap::new(), 100, "second")
        );
        assert!(a.unwrap_err().to_string().contains("first"));
        assert!(b.unwrap_err().to_string().contains("second"));
    }

    #[tokio::test]
    async fn response_limits_are_enforced_and_not_shared_with_other_limits() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let server = MockServer::start().await;
        Mock::given(path("/large"))
            .respond_with(ResponseTemplate::new(200).set_body_string("1234567890"))
            .expect(2)
            .mount(&server)
            .await;
        let cache = MetadataCache::new();
        let url = format!("{}/large", server.uri());
        assert!(
            cache
                .get(&url, HeaderMap::new(), 4, "small")
                .await
                .unwrap_err()
                .to_string()
                .contains("size limit")
        );
        assert!(cache.get(&url, HeaderMap::new(), 20, "large").await.is_ok());
    }

    #[tokio::test]
    async fn chunked_bodies_cannot_bypass_the_response_size_limit() {
        use std::io::{Read, Write};
        let _ = rustls::crypto::ring::default_provider().install_default();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/stream", listener.local_addr().unwrap());
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .unwrap();
            let mut request = [0; 4096];
            assert!(stream.read(&mut request).unwrap() > 0);
            stream.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\na\r\n1234567890\r\n0\r\n\r\n").unwrap();
        });
        let cache = MetadataCache::default();
        assert!(
            cache
                .get(&url, HeaderMap::new(), 4, "streamed")
                .await
                .unwrap_err()
                .to_string()
                .contains("size limit")
        );
        peer.join().unwrap();
    }

    #[tokio::test]
    async fn authorization_and_repository_urls_are_distinct_cache_keys() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let server = MockServer::start().await;
        for (url, token, body) in [
            ("/one", "Bearer one", "first"),
            ("/one", "Bearer two", "second"),
            ("/two", "Bearer one", "third"),
        ] {
            Mock::given(path(url))
                .and(header("authorization", token))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .expect(1)
                .mount(&server)
                .await;
        }
        let cache = MetadataCache::new();
        for (url, token, body) in [
            ("/one", "Bearer one", "first"),
            ("/one", "Bearer two", "second"),
            ("/two", "Bearer one", "third"),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert("authorization", token.parse().unwrap());
            assert_eq!(
                cache
                    .get(&format!("{}{url}", server.uri()), headers, 100, "consumer")
                    .await
                    .unwrap()
                    .as_ref(),
                body.as_bytes()
            );
        }
    }

    #[tokio::test]
    async fn exhausted_byte_budget_does_not_break_lookups_or_retain_more_bodies() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let server = MockServer::start().await;
        Mock::given(path("/body"))
            .respond_with(ResponseTemplate::new(200).set_body_string("body"))
            .expect(2)
            .mount(&server)
            .await;
        let cache = MetadataCache::new();
        cache
            .state
            .cached_bytes
            .store(MAX_CACHED_BYTES, Ordering::Relaxed);
        let url = format!("{}/body", server.uri());
        for _ in 0..2 {
            assert_eq!(
                cache
                    .get(&url, HeaderMap::new(), 100, "consumer")
                    .await
                    .unwrap()
                    .as_ref(),
                b"body"
            );
        }
        assert!(cache.state.requests.lock().unwrap().is_empty());
        assert_eq!(
            cache.state.cached_bytes.load(Ordering::Relaxed),
            MAX_CACHED_BYTES
        );
    }

    #[tokio::test]
    async fn canceling_last_caller_does_not_keep_cache_state_in_a_reference_cycle() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let server = MockServer::start().await;
        Mock::given(path("/slow"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("body")
                    .set_delay(std::time::Duration::from_secs(1)),
            )
            .mount(&server)
            .await;
        let cache = MetadataCache::new();
        let weak = Arc::downgrade(&cache.state);
        let url = format!("{}/slow", server.uri());
        {
            let pending = cache.get(&url, HeaderMap::new(), 100, "consumer");
            futures::pin_mut!(pending);
            assert!(futures::poll!(pending).is_pending());
        }
        drop(cache);
        assert!(weak.upgrade().is_none());
    }

    #[tokio::test]
    async fn exhausted_key_budget_keeps_request_count_bounded_without_failing_new_lookups() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let server = MockServer::start().await;
        Mock::given(path("/new"))
            .respond_with(ResponseTemplate::new(200).set_body_string("body"))
            .expect(2)
            .mount(&server)
            .await;
        let cache = MetadataCache::new();
        for i in 0..MAX_KEYS {
            let key = Key {
                url: format!("http://unused.invalid/{i}"),
                headers: Vec::new(),
                limit: 100,
            };
            let request = async { Err(Arc::<str>::from("cached failure")) }
                .boxed()
                .shared();
            cache
                .state
                .requests
                .lock()
                .unwrap()
                .insert(key, Cached { id: 0, request });
        }
        let url = format!("{}/new", server.uri());
        for _ in 0..2 {
            assert!(
                cache
                    .get(&url, HeaderMap::new(), 100, "consumer")
                    .await
                    .is_ok()
            );
        }
        assert_eq!(cache.state.requests.lock().unwrap().len(), MAX_KEYS);
        assert_eq!(cache.state.cached_bytes.load(Ordering::Relaxed), 0);
    }
}
