//! `dcu` short-alias binary. Behaviour is identical to
//! `dependency-check-updates`; both binaries delegate to
//! `dependency_check_updates::run_cli`. This file exists separately from
//! `src/main.rs` only to avoid Cargo's "file present in multiple build
//! targets" warning when two `[[bin]]` targets would otherwise share a
//! path.

use std::process::ExitCode;

#[tokio::main(flavor = "current_thread")]
#[cfg(not(tarpaulin_include))]
async fn main() -> ExitCode {
    dependency_check_updates::run_cli().await
}
