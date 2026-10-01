//! Explicit read-only installed-tool inspection, with bounded command execution.
use crate::cli::Cli;
use crate::project::Entry;
use crate::report::{Item, LocalRow, RunReport, Status};
use crate::tool_registry::ToolRegistry;
use dependency_check_updates_core::{
    DcuError, DependencySection, DependencySpec, pad_to_three_segments,
};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub(crate) fn probe(program: &str, args: &[&str]) -> Result<String, String> {
    let directory = tempfile::TempDir::new()
        .map_err(|e| format!("cannot create neutral probe directory: {e}"))?;
    let mut child = Command::new(program)
        .args(args)
        .current_dir(directory.path())
        .env("COREPACK_ENABLE_NETWORK", "0")
        .env("COREPACK_ENABLE_AUTO_PIN", "0")
        .env("COREPACK_ENABLE_PROJECT_SPEC", "0")
        .env("COREPACK_ENV_FILE", "0")
        .env("COREPACK_DEFAULT_TO_LATEST", "0")
        .env("RUSTUP_AUTO_INSTALL", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "not installed".to_owned()
            } else {
                format!("version query failed: {e}")
            }
        })?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < Duration::from_secs(5) => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("version query failed or timed out after 5s".into());
            }
        }
    }
    let result = child.wait_with_output().map_err(|e| e.to_string())?;
    let text = format!(
        "{} {}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    if !result.status.success() {
        return Err(format!("version query failed: {}", text.trim()));
    }
    regex::Regex::new(r#"(?:^|[\s"])([0-9]+(?:\.[0-9]+){0,2})"#)
        .unwrap()
        .captures(&text)
        .map(|c| c[1].to_owned())
        .ok_or_else(|| format!("unrecognized version output: {}", text.trim()))
}

pub(crate) async fn run(cli: &Cli, registry: &ToolRegistry) -> Result<RunReport, DcuError> {
    run_with_probe(cli, registry, probe).await
}

#[allow(clippy::too_many_lines)]
async fn run_with_probe<F>(
    cli: &Cli,
    registry: &ToolRegistry,
    probe_tool: F,
) -> Result<RunReport, DcuError>
where
    F: Fn(&str, &[&str]) -> Result<String, String> + Send + Sync + Copy + 'static,
{
    if cli.format == crate::cli::OutputFormat::JsonLegacy {
        return Err(crate::project::error(
            "json-legacy",
            "local tools require --format json or json-report",
        ));
    }
    let specs = [
        (
            "node",
            "node",
            vec!["--version"],
            "Node has no self-update command; use its official installer: https://nodejs.org/en/download",
        ),
        ("bun", "bun", vec!["--version"], "bun upgrade"),
        ("pnpm", "pnpm", vec!["--version"], "pnpm self-update"),
        ("rust", "rustc", vec!["--version"], "rustup update stable"),
        (
            "jdk",
            "java",
            vec!["-version"],
            "JDK has no self-update command; use the official installer or configured Adoptium repository: https://adoptium.net/installation",
        ),
    ];
    let mut rows = Vec::new();
    for (name, program, args, guide) in specs {
        if (!cli.filter.is_empty() && !cli.filter.iter().any(|f| name.contains(f)))
            || cli.reject.iter().any(|f| name.contains(f))
        {
            continue;
        }
        let program = program.to_owned();
        let args: Vec<_> = args.into_iter().map(str::to_owned).collect();
        let installed = tokio::task::spawn_blocking(move || {
            let refs: Vec<_> = args.iter().map(String::as_str).collect();
            probe_tool(&program, &refs).or_else(|e| {
                if cfg!(windows) && program == "pnpm" && e == "not installed" {
                    probe_tool("pnpm.cmd", &refs)
                } else {
                    Err(e)
                }
            })
        })
        .await
        .map_err(|e| crate::project::error(name, e.to_string()))?;
        let mut row = LocalRow {
            name: name.into(),
            scope: "local".into(),
            installed: None,
            latest: None,
            selected: None,
            status: Status::Missing,
            reason: None,
            update_command: guide.into(),
            updated: false,
        };
        match installed {
            Ok(version) => {
                row.installed = Some(version.clone());
                let entry = Entry {
                    requested: true,
                    dep: DependencySpec {
                        name: name.into(),
                        current_req: version.clone(),
                        section: DependencySection::Toolchain,
                        path_version: None,
                    },
                    span: None,
                    reason: None,
                    repositories: Vec::new(),
                    integrity: None,
                };
                match registry.resolve(&entry, cli.target).await {
                    Ok(r) => {
                        let cmp = r
                            .selected
                            .as_ref()
                            .and_then(|v| semver::Version::parse(&pad_to_three_segments(v)).ok())
                            .zip(semver::Version::parse(&pad_to_three_segments(&version)).ok());
                        let newer = cmp
                            .as_ref()
                            .is_some_and(|(latest, current)| latest > current);
                        row.latest = r.latest;
                        row.selected = r.selected;
                        row.status = if newer {
                            Status::Update
                        } else if cmp.is_some() {
                            Status::Current
                        } else {
                            Status::Unverified
                        };
                    }
                    Err(e) => {
                        row.status = Status::Failed;
                        row.reason = Some(e.to_string());
                    }
                }
            }
            Err(e) => {
                if e != "not installed" {
                    row.status = Status::Failed;
                }
                row.reason = Some(e);
            }
        }
        rows.push(row);
    }
    let report = RunReport {
        items: rows.into_iter().map(Item::Local).collect(),
        ..RunReport::default()
    };
    if cli.format.is_json() {
        report.print_json(cli.format)?;
    } else {
        for item in &report.items {
            if let Item::Local(row) = item {
                println!(
                    "{}: installed={} latest={} [{:?}]\n  {}",
                    row.name,
                    row.installed.as_deref().unwrap_or("unknown"),
                    row.latest.as_deref().unwrap_or("unknown"),
                    row.status,
                    row.reason.as_deref().unwrap_or(&row.update_command)
                );
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[tokio::test]
    async fn local_reports_use_fixed_metadata_and_never_confuse_missing_failed_and_current() {
        use crate::tool_registry::Endpoints;
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};
        let _ = rustls::crypto::ring::default_provider().install_default();
        let server = MockServer::start().await;
        Mock::given(path("/node"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"version":"v22.0.0", "lts":"Fixed"}
            ])))
            .mount(&server)
            .await;
        let registry = ToolRegistry::with_endpoints(Endpoints {
            node: format!("{}/node", server.uri()),
            ..Endpoints::default()
        });
        for (installed, expected) in [
            (Ok("20.0.0"), Status::Update),
            (Ok("22.0.0"), Status::Current),
            (Ok("unparseable"), Status::Unverified),
            (Err("not installed"), Status::Missing),
            (Err("version query failed"), Status::Failed),
        ] {
            let cli = Cli::parse_from(["dcu", "--local-tools", "node"]);
            let report = run_with_probe(&cli, &registry, move |_, _| {
                installed.map(str::to_owned).map_err(str::to_owned)
            })
            .await
            .unwrap();
            let Item::Local(row) = &report.items[0] else {
                panic!("expected local item")
            };
            assert_eq!(row.status, expected);
            assert!(!row.updated);
        }
        let cli = Cli::parse_from([
            "dcu",
            "--local-tools",
            "node",
            "--reject",
            "node",
            "--format",
            "json",
        ]);
        assert!(
            run_with_probe(&cli, &registry, |_, _| panic!("filtered tool was probed"))
                .await
                .unwrap()
                .items
                .is_empty()
        );
        let cli = Cli::parse_from(["dcu", "--local-tools", "node", "--format", "json-legacy"]);
        assert!(
            run_with_probe(&cli, &registry, |_, _| panic!("legacy tool was probed"))
                .await
                .is_err()
        );
        let registry = ToolRegistry::with_endpoints(Endpoints {
            node: format!("{}/missing", server.uri()),
            ..Endpoints::default()
        });
        let cli = Cli::parse_from(["dcu", "--local-tools", "node", "--format", "json-report"]);
        let report = run_with_probe(&cli, &registry, |_, _| Ok("20.0.0".into()))
            .await
            .unwrap();
        let Item::Local(row) = &report.items[0] else {
            panic!("expected local item")
        };
        assert_eq!(row.status, Status::Failed);
        assert!(row.reason.as_ref().unwrap().contains("404"));
    }
    #[test]
    fn missing_tool_is_reported() {
        assert_eq!(
            probe("dcu-intentionally-missing-tool-0d215b", &["--version"]).unwrap_err(),
            "not installed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn probe_reports_command_failures_and_disables_automatic_installs() {
        assert!(
            probe("sh", &["-c", "exit 7"])
                .unwrap_err()
                .contains("version query failed")
        );
        assert!(
            probe("sh", &["-c", "printf unrecognized"])
                .unwrap_err()
                .contains("unrecognized version output")
        );
        let script = "test \"$COREPACK_ENABLE_NETWORK\" = 0 && test \"$COREPACK_ENABLE_PROJECT_SPEC\" = 0 && test \"$RUSTUP_AUTO_INSTALL\" = 0 && printf 'rustc 1.85.0'";
        assert_eq!(probe("sh", &["-c", script]).unwrap(), "1.85.0");
    }

    #[cfg(unix)]
    #[test]
    fn probe_times_out_without_waiting_for_a_hung_tool() {
        let started = Instant::now();
        assert!(
            probe("sh", &["-c", "exec sleep 30"])
                .unwrap_err()
                .contains("timed out")
        );
        assert!(started.elapsed() < Duration::from_secs(15));
    }

    #[cfg(windows)]
    #[test]
    fn probe_reports_command_failures_and_disables_automatic_installs() {
        assert!(
            probe("cmd", &["/d", "/c", "exit 7"])
                .unwrap_err()
                .contains("version query failed")
        );
        let script = "if %COREPACK_ENABLE_NETWORK%==0 if %COREPACK_ENABLE_PROJECT_SPEC%==0 if %RUSTUP_AUTO_INSTALL%==0 echo rustc 1.85.0";
        assert_eq!(probe("cmd", &["/d", "/c", script]).unwrap(), "1.85.0");
    }
}
