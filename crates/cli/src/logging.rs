/// Initialize tracing/logging based on verbosity level.
#[cfg(not(tarpaulin_include))]
pub(crate) fn init_tracing(verbose: u8) {
    use tracing_subscriber::{EnvFilter, fmt};

    let level = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("dependency_check_updates={level}")));

    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}
