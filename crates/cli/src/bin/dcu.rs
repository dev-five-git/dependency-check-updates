//! `dcu` short-alias binary. Behaviour is identical to
//! `dependency-check-updates`; both call into the same library entry point.
//! The duplication is a 20-line stub — keeping it as a separate source file
//! avoids the Cargo warning emitted when two `[[bin]]` targets share a path.

use std::process::ExitCode;

#[tokio::main(flavor = "current_thread")]
#[cfg(not(tarpaulin_include))]
async fn main() -> ExitCode {
    let cli = dependency_check_updates::parse_args();
    let error_level = cli.error_level;

    match dependency_check_updates::run(&cli).await {
        Ok(has_updates) => {
            if error_level >= 2 && has_updates {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eprintln!("Error: {e}");
            ExitCode::FAILURE
        }
    }
}
