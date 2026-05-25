//! `dcu` short-alias binary for the `PyPI` distribution. Identical behaviour to
//! the `dependency-check-updates` binary in this same crate; the duplication
//! exists so each `[[bin]]` target gets its own source path and Cargo does not
//! warn about a shared file.

#[tokio::main(flavor = "current_thread")]
#[cfg(not(tarpaulin_include))]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    dependency_check_updates::main(&std::env::args().collect::<Vec<String>>()).await
}
