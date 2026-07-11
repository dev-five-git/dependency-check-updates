use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use tracing::warn;

use crate::cleanup::{installed_dirs_for, lockfiles_for};
use crate::run::ManifestJob;

#[derive(Clone, Copy)]
enum CleanupKind {
    Lockfile,
    InstalledDir,
}

#[derive(Clone)]
pub(crate) struct CleanupTarget {
    path: PathBuf,
    label: String,
    kind: CleanupKind,
}

struct RemovalOutcome {
    label: String,
    bytes: u64,
}

pub(crate) fn targets_for_job(
    job: &ManifestJob,
    remove_lockfile: bool,
    remove_installed: bool,
) -> Vec<CleanupTarget> {
    let Some(dir) = job.manifest_ref.path.parent() else {
        return Vec::new();
    };

    let mut targets = Vec::new();

    if remove_lockfile {
        for name in lockfiles_for(job.manifest_ref.kind) {
            targets.push(CleanupTarget {
                path: dir.join(name),
                label: format!("{}:{name}", job.display_path),
                kind: CleanupKind::Lockfile,
            });
        }
    }

    if remove_installed {
        for name in installed_dirs_for(job.manifest_ref.kind) {
            targets.push(CleanupTarget {
                path: dir.join(name),
                label: format!("{}:{name}/", job.display_path),
                kind: CleanupKind::InstalledDir,
            });
        }
    }

    targets
}

pub(crate) async fn cleanup_with_progress(targets: &[CleanupTarget]) -> String {
    if targets.is_empty() {
        return String::new();
    }

    let pb = ProgressBar::new(targets.len() as u64);
    if let Ok(style) = ProgressStyle::with_template(
        "{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {pos}/{len} {msg}",
    ) {
        pb.set_style(style.progress_chars("=>-"));
    }
    pb.enable_steady_tick(Duration::from_millis(80));

    pb.set_message("phase 2: removing lockfiles and installed directories");

    let mut removals = FuturesUnordered::new();
    for target in targets {
        let target = target.clone();
        removals.push(tokio::task::spawn_blocking(move || remove_target(&target)));
    }

    let mut removed = Vec::with_capacity(targets.len());
    let mut total_bytes = 0_u64;

    while let Some(outcome) = removals.next().await {
        match outcome {
            Ok(Some(Ok(outcome))) => {
                total_bytes = total_bytes.saturating_add(outcome.bytes);
                pb.set_message(format!(
                    "removed {} ({}, total {})",
                    outcome.label,
                    format_bytes(outcome.bytes),
                    format_bytes(total_bytes),
                ));
                removed.push(outcome);
            }
            Ok(Some(Err(error))) => {
                warn!(error = %error, "failed to remove cleanup target");
            }
            Ok(None) => {}
            Err(error) => {
                warn!(error = %error, "cleanup worker failed");
            }
        }

        pb.inc(1);
    }

    pb.finish_and_clear();
    render_cleanup_summary(&mut removed, total_bytes)
}

fn remove_target(target: &CleanupTarget) -> Option<Result<RemovalOutcome, io::Error>> {
    let bytes = match path_size(&target.path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(error) => return Some(Err(error)),
    };

    let remove_result = match target.kind {
        CleanupKind::Lockfile => fs::remove_file(&target.path),
        CleanupKind::InstalledDir => fs::remove_dir_all(&target.path),
    };

    match remove_result {
        Ok(()) => Some(Ok(RemovalOutcome {
            label: target.label.clone(),
            bytes,
        })),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => Some(Err(error)),
    }
}

fn path_size(path: &Path) -> io::Result<u64> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    if !metadata.is_dir() {
        return Ok(0);
    }

    dir_size(path)
}

fn dir_size(path: &Path) -> io::Result<u64> {
    let mut total = 0_u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        total = total.saturating_add(path_size(&entry.path())?);
    }
    Ok(total)
}

fn render_cleanup_summary(removed: &mut [RemovalOutcome], total_bytes: u64) -> String {
    if removed.is_empty() {
        return String::new();
    }

    removed.sort_unstable_by(|a, b| a.label.cmp(&b.label));

    let mut output = String::new();
    for outcome in removed {
        output.push_str(" Removed ");
        output.push_str(&outcome.label);
        output.push_str(" (");
        output.push_str(&format_bytes(outcome.bytes));
        output.push_str(")\n");
    }
    output.push_str(" Total removed ");
    output.push_str(&format_bytes(total_bytes));
    output.push('\n');
    output
}

fn format_bytes(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    const KIB: u64 = 1024;

    if bytes >= GIB {
        format_unit(bytes, GIB, "GiB")
    } else if bytes >= MIB {
        format_unit(bytes, MIB, "MiB")
    } else if bytes >= KIB {
        format_unit(bytes, KIB, "KiB")
    } else {
        format!("{bytes} B")
    }
}

fn format_unit(bytes: u64, unit: u64, suffix: &str) -> String {
    let whole = bytes / unit;
    let rounded_fraction = ((bytes % unit) * 100 + unit / 2) / unit;
    if rounded_fraction == 100 {
        format!("{}.00 {suffix}", whole + 1)
    } else {
        format!("{whole}.{rounded_fraction:02} {suffix}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dependency_check_updates_core::{ManifestKind, ManifestRef};

    #[test]
    fn format_bytes_uses_binary_units() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.50 KiB");
        assert_eq!(format_bytes(2 * 1024 * 1024), "2.00 MiB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.00 GiB");
    }

    #[test]
    fn targets_for_job_defers_deletion_but_preserves_requested_entries() {
        let job = ManifestJob {
            manifest_ref: ManifestRef {
                path: PathBuf::from("repo/package.json"),
                kind: ManifestKind::PackageJson,
            },
            display_path: "package.json".to_owned(),
            text: String::new(),
            handler: &dependency_check_updates_node::NodeHandler,
            deps: Vec::new(),
        };

        let targets = targets_for_job(&job, true, true);

        assert!(
            targets
                .iter()
                .any(|target| target.label == "package.json:node_modules/")
        );
        assert!(
            targets
                .iter()
                .any(|target| target.label == "package.json:package-lock.json")
        );
    }

    #[test]
    fn render_cleanup_summary_sorts_removal_outcomes_lexicographically() {
        // Given: removal outcomes in non-alphabetical order
        let mut outcomes = vec![
            RemovalOutcome {
                label: "zebra.lock".to_owned(),
                bytes: 1024,
            },
            RemovalOutcome {
                label: "apple.lock".to_owned(),
                bytes: 2048,
            },
            RemovalOutcome {
                label: "middle.lock".to_owned(),
                bytes: 512,
            },
        ];

        // When: rendering the cleanup summary
        let output = render_cleanup_summary(&mut outcomes, 3584);

        // Then: the output lines are sorted lexicographically by label
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 4); // 3 removal lines + 1 total line
        assert!(lines[0].contains("apple.lock"));
        assert!(lines[1].contains("middle.lock"));
        assert!(lines[2].contains("zebra.lock"));
        assert!(lines[3].contains("Total removed"));
    }
}
