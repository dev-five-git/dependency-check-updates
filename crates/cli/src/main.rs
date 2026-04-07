use std::process::ExitCode;

#[tokio::main(flavor = "current_thread")]
#[cfg(not(tarpaulin_include))]
async fn main() -> ExitCode {
    let cli = dependency_check_updates_cli::parse_args();
    let error_level = cli.error_level;

    match dependency_check_updates_cli::run(&cli).await {
        Ok(has_updates) => {
            // error_level 2: exit 1 if any updates were found (CI mode)
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
