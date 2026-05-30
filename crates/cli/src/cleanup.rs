use std::path::Path;

use tracing::warn;

use dependency_check_updates_core::ManifestKind;

use crate::cli::Cli;
use crate::run::ManifestJob;

/// Lockfiles that sit next to a manifest of the given kind.
///
/// These are the files `--remove-lockfile` clears. The intent is to force the
/// downstream package manager to re-resolve every transitive dependency on
/// the next install, so that `dcu -u --remove-lockfile` is a true "update
/// everything, including dep-of-dep" operation rather than just the
/// top-level entries written into the manifest.
#[must_use]
pub(crate) fn lockfiles_for(kind: ManifestKind) -> &'static [&'static str] {
    match kind {
        ManifestKind::PackageJson => &[
            "bun.lock",
            "bun.lockb",
            "package-lock.json",
            "pnpm-lock.yaml",
            "yarn.lock",
        ],
        ManifestKind::CargoToml => &["Cargo.lock"],
        ManifestKind::PyProjectToml => &["uv.lock", "poetry.lock", "Pipfile.lock"],
        // Workflow files have no companion lockfile.
        ManifestKind::GitHubWorkflow => &[],
    }
}

/// Installed-dependency directories that sit next to a manifest of the given
/// kind.
///
/// `--remove-installed` wipes these so the package manager performs a clean
/// install. Without this step, an already-installed copy of a transitive
/// dependency can pin the resolver back to its old version even after the
/// lockfile is gone (bun/pnpm/uv all exhibit this).
#[must_use]
pub(crate) fn installed_dirs_for(kind: ManifestKind) -> &'static [&'static str] {
    match kind {
        ManifestKind::PackageJson => &["node_modules"],
        ManifestKind::CargoToml => &["target"],
        ManifestKind::PyProjectToml => &[".venv", "venv"],
        ManifestKind::GitHubWorkflow => &[],
    }
}

/// Delete sibling lockfiles and/or installed-dep directories next to a
/// manifest. Missing entries are silently skipped — the goal is idempotency,
/// not strictness.
///
/// Returns the display names (lockfiles as-is, directories with a trailing
/// `/`) of every entry actually removed, in the order they were processed.
/// The caller uses this list to print a per-manifest summary.
pub(crate) fn cleanup_manifest_siblings(
    manifest_path: &Path,
    kind: ManifestKind,
    remove_lockfile: bool,
    remove_installed: bool,
) -> Vec<String> {
    let mut removed = Vec::new();

    if !remove_lockfile && !remove_installed {
        return removed;
    }

    let Some(dir) = manifest_path.parent() else {
        return removed;
    };

    if remove_lockfile {
        for name in lockfiles_for(kind) {
            let path = dir.join(name);
            match std::fs::remove_file(&path) {
                Ok(()) => removed.push((*name).to_owned()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "failed to remove lockfile");
                }
            }
        }
    }

    if remove_installed {
        for name in installed_dirs_for(kind) {
            let path = dir.join(name);
            match std::fs::remove_dir_all(&path) {
                Ok(()) => removed.push(format!("{name}/")),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "failed to remove installed directory");
                }
            }
        }
    }

    removed
}

/// Render the " Removed <name>\n" lines for a list of deleted siblings.
/// Returns an empty string when nothing was removed so the caller can print
/// it unconditionally without producing a stray blank line.
#[must_use]
pub(crate) fn render_removed(removed: &[String]) -> String {
    let mut out = String::new();
    for name in removed {
        let _ = std::fmt::Write::write_fmt(&mut out, format_args!(" Removed {name}\n"));
    }
    out
}

/// Convenience wrapper: run the sibling cleanup for a job and render the
/// resulting summary string in one call.
///
/// Reads the effective removal flags via [`Cli::remove_lockfile_requested`]
/// and [`Cli::remove_installed_requested`] so the `--rm` shortcut and the
/// granular flags share one OR-semantics implementation.
#[cfg(not(tarpaulin_include))]
pub(crate) fn cleanup_and_render(job: &ManifestJob, cli: &Cli) -> String {
    let removed = cleanup_manifest_siblings(
        &job.manifest_ref.path,
        job.manifest_ref.kind,
        cli.remove_lockfile_requested(),
        cli.remove_installed_requested(),
    );
    render_removed(&removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------- Cleanup helpers --------

    #[test]
    fn test_lockfiles_for_each_kind() {
        // PackageJson covers every major Node lockfile variant.
        let node = lockfiles_for(ManifestKind::PackageJson);
        assert!(node.contains(&"bun.lock"));
        assert!(node.contains(&"bun.lockb"));
        assert!(node.contains(&"package-lock.json"));
        assert!(node.contains(&"pnpm-lock.yaml"));
        assert!(node.contains(&"yarn.lock"));

        assert_eq!(lockfiles_for(ManifestKind::CargoToml), &["Cargo.lock"]);

        let py = lockfiles_for(ManifestKind::PyProjectToml);
        assert!(py.contains(&"uv.lock"));
        assert!(py.contains(&"poetry.lock"));
        assert!(py.contains(&"Pipfile.lock"));

        assert!(lockfiles_for(ManifestKind::GitHubWorkflow).is_empty());
    }

    #[test]
    fn test_installed_dirs_for_each_kind() {
        assert_eq!(
            installed_dirs_for(ManifestKind::PackageJson),
            &["node_modules"]
        );
        assert_eq!(installed_dirs_for(ManifestKind::CargoToml), &["target"]);

        let py = installed_dirs_for(ManifestKind::PyProjectToml);
        assert!(py.contains(&".venv"));
        assert!(py.contains(&"venv"));

        assert!(installed_dirs_for(ManifestKind::GitHubWorkflow).is_empty());
    }

    #[test]
    fn test_render_removed_empty_returns_empty_string() {
        // No removals → no output (caller prints unconditionally).
        assert_eq!(render_removed(&[]), "");
    }

    #[test]
    fn test_render_removed_formats_each_entry_on_its_own_line() {
        let lines = render_removed(&["Cargo.lock".to_owned(), "target/".to_owned()]);
        assert_eq!(lines, " Removed Cargo.lock\n Removed target/\n");
    }

    #[test]
    fn test_cleanup_manifest_siblings_both_flags_off_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        std::fs::write(&manifest, "").unwrap();
        let lock = dir.path().join("Cargo.lock");
        std::fs::write(&lock, "").unwrap();

        let removed =
            cleanup_manifest_siblings(&manifest, ManifestKind::CargoToml, false, false);

        assert!(removed.is_empty());
        assert!(lock.exists(), "Cargo.lock must be untouched when both flags are off");
    }

    #[test]
    fn test_cleanup_manifest_siblings_removes_existing_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        std::fs::write(&manifest, "").unwrap();
        let lock = dir.path().join("Cargo.lock");
        std::fs::write(&lock, "# whatever").unwrap();

        let removed = cleanup_manifest_siblings(&manifest, ManifestKind::CargoToml, true, false);

        assert_eq!(removed, vec!["Cargo.lock".to_owned()]);
        assert!(!lock.exists());
    }

    #[test]
    fn test_cleanup_manifest_siblings_silently_skips_missing_lockfile() {
        // No Cargo.lock present — must succeed and report nothing removed.
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        std::fs::write(&manifest, "").unwrap();

        let removed = cleanup_manifest_siblings(&manifest, ManifestKind::CargoToml, true, true);

        assert!(removed.is_empty());
    }

    #[test]
    fn test_cleanup_manifest_siblings_removes_node_modules() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("package.json");
        std::fs::write(&manifest, "{}").unwrap();
        let nm = dir.path().join("node_modules");
        std::fs::create_dir_all(nm.join("react")).unwrap();
        std::fs::write(nm.join("react").join("index.js"), "").unwrap();

        let removed =
            cleanup_manifest_siblings(&manifest, ManifestKind::PackageJson, false, true);

        assert_eq!(removed, vec!["node_modules/".to_owned()]);
        assert!(!nm.exists(), "node_modules must be removed recursively");
    }

    #[test]
    fn test_cleanup_manifest_siblings_removes_lockfile_and_installed_together() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("package.json");
        std::fs::write(&manifest, "{}").unwrap();
        std::fs::write(dir.path().join("bun.lock"), "").unwrap();
        std::fs::write(dir.path().join("yarn.lock"), "").unwrap();
        std::fs::create_dir_all(dir.path().join("node_modules")).unwrap();

        let removed =
            cleanup_manifest_siblings(&manifest, ManifestKind::PackageJson, true, true);

        // Lockfiles come first (in their declared order), then installed dirs.
        assert!(removed.contains(&"bun.lock".to_owned()));
        assert!(removed.contains(&"yarn.lock".to_owned()));
        assert!(removed.contains(&"node_modules/".to_owned()));
        // Lockfiles must appear before installed dirs.
        let lock_idx = removed.iter().position(|n| n == "bun.lock").unwrap();
        let nm_idx = removed.iter().position(|n| n == "node_modules/").unwrap();
        assert!(lock_idx < nm_idx, "lockfiles should be listed before installed dirs");

        assert!(!dir.path().join("bun.lock").exists());
        assert!(!dir.path().join("yarn.lock").exists());
        assert!(!dir.path().join("node_modules").exists());
    }

    #[test]
    fn test_cleanup_manifest_siblings_does_not_touch_unrelated_lockfiles() {
        // A Cargo manifest must NEVER delete bun.lock or pnpm-lock.yaml, even
        // when both --remove-lockfile and --remove-installed are set.
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        std::fs::write(&manifest, "").unwrap();
        std::fs::write(dir.path().join("Cargo.lock"), "").unwrap();
        let foreign_lock = dir.path().join("bun.lock");
        std::fs::write(&foreign_lock, "").unwrap();
        let foreign_dir = dir.path().join("node_modules");
        std::fs::create_dir_all(&foreign_dir).unwrap();

        let removed = cleanup_manifest_siblings(&manifest, ManifestKind::CargoToml, true, true);

        assert!(removed.contains(&"Cargo.lock".to_owned()));
        assert!(
            !removed.iter().any(|n| n == "bun.lock"),
            "Cargo cleanup must not touch bun.lock"
        );
        assert!(foreign_lock.exists(), "bun.lock must survive Cargo cleanup");
        assert!(foreign_dir.exists(), "node_modules must survive Cargo cleanup");
    }

    #[test]
    fn test_cleanup_manifest_siblings_github_workflow_is_noop() {
        // Workflows have no companion lockfile or installed dir — both flags
        // must produce zero removals and zero side effects.
        let dir = tempfile::tempdir().unwrap();
        let workflows = dir.path().join(".github").join("workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        let manifest = workflows.join("CI.yml");
        std::fs::write(&manifest, "name: CI\n").unwrap();
        // A stray bun.lock next to the workflow must not be touched.
        let stray = workflows.join("bun.lock");
        std::fs::write(&stray, "").unwrap();

        let removed =
            cleanup_manifest_siblings(&manifest, ManifestKind::GitHubWorkflow, true, true);

        assert!(removed.is_empty());
        assert!(stray.exists());
    }

}
