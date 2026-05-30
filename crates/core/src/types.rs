//! Core domain types shared across every ecosystem crate: manifest kinds,
//! dependency sections, version targets, and the parsed/resolved/planned
//! value objects that flow through the scan → resolve → patch pipeline.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The kind of package manifest file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ManifestKind {
    /// Node.js `package.json`.
    PackageJson,
    /// Rust `Cargo.toml`.
    CargoToml,
    /// Python `pyproject.toml`.
    PyProjectToml,
    /// GitHub Actions workflow (`.github/workflows/*.yml` /`*.yaml`) or
    /// composite action definition (`action.yml` / `action.yaml`).
    GitHubWorkflow,
}

impl ManifestKind {
    /// Detect manifest kind from a file path.
    ///
    /// GitHub workflow detection requires the parent directory context because
    /// arbitrary `*.yml` files exist throughout repos and only files under
    /// `.github/workflows/` or named `action.yml`/`action.yaml` are treated as
    /// workflow manifests.
    #[must_use]
    pub fn from_path(path: &std::path::Path) -> Option<Self> {
        let file_name = path.file_name()?.to_str()?;
        match file_name {
            "package.json" => Some(Self::PackageJson),
            "Cargo.toml" => Some(Self::CargoToml),
            "pyproject.toml" => Some(Self::PyProjectToml),
            "action.yml" | "action.yaml" => Some(Self::GitHubWorkflow),
            _ => {
                // Workflow YAMLs live in `.github/workflows/`.
                if matches!(
                    path.extension().and_then(|s| s.to_str()),
                    Some("yml" | "yaml")
                ) && path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|s| s.to_str())
                    == Some("workflows")
                    && path
                        .parent()
                        .and_then(std::path::Path::parent)
                        .and_then(|p| p.file_name())
                        .and_then(|s| s.to_str())
                        == Some(".github")
                {
                    Some(Self::GitHubWorkflow)
                } else {
                    None
                }
            }
        }
    }
}

impl std::fmt::Display for ManifestKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PackageJson => write!(f, "package.json"),
            Self::CargoToml => write!(f, "Cargo.toml"),
            Self::PyProjectToml => write!(f, "pyproject.toml"),
            Self::GitHubWorkflow => write!(f, "GitHub workflow"),
        }
    }
}

/// A reference to a manifest file on disk.
#[derive(Debug, Clone)]
pub struct ManifestRef {
    /// Filesystem path to the manifest.
    pub path: PathBuf,
    /// Which manifest format this file is.
    pub kind: ManifestKind,
}

/// Which dependency section a dependency belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DependencySection {
    /// Node.js `dependencies`.
    Dependencies,
    /// Node.js `devDependencies`.
    DevDependencies,
    /// Node.js `peerDependencies`.
    PeerDependencies,
    /// Node.js `optionalDependencies`.
    OptionalDependencies,
    /// Rust `[build-dependencies]`.
    BuildDependencies,
    /// Rust `[workspace.dependencies]`.
    WorkspaceDependencies,
    /// Python `[project] dependencies`.
    ProjectDependencies,
    /// GitHub Actions `uses:` directives in workflows / composite actions.
    GitHubActions,
}

impl DependencySection {
    /// Human-readable label for this section.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Dependencies => "dependencies",
            Self::DevDependencies => "devDependencies",
            Self::PeerDependencies => "peerDependencies",
            Self::OptionalDependencies => "optionalDependencies",
            Self::BuildDependencies => "build-dependencies",
            Self::WorkspaceDependencies => "workspace.dependencies",
            Self::ProjectDependencies => "project.dependencies",
            Self::GitHubActions => "uses",
        }
    }
}

impl std::fmt::Display for DependencySection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.label())
    }
}

/// A dependency found in a manifest file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencySpec {
    /// Package name as written in the manifest.
    pub name: String,
    /// Current version requirement string (range prefix preserved).
    pub current_req: String,
    /// Section the dependency was found in.
    pub section: DependencySection,
}

/// The target level for version updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TargetLevel {
    /// Only patch version bumps (e.g., 1.0.1 -> 1.0.2)
    Patch,
    /// Minor version bumps (e.g., 1.0.0 -> 1.1.0)
    Minor,
    /// Latest stable version (default)
    #[default]
    Latest,
    /// Most recently published version by date
    Newest,
    /// Highest version number
    Greatest,
}

impl std::fmt::Display for TargetLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Patch => write!(f, "patch"),
            Self::Minor => write!(f, "minor"),
            Self::Latest => write!(f, "latest"),
            Self::Newest => write!(f, "newest"),
            Self::Greatest => write!(f, "greatest"),
        }
    }
}

impl std::str::FromStr for TargetLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "patch" => Ok(Self::Patch),
            "minor" => Ok(Self::Minor),
            "latest" => Ok(Self::Latest),
            "newest" => Ok(Self::Newest),
            "greatest" => Ok(Self::Greatest),
            other => Err(format!("unknown target level: {other}")),
        }
    }
}

/// Resolved version information from a registry.
#[derive(Debug, Clone)]
pub struct ResolvedVersion {
    /// The latest version available (dist-tags.latest for npm).
    pub latest: Option<String>,
    /// The selected version based on target level.
    pub selected: Option<String>,
}

/// A planned update for a single dependency.
#[derive(Debug, Clone)]
pub struct PlannedUpdate {
    /// Package name being updated.
    pub name: String,
    /// Section the dependency lives in.
    pub section: DependencySection,
    /// Current version requirement string.
    pub from: String,
    /// New version requirement string to write.
    pub to: String,
}

/// The type of version bump.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BumpType {
    /// Major version change (e.g. `1.x → 2.x`).
    Major,
    /// Minor version change (e.g. `1.0 → 1.1`).
    Minor,
    /// Patch version change (e.g. `1.0.0 → 1.0.1`).
    Patch,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use std::str::FromStr;

    /// Detection of [`ManifestKind`] from a file path. `None` cases prove that
    /// random `.yml` files outside `.github/workflows/` and unknown extensions
    /// are ignored — the central guarantee of the dispatcher.
    #[rstest]
    #[case::package_json("package.json", Some(ManifestKind::PackageJson))]
    #[case::cargo_toml("Cargo.toml", Some(ManifestKind::CargoToml))]
    #[case::pyproject_toml("pyproject.toml", Some(ManifestKind::PyProjectToml))]
    #[case::workflow_yml(".github/workflows/CI.yml", Some(ManifestKind::GitHubWorkflow))]
    #[case::workflow_yaml(".github/workflows/release.yaml", Some(ManifestKind::GitHubWorkflow))]
    #[case::action_yml(".github/actions/setup/action.yml", Some(ManifestKind::GitHubWorkflow))]
    #[case::action_yaml("path/to/action.yaml", Some(ManifestKind::GitHubWorkflow))]
    #[case::nested_workflow("repo/.github/workflows/test.yml", Some(ManifestKind::GitHubWorkflow))]
    #[case::unknown_extension("unknown.txt", None)]
    #[case::unrelated_yml_ignored("docker-compose.yml", None)]
    fn manifest_kind_from_path_cases(
        #[case] path: &str,
        #[case] expected: Option<ManifestKind>,
    ) {
        assert_eq!(
            ManifestKind::from_path(std::path::Path::new(path)),
            expected
        );
    }

    /// [`Display`] strings used by user-facing CLI output for every manifest
    /// kind.
    #[rstest]
    #[case::package_json(ManifestKind::PackageJson, "package.json")]
    #[case::cargo_toml(ManifestKind::CargoToml, "Cargo.toml")]
    #[case::pyproject_toml(ManifestKind::PyProjectToml, "pyproject.toml")]
    #[case::github_workflow(ManifestKind::GitHubWorkflow, "GitHub workflow")]
    fn manifest_kind_display_cases(#[case] kind: ManifestKind, #[case] expected: &str) {
        assert_eq!(kind.to_string(), expected);
    }

    /// [`TargetLevel::from_str`] is case-insensitive and rejects unknown
    /// values with a message that explains itself (`expected = None` cases).
    #[rstest]
    #[case::lower_patch("patch", Some(TargetLevel::Patch))]
    #[case::upper_latest("LATEST", Some(TargetLevel::Latest))]
    #[case::mixed_minor("MiNoR", Some(TargetLevel::Minor))]
    #[case::invalid_value("invalid", None)]
    fn target_level_from_str_cases(#[case] input: &str, #[case] expected: Option<TargetLevel>) {
        let result = TargetLevel::from_str(input);
        if let Some(level) = expected {
            assert_eq!(result, Ok(level));
        } else {
            let err = result.expect_err("expected an error for unknown target");
            assert!(
                err.contains("unknown target level"),
                "error must mention `unknown target level`, got: {err}",
            );
        }
    }

    /// `Display(level).parse()` must round-trip back to the same variant for
    /// every supported target.
    #[rstest]
    #[case(TargetLevel::Patch)]
    #[case(TargetLevel::Minor)]
    #[case(TargetLevel::Latest)]
    #[case(TargetLevel::Newest)]
    #[case(TargetLevel::Greatest)]
    fn target_level_display_roundtrip(#[case] level: TargetLevel) {
        let displayed = level.to_string();
        let parsed = TargetLevel::from_str(&displayed).expect("display string must round-trip");
        assert_eq!(level, parsed);
    }

    /// `label()` is the wire-format identifier surfaced in JSON output and
    /// must match every section's canonical name.
    #[rstest]
    #[case::dependencies(DependencySection::Dependencies, "dependencies")]
    #[case::dev_dependencies(DependencySection::DevDependencies, "devDependencies")]
    #[case::peer_dependencies(DependencySection::PeerDependencies, "peerDependencies")]
    #[case::optional_dependencies(DependencySection::OptionalDependencies, "optionalDependencies")]
    #[case::build_dependencies(DependencySection::BuildDependencies, "build-dependencies")]
    #[case::workspace_dependencies(
        DependencySection::WorkspaceDependencies,
        "workspace.dependencies"
    )]
    #[case::project_dependencies(DependencySection::ProjectDependencies, "project.dependencies")]
    #[case::github_actions(DependencySection::GitHubActions, "uses")]
    fn dependency_section_label_cases(
        #[case] section: DependencySection,
        #[case] expected: &str,
    ) {
        assert_eq!(section.label(), expected);
    }

    /// [`Display`] mirrors `label()` — this case set guards both forms from
    /// drifting apart.
    #[rstest]
    #[case::dependencies(DependencySection::Dependencies, "dependencies")]
    #[case::dev_dependencies(DependencySection::DevDependencies, "devDependencies")]
    #[case::peer_dependencies(DependencySection::PeerDependencies, "peerDependencies")]
    #[case::optional_dependencies(DependencySection::OptionalDependencies, "optionalDependencies")]
    #[case::build_dependencies(DependencySection::BuildDependencies, "build-dependencies")]
    #[case::workspace_dependencies(
        DependencySection::WorkspaceDependencies,
        "workspace.dependencies"
    )]
    #[case::project_dependencies(DependencySection::ProjectDependencies, "project.dependencies")]
    fn dependency_section_display_cases(
        #[case] section: DependencySection,
        #[case] expected: &str,
    ) {
        assert_eq!(section.to_string(), expected);
    }
}
