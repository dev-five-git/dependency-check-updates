//! Shared HTTP client construction for the registry clients.
//!
//! Every ecosystem registry (npm, crates.io, `PyPI`, GitHub) builds a
//! `reqwest::Client` with the same timeout and user-agent. Centralising the
//! builder here keeps that configuration in one place; the concurrency ceiling
//! is exposed as a constant because each registry wraps the client in its own
//! [`tokio::sync::Semaphore`].

use std::future::Future;
use std::time::Duration;

use reqwest::Client;

use crate::error::DcuError;
use crate::types::{DependencySpec, ResolvedVersion};

/// Default ceiling on concurrent in-flight registry requests.
///
/// GitHub's registry uses a lower limit (its unauthenticated rate budget is
/// only 60 req/hr); it defines its own constant rather than using this one.
pub const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 10;

/// Default per-request timeout, in seconds.
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;

/// Build the shared `reqwest::Client` used by every registry.
///
/// Applies the default timeout and a `dependency-check-updates/<version>`
/// user-agent.
///
/// # Panics
///
/// Panics if the client cannot be built. With the fixed configuration used
/// here this never happens in practice — a failure would indicate a broken
/// TLS backend at the platform level, not a recoverable runtime condition.
#[must_use]
pub fn build_client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS))
        .user_agent(concat!(
            "dependency-check-updates/",
            env!("CARGO_PKG_VERSION")
        ))
        .build()
        .expect("failed to create HTTP client")
}

/// Drive a batch of per-dependency resolutions concurrently while preserving
/// the input ordering.
///
/// Every per-ecosystem registry whose `resolve_version` is an `async fn` over
/// a single dep funnels through this helper. Centralising the
/// `join_all`-over-`enumerate().map()` pipeline keeps the concurrency model —
/// no `tokio::spawn`, so no per-dep `JoinHandle` allocation and no
/// `DependencySpec`/`Arc` clones (each dep is borrowed for the duration of the
/// `.await`) — defined in one place. Real concurrency still comes from each
/// registry's `Semaphore`-gated HTTP requests, which cooperate via `.await`.
///
/// `join_all` preserves the source order of the iterator, so callers can rely
/// on `result[i].0 == i` and no post-sort is needed.
pub async fn resolve_batch_concurrent<'a, F, Fut>(
    deps: &'a [DependencySpec],
    resolve_one: F,
) -> Vec<(usize, Result<ResolvedVersion, DcuError>)>
where
    F: Fn(&'a DependencySpec) -> Fut,
    Fut: Future<Output = Result<ResolvedVersion, DcuError>>,
{
    futures::future::join_all(deps.iter().enumerate().map(|(idx, dep)| {
        let fut = resolve_one(dep);
        async move { (idx, fut.await) }
    }))
    .await
}
