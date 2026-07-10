use dependency_check_updates_core::ManifestKind;

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

/// Installed-dependency or generated environment directories that sit next to a
/// manifest of the given kind.
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
        ManifestKind::PyProjectToml => &[".venv", "venv", "__pypackages__", ".tox", ".nox"],
        ManifestKind::GitHubWorkflow => &[],
    }
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
    #[case::pyproject_toml(
        ManifestKind::PyProjectToml,
        &[".venv", "venv", "__pypackages__", ".tox", ".nox"]
    )]
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
}
