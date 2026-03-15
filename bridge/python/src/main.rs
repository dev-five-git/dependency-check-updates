//! Standalone binary for `PyPI` distribution of dependency-check-updates.
//!
//! Compiled with maturin as a native executable. The Python stub locates this
//! binary via `sysconfig` paths and executes it with command-line arguments
//! forwarded from `sys.argv`.

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    dependency_check_updates_cli::main(&std::env::args().collect::<Vec<String>>()).await
}
