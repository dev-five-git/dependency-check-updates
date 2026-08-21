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

#[derive(Debug)]
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

pub(crate) async fn cleanup_with_progress(targets: Vec<CleanupTarget>) -> String {
    let len = targets.len();
    if len == 0 {
        return String::new();
    }

    let pb = ProgressBar::new(len as u64);
    if let Ok(style) = ProgressStyle::with_template(
        "{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {pos}/{len} {msg}",
    ) {
        pb.set_style(style.progress_chars("=>-"));
    }
    pb.enable_steady_tick(Duration::from_millis(80));

    pb.set_message("phase 2: removing lockfiles and installed directories");

    let mut removals = FuturesUnordered::new();
    for target in targets {
        removals.push(tokio::task::spawn_blocking(move || remove_target(target)));
    }

    let mut removed = Vec::with_capacity(len);
    let mut total_bytes = 0_u64;

    while let Some(outcome) = removals.next().await {
        if let Some(message) = absorb_outcome(outcome, &mut removed, &mut total_bytes) {
            pb.set_message(message);
        }
        pb.inc(1);
    }

    pb.finish_and_clear();
    render_cleanup_summary(&mut removed, total_bytes)
}

/// Fold one worker's result into the running tally, returning the progress
/// message to display when something was actually removed.
///
/// Split out of [`cleanup_with_progress`] so every arm is reachable from a
/// test. The `JoinError` arm in particular only arises when a worker panics,
/// which cannot be provoked by driving the public entry point — [`remove_target`]
/// has no panic path — but is trivially constructed by awaiting a task that
/// does panic.
fn absorb_outcome(
    outcome: Result<Option<Result<RemovalOutcome, io::Error>>, tokio::task::JoinError>,
    removed: &mut Vec<RemovalOutcome>,
    total_bytes: &mut u64,
) -> Option<String> {
    match outcome {
        Ok(Some(Ok(outcome))) => {
            *total_bytes = total_bytes.saturating_add(outcome.bytes);
            let message = format!(
                "removed {} ({}, total {})",
                outcome.label,
                format_bytes(outcome.bytes),
                format_bytes(*total_bytes),
            );
            removed.push(outcome);
            Some(message)
        }
        Ok(Some(Err(error))) => {
            warn!(error = %error, "failed to remove cleanup target");
            None
        }
        // The target was already gone — nothing removed, nothing to report.
        Ok(None) => None,
        Err(error) => {
            warn!(error = %error, "cleanup worker failed");
            None
        }
    }
}

fn remove_target(target: CleanupTarget) -> Option<Result<RemovalOutcome, io::Error>> {
    let bytes = match path_size(&target.path) {
        Ok(bytes) => bytes,
        // A target that vanished between planning and removal is not a
        // failure; anything else is. Both readings share one expression so the
        // "already gone" case cannot drift from the sizing step below.
        Err(error) => return (error.kind() != io::ErrorKind::NotFound).then_some(Err(error)),
    };

    let remove_result = match target.kind {
        CleanupKind::Lockfile => fs::remove_file(&target.path),
        CleanupKind::InstalledDir => fs::remove_dir_all(&target.path),
    };

    match remove_result {
        Ok(()) => Some(Ok(RemovalOutcome {
            label: target.label,
            bytes,
        })),
        Err(error) => (error.kind() != io::ErrorKind::NotFound).then_some(Err(error)),
    }
}

fn path_size(path: &Path) -> io::Result<u64> {
    // `symlink_metadata` deliberately does not follow links: a symlink into a
    // directory tree would otherwise be counted twice, or lead outside it.
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        return dir_size(path);
    }
    // Regular files report their own length. Everything else a directory can
    // contain — symlinks, sockets, device nodes — reclaims no space when the
    // entry itself is unlinked, so it contributes nothing.
    Ok(if metadata.is_file() {
        metadata.len()
    } else {
        0
    })
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
    use tempfile::TempDir;

    /// Write `contents` to `dir/name` and return the path.
    fn write_file(dir: &Path, name: &str, contents: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, contents).expect("write fixture file");
        path
    }

    fn target(path: PathBuf, kind: CleanupKind) -> CleanupTarget {
        CleanupTarget {
            path,
            label: "fixture".to_owned(),
            kind,
        }
    }

    #[test]
    fn path_size_reports_a_file_length() {
        let dir = TempDir::new().unwrap();
        let file = write_file(dir.path(), "bun.lock", &[0u8; 128]);
        assert_eq!(path_size(&file).unwrap(), 128);
    }

    #[test]
    fn path_size_sums_a_directory_tree_recursively() {
        // node_modules is nested, so the size must come from a full walk
        // rather than the directory entry's own metadata.
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("node_modules").join("pkg").join("dist");
        fs::create_dir_all(&nested).unwrap();
        write_file(dir.path().join("node_modules").as_path(), "top", &[0u8; 10]);
        write_file(&nested, "deep", &[0u8; 25]);

        assert_eq!(path_size(&dir.path().join("node_modules")).unwrap(), 35);
    }

    #[test]
    fn path_size_reports_zero_for_an_empty_directory() {
        let dir = TempDir::new().unwrap();
        let empty = dir.path().join("empty");
        fs::create_dir(&empty).unwrap();
        assert_eq!(path_size(&empty).unwrap(), 0);
    }

    #[test]
    fn path_size_propagates_a_missing_path() {
        let dir = TempDir::new().unwrap();
        let error = path_size(&dir.path().join("absent")).expect_err("missing path must error");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    /// Entries that are neither a regular file nor a directory contribute
    /// nothing. Exercised through a dangling symlink, which is the only such
    /// entry creatable without elevated privileges — and only on Unix, where
    /// `std::os::unix::fs::symlink` needs no special rights.
    #[cfg(unix)]
    #[test]
    fn path_size_ignores_entries_that_are_neither_file_nor_directory() {
        let dir = TempDir::new().unwrap();
        let link = dir.path().join("dangling");
        std::os::unix::fs::symlink("nowhere", &link).unwrap();
        assert_eq!(path_size(&link).unwrap(), 0);
    }

    #[test]
    fn dir_size_propagates_a_missing_directory() {
        let dir = TempDir::new().unwrap();
        let error = dir_size(&dir.path().join("absent")).expect_err("missing dir must error");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn remove_target_deletes_a_lockfile_and_reports_its_size() {
        let dir = TempDir::new().unwrap();
        let file = write_file(dir.path(), "bun.lock", &[0u8; 64]);

        let outcome = remove_target(target(file.clone(), CleanupKind::Lockfile))
            .expect("an existing target yields an outcome")
            .expect("removing a plain file succeeds");

        assert_eq!(outcome.bytes, 64);
        assert_eq!(outcome.label, "fixture");
        assert!(!file.exists(), "the lockfile must be gone");
    }

    #[test]
    fn remove_target_deletes_an_installed_directory_tree() {
        let dir = TempDir::new().unwrap();
        let installed = dir.path().join("node_modules");
        fs::create_dir_all(installed.join("pkg")).unwrap();
        write_file(installed.join("pkg").as_path(), "index.js", &[0u8; 40]);

        let outcome = remove_target(target(installed.clone(), CleanupKind::InstalledDir))
            .expect("an existing target yields an outcome")
            .expect("removing a directory tree succeeds");

        assert_eq!(outcome.bytes, 40);
        assert!(!installed.exists(), "the directory tree must be gone");
    }

    #[test]
    fn remove_target_treats_an_absent_target_as_nothing_to_do() {
        // Targets are planned before removal runs, so one may legitimately
        // vanish in between. That is not a failure to report.
        let dir = TempDir::new().unwrap();
        let missing = target(dir.path().join("never-existed.lock"), CleanupKind::Lockfile);
        assert!(remove_target(missing).is_none());
    }

    #[test]
    fn remove_target_surfaces_a_real_removal_failure() {
        // A lockfile-kind target pointing at a directory: sizing succeeds, but
        // `remove_file` refuses with something other than "not found".
        let dir = TempDir::new().unwrap();
        let not_a_file = dir.path().join("node_modules");
        fs::create_dir(&not_a_file).unwrap();

        let error = remove_target(target(not_a_file.clone(), CleanupKind::Lockfile))
            .expect("a real failure must be reported")
            .expect_err("removing a directory as a file cannot succeed");

        assert_ne!(error.kind(), io::ErrorKind::NotFound);
        assert!(not_a_file.exists(), "the directory must survive");
    }

    #[test]
    fn absorb_outcome_accumulates_removals_and_reports_a_running_total() {
        let mut removed = Vec::new();
        let mut total = 0_u64;

        let first = absorb_outcome(
            Ok(Some(Ok(RemovalOutcome {
                label: "a.lock".to_owned(),
                bytes: 1024,
            }))),
            &mut removed,
            &mut total,
        );
        let second = absorb_outcome(
            Ok(Some(Ok(RemovalOutcome {
                label: "b.lock".to_owned(),
                bytes: 1024,
            }))),
            &mut removed,
            &mut total,
        );

        assert_eq!(total, 2048);
        assert_eq!(removed.len(), 2);
        assert!(first.unwrap().contains("total 1.00 KiB"));
        assert!(second.unwrap().contains("total 2.00 KiB"));
    }

    /// The three non-removal arms must all leave the tally untouched and
    /// produce no progress message.
    #[tokio::test]
    async fn absorb_outcome_ignores_every_non_removal_result() {
        let mut removed = Vec::new();
        let mut total = 0_u64;

        // A removal that failed.
        assert!(
            absorb_outcome(
                Ok(Some(Err(io::Error::other("disk on fire")))),
                &mut removed,
                &mut total,
            )
            .is_none()
        );
        // A target that was already gone.
        assert!(absorb_outcome(Ok(None), &mut removed, &mut total).is_none());
        // A worker that panicked. `remove_target` has no panic path, so the
        // only way to obtain a real `JoinError` is to await a task that does.
        let join_error = tokio::task::spawn_blocking(|| panic!("worker exploded"))
            .await
            .expect_err("the worker panicked");
        assert!(absorb_outcome(Err(join_error), &mut removed, &mut total).is_none());

        assert!(removed.is_empty());
        assert_eq!(total, 0);
    }

    #[tokio::test]
    async fn cleanup_with_progress_is_silent_when_there_is_nothing_to_remove() {
        assert_eq!(cleanup_with_progress(Vec::new()).await, "");
    }

    #[tokio::test]
    async fn cleanup_with_progress_removes_every_target_and_summarises_once() {
        let dir = TempDir::new().unwrap();
        let lockfile = write_file(dir.path(), "bun.lock", &[0u8; 2048]);
        let installed = dir.path().join("node_modules");
        fs::create_dir(&installed).unwrap();
        write_file(&installed, "index.js", &[0u8; 1024]);
        // A target that is already gone and one that cannot be removed must
        // both be tolerated without aborting the run.
        let absent = dir.path().join("absent.lock");
        let undeletable = dir.path().join("target");
        fs::create_dir(&undeletable).unwrap();

        let summary = cleanup_with_progress(vec![
            CleanupTarget {
                path: lockfile.clone(),
                label: "app:bun.lock".to_owned(),
                kind: CleanupKind::Lockfile,
            },
            CleanupTarget {
                path: installed.clone(),
                label: "app:node_modules/".to_owned(),
                kind: CleanupKind::InstalledDir,
            },
            CleanupTarget {
                path: absent,
                label: "app:absent.lock".to_owned(),
                kind: CleanupKind::Lockfile,
            },
            CleanupTarget {
                path: undeletable.clone(),
                label: "app:target".to_owned(),
                kind: CleanupKind::Lockfile,
            },
        ])
        .await;

        assert!(!lockfile.exists());
        assert!(!installed.exists());
        assert!(undeletable.exists(), "the failing target must survive");

        // Only the two successful removals are listed, sorted by label, and
        // the total is their sum.
        let lines: Vec<&str> = summary.lines().collect();
        assert_eq!(lines.len(), 3, "got: {summary}");
        assert!(lines[0].contains("app:bun.lock"));
        assert!(lines[1].contains("app:node_modules/"));
        assert!(
            lines[2].contains("Total removed 3.00 KiB"),
            "got: {summary}"
        );
    }

    #[test]
    fn targets_for_job_yields_nothing_for_a_manifest_without_a_parent() {
        let job = ManifestJob {
            manifest_ref: ManifestRef {
                // An empty path has no parent directory to clean up beside.
                path: PathBuf::new(),
                kind: ManifestKind::PackageJson,
            },
            display_path: String::new(),
            text: String::new(),
            handler: &dependency_check_updates_node::NodeHandler,
            deps: Vec::new(),
        };

        assert!(targets_for_job(&job, true, true).is_empty());
    }

    #[test]
    fn render_cleanup_summary_is_empty_when_nothing_was_removed() {
        assert_eq!(render_cleanup_summary(&mut [], 0), "");
    }

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
