use std::process::ExitCode;

#[tokio::main(flavor = "current_thread")]
#[cfg(not(tarpaulin_include))]
async fn main() -> ExitCode {
    dependency_check_updates::run_cli().await
}
