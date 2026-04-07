use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The kind of package manifest file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ManifestKind {
    PackageJson,
    CargoToml,
    PyProjectToml,
}

impl ManifestKind {
    /// Detect manifest kind from a file path.
    #[must_use]
    pub fn from_path(path: &std::path::Path) -> Option<Self> {
        match path.file_name()?.to_str()? {
            "package.json" => Some(Self::PackageJson),
            "Cargo.toml" => Some(Self::CargoToml),
            "pyproject.toml" => Some(Self::PyProjectToml),
            _ => None,
        }
    }
}

impl std::fmt::Display for ManifestKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PackageJson => write!(f, "package.json"),
            Self::CargoToml => write!(f, "Cargo.toml"),
            Self::PyProjectToml => write!(f, "pyproject.toml"),
        }
    }
}

/// A reference to a manifest file on disk.
#[derive(Debug, Clone)]
pub struct ManifestRef {
    pub path: PathBuf,
    pub kind: ManifestKind,
}

/// Which dependency section a dependency belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DependencySection {
    // Node.js (package.json)
    Dependencies,
    DevDependencies,
    PeerDependencies,
    OptionalDependencies,
    // Rust (Cargo.toml)
    BuildDependencies,
    WorkspaceDependencies,
    // Python (pyproject.toml)
    ProjectDependencies,
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
    pub name: String,
    pub current_req: String,
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
    pub name: String,
    pub section: DependencySection,
    pub from: String,
    pub to: String,
}

/// The type of version bump.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BumpType {
    Major,
    Minor,
    Patch,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn test_manifest_kind_from_path_package_json() {
        let path = std::path::Path::new("package.json");
        assert_eq!(
            ManifestKind::from_path(path),
            Some(ManifestKind::PackageJson)
        );
    }

    #[test]
    fn test_manifest_kind_from_path_unknown() {
        let path = std::path::Path::new("unknown.txt");
        assert_eq!(ManifestKind::from_path(path), None);
    }

    #[test]
    fn test_target_level_from_str_patch() {
        assert_eq!(TargetLevel::from_str("patch"), Ok(TargetLevel::Patch));
    }

    #[test]
    fn test_target_level_from_str_case_insensitive() {
        assert_eq!(TargetLevel::from_str("LATEST"), Ok(TargetLevel::Latest));
        assert_eq!(TargetLevel::from_str("MiNoR"), Ok(TargetLevel::Minor));
    }

    #[test]
    fn test_target_level_from_str_invalid() {
        let result = TargetLevel::from_str("invalid");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown target level"));
    }

    #[test]
    fn test_dependency_section_label() {
        assert_eq!(DependencySection::Dependencies.label(), "dependencies");
        assert_eq!(
            DependencySection::DevDependencies.label(),
            "devDependencies"
        );
        assert_eq!(
            DependencySection::BuildDependencies.label(),
            "build-dependencies"
        );
        assert_eq!(
            DependencySection::WorkspaceDependencies.label(),
            "workspace.dependencies"
        );
        assert_eq!(
            DependencySection::ProjectDependencies.label(),
            "project.dependencies"
        );
    }

    #[test]
    fn test_target_level_display_roundtrip() {
        let levels = vec![
            TargetLevel::Patch,
            TargetLevel::Minor,
            TargetLevel::Latest,
            TargetLevel::Newest,
            TargetLevel::Greatest,
        ];

        for level in levels {
            let displayed = level.to_string();
            let parsed = TargetLevel::from_str(&displayed).unwrap();
            assert_eq!(level, parsed);
        }
    }

    #[test]
    fn test_manifest_kind_display() {
        assert_eq!(ManifestKind::PackageJson.to_string(), "package.json");
    }

    #[test]
    fn test_manifest_kind_display_cargo_toml() {
        assert_eq!(ManifestKind::CargoToml.to_string(), "Cargo.toml");
    }

    #[test]
    fn test_manifest_kind_display_pyproject_toml() {
        assert_eq!(ManifestKind::PyProjectToml.to_string(), "pyproject.toml");
    }

    #[test]
    fn test_manifest_kind_from_path_cargo_toml() {
        let path = std::path::Path::new("Cargo.toml");
        assert_eq!(ManifestKind::from_path(path), Some(ManifestKind::CargoToml));
    }

    #[test]
    fn test_manifest_kind_from_path_pyproject_toml() {
        let path = std::path::Path::new("pyproject.toml");
        assert_eq!(
            ManifestKind::from_path(path),
            Some(ManifestKind::PyProjectToml)
        );
    }

    #[test]
    fn test_dependency_section_display() {
        assert_eq!(DependencySection::Dependencies.to_string(), "dependencies");
        assert_eq!(
            DependencySection::DevDependencies.to_string(),
            "devDependencies"
        );
        assert_eq!(
            DependencySection::PeerDependencies.to_string(),
            "peerDependencies"
        );
        assert_eq!(
            DependencySection::OptionalDependencies.to_string(),
            "optionalDependencies"
        );
        assert_eq!(
            DependencySection::BuildDependencies.to_string(),
            "build-dependencies"
        );
        assert_eq!(
            DependencySection::WorkspaceDependencies.to_string(),
            "workspace.dependencies"
        );
        assert_eq!(
            DependencySection::ProjectDependencies.to_string(),
            "project.dependencies"
        );
    }

    #[test]
    fn test_dependency_section_label_peer_optional() {
        assert_eq!(
            DependencySection::PeerDependencies.label(),
            "peerDependencies"
        );
        assert_eq!(
            DependencySection::OptionalDependencies.label(),
            "optionalDependencies"
        );
    }
}
