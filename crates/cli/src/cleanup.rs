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
    use rstest::rstest;

    // -------- Cleanup helpers --------

    #[rstest]
    // (kind, every entry that MUST appear in the returned slice)
    #[case::package_json(
        ManifestKind::PackageJson,
        &["bun.lock", "bun.lockb", "package-lock.json", "pnpm-lock.yaml", "yarn.lock"],
    )]
    #[case::cargo_toml(ManifestKind::CargoToml, &["Cargo.lock"])]
    #[case::pyproject_toml(
        ManifestKind::PyProjectToml,
        &["uv.lock", "poetry.lock", "Pipfile.lock"],
    )]
    #[case::github_workflow(ManifestKind::GitHubWorkflow, &[])]
    fn lockfiles_for_cases(#[case] kind: ManifestKind, #[case] expected: &[&str]) {
        let got = lockfiles_for(kind);
        for needle in expected {
            assert!(
                got.contains(needle),
                "{got:?} should contain {needle:?} for {kind:?}"
            );
        }
        if expected.is_empty() {
            assert!(got.is_empty(), "{got:?} should be empty for {kind:?}");
        }
    }

    #[rstest]
    #[case::package_json(ManifestKind::PackageJson, &["node_modules"])]
    #[case::cargo_toml(ManifestKind::CargoToml, &["target"])]
    #[case::pyproject_toml(ManifestKind::PyProjectToml, &[".venv", "venv"])]
    #[case::github_workflow(ManifestKind::GitHubWorkflow, &[])]
    fn installed_dirs_for_cases(#[case] kind: ManifestKind, #[case] expected: &[&str]) {
        let got = installed_dirs_for(kind);
        for needle in expected {
            assert!(
                got.contains(needle),
                "{got:?} should contain {needle:?} for {kind:?}"
            );
        }
        if expected.is_empty() {
            assert!(got.is_empty(), "{got:?} should be empty for {kind:?}");
        }
    }

    #[rstest]
    // (entries handed to `render_removed`, exact expected output)
    #[case::empty_returns_empty_string(&[], "")]
    #[case::formats_each_entry_on_its_own_line(
        &["Cargo.lock", "target/"],
        " Removed Cargo.lock\n Removed target/\n",
    )]
    fn render_removed_cases(#[case] entries: &[&str], #[case] expected: &str) {
        let owned: Vec<String> = entries.iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(render_removed(&owned), expected);
    }

    // -------- cleanup_manifest_siblings scenarios --------

    /// `(remove_lockfile, remove_installed)` flag pair handed to
    /// [`cleanup_manifest_siblings`]. Bundled as a tuple alias so the
    /// parametrized test stays under `clippy::too_many_arguments`'s threshold
    /// (7) while keeping the individual case rows readable.
    type CleanupFlags = (bool, bool);

    #[rstest]
    // Cargo manifest, both flags off → nothing touched, Cargo.lock survives.
    #[case::cargo_both_flags_off_is_noop(
        ManifestKind::CargoToml, "Cargo.toml",
        &["Cargo.lock"], &[],
        (false, false),
        &[], &["Cargo.lock"],
    )]
    // Cargo manifest, lockfile flag on → Cargo.lock removed.
    #[case::cargo_removes_existing_lockfile(
        ManifestKind::CargoToml, "Cargo.toml",
        &["Cargo.lock"], &[],
        (true, false),
        &["Cargo.lock"], &[],
    )]
    // No lockfile present → silently skipped, no removals reported.
    #[case::cargo_silently_skips_missing_lockfile(
        ManifestKind::CargoToml, "Cargo.toml",
        &[], &[],
        (true, true),
        &[], &[],
    )]
    // Node manifest, installed flag on → node_modules/ removed recursively.
    #[case::node_removes_node_modules(
        ManifestKind::PackageJson, "package.json",
        &[], &["node_modules"],
        (false, true),
        &["node_modules/"], &[],
    )]
    // Both flags on: lockfiles first (in declared order), then installed dirs.
    // Exact-equality assertion on `expected_removed` verifies the ordering.
    #[case::node_removes_lockfile_and_installed_together(
        ManifestKind::PackageJson, "package.json",
        &["bun.lock", "yarn.lock"], &["node_modules"],
        (true, true),
        &["bun.lock", "yarn.lock", "node_modules/"], &[],
    )]
    // Cargo cleanup must NEVER delete foreign (node) lockfiles or dirs.
    #[case::cargo_does_not_touch_unrelated_lockfiles(
        ManifestKind::CargoToml, "Cargo.toml",
        &["Cargo.lock", "bun.lock"], &["node_modules"],
        (true, true),
        &["Cargo.lock"], &["bun.lock", "node_modules"],
    )]
    // GitHub workflows have no companion lockfile/dir — both flags are a noop.
    #[case::github_workflow_is_noop(
        ManifestKind::GitHubWorkflow, ".github/workflows/CI.yml",
        &["bun.lock"], &[],
        (true, true),
        &[], &["bun.lock"],
    )]
    fn cleanup_manifest_siblings_cases(
        #[case] kind: ManifestKind,
        #[case] manifest_rel: &str,
        #[case] seed_lockfiles: &[&str],
        #[case] seed_install_dirs: &[&str],
        #[case] flags: CleanupFlags,
        #[case] expected_removed: &[&str],
        #[case] expected_surviving: &[&str],
    ) {
        let (remove_lockfile, remove_installed) = flags;

        let tmp = tempfile::tempdir().unwrap();
        let manifest = tmp.path().join(manifest_rel);
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        std::fs::write(&manifest, "").unwrap();
        let parent = manifest.parent().unwrap();

        for name in seed_lockfiles {
            std::fs::write(parent.join(name), "").unwrap();
        }
        for name in seed_install_dirs {
            // Seed a child file inside so the recursive-remove path is exercised.
            let dir = parent.join(name);
            std::fs::create_dir_all(dir.join("child")).unwrap();
            std::fs::write(dir.join("child").join("file"), "").unwrap();
        }

        let removed =
            cleanup_manifest_siblings(&manifest, kind, remove_lockfile, remove_installed);

        let expected_vec: Vec<String> =
            expected_removed.iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(
            removed, expected_vec,
            "removed list mismatch (order matters)"
        );

        for name in expected_surviving {
            assert!(
                parent.join(name).exists(),
                "{name} must survive cleanup but is gone"
            );
        }
        for name in expected_removed {
            // `node_modules/` display name maps back to `node_modules` on disk.
            let bare = name.trim_end_matches('/');
            assert!(
                !parent.join(bare).exists(),
                "{bare} should have been removed"
            );
        }
    }

    // -------- Early-return + warn-arm error paths --------

    /// Covers the `manifest_path.parent() == None` early-return branch:
    /// `Path::new("").parent()` is `None`, so the function returns an empty
    /// Vec without touching the filesystem.
    #[test]
    fn cleanup_returns_empty_when_manifest_has_no_parent() {
        let removed = cleanup_manifest_siblings(
            Path::new(""),
            ManifestKind::CargoToml,
            true,
            true,
        );
        assert!(
            removed.is_empty(),
            "expected empty removal list for parent-less path, got {removed:?}"
        );
    }

    /// Covers the `Err(e) => warn!(...)` arm of `remove_file` for a
    /// non-`NotFound` error: a *directory* named `Cargo.lock` sits where a
    /// lockfile would. `std::fs::remove_file` refuses to delete a directory,
    /// returning an error whose kind is not `NotFound`, so the warn arm
    /// fires. The directory must survive and must NOT appear in `removed`.
    #[test]
    fn cleanup_lockfile_warns_when_remove_file_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = tmp.path().join("Cargo.toml");
        std::fs::write(&manifest, "").unwrap();

        // Lockfile slot occupied by a *directory* — remove_file will fail.
        let lock_as_dir = tmp.path().join("Cargo.lock");
        std::fs::create_dir(&lock_as_dir).unwrap();

        let removed = cleanup_manifest_siblings(
            &manifest,
            ManifestKind::CargoToml,
            true,
            false,
        );

        assert!(
            removed.is_empty(),
            "remove_file failure must not push to removed, got {removed:?}"
        );
        assert!(
            lock_as_dir.exists() && lock_as_dir.is_dir(),
            "Cargo.lock directory must survive the failed remove_file"
        );
    }

    /// Covers the `Err(e) => warn!(...)` arm of `remove_dir_all` for a
    /// non-`NotFound` error: a regular *file* named `target` sits where the
    /// installed-deps directory would. `std::fs::remove_dir_all` cannot
    /// recurse into a non-directory and returns a non-`NotFound` error, so
    /// the warn arm fires. The file must survive and must NOT appear in
    /// `removed`.
    #[test]
    fn cleanup_installed_warns_when_remove_dir_all_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = tmp.path().join("Cargo.toml");
        std::fs::write(&manifest, "").unwrap();

        // Installed-dir slot occupied by a regular *file* — remove_dir_all fails.
        let target_as_file = tmp.path().join("target");
        std::fs::write(&target_as_file, "not a directory").unwrap();

        let removed = cleanup_manifest_siblings(
            &manifest,
            ManifestKind::CargoToml,
            false,
            true,
        );

        assert!(
            removed.is_empty(),
            "remove_dir_all failure must not push to removed, got {removed:?}"
        );
        assert!(
            target_as_file.exists() && target_as_file.is_file(),
            "`target` file must survive the failed remove_dir_all"
        );
    }
}
