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

#[allow(clippy::too_many_lines)]
pub(crate) async fn run(cli: &Cli, registry: &ToolRegistry) -> Result<RunReport, DcuError> {
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
            probe(&program, &refs).or_else(|e| {
                if cfg!(windows) && program == "pnpm" && e == "not installed" {
                    probe("pnpm.cmd", &refs)
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
            let Item::Local(row) = item else {
                continue;
            };
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
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_tool_is_reported() {
        assert_eq!(
            probe("dcu-intentionally-missing-tool-0d215b", &["--version"]).unwrap_err(),
            "not installed"
        );
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
