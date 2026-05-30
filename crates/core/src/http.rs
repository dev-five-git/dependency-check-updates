//! Shared HTTP client construction for the registry clients.
//!
//! Every ecosystem registry (npm, crates.io, `PyPI`, GitHub) builds a
//! `reqwest::Client` with the same timeout and user-agent. Centralising the
//! builder here keeps that configuration in one place; the concurrency ceiling
//! is exposed as a constant because each registry wraps the client in its own
//! [`tokio::sync::Semaphore`].

use std::time::Duration;

use reqwest::Client;

/// Default ceiling on concurrent in-flight registry requests.
///
/// GitHub's registry uses a lower limit (its unauthenticated rate budget is
/// only 60 req/hr); it defines its own constant rather than using this one.
pub const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 10;

/// Default per-request timeout, in seconds.
pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;

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
