//! Shared manifest handling abstractions.
//!
//! Each language crate (dependency-check-updates-node, dependency-check-updates-rust, dependency-check-updates-python) implements these
//! traits for its specific manifest format and registry.

use std::path::{Path, PathBuf};

use tracing::debug;

use crate::error::DcuError;
use crate::types::{
    DependencySpec, ManifestKind, ManifestRef, PlannedUpdate, ResolvedVersion, TargetLevel,
};

// ---------------------------------------------------------------------------
// ManifestHandler — parse manifests and apply updates
// ---------------------------------------------------------------------------

/// A handler for a specific manifest file format.
///
/// Each language crate provides an implementation:
/// - `dependency-check-updates-node`   → `package.json`
/// - `dependency-check-updates-rust`   → `Cargo.toml`
/// - `dependency-check-updates-python` → `pyproject.toml`
pub trait ManifestHandler {
    /// Parse a manifest file from raw text and collect its dependencies.
    ///
    /// # Errors
    ///
    /// Returns an error if the text cannot be parsed.
    fn parse(&self, text: &str, path: &Path) -> Result<ParsedManifest, DcuError>;

    /// Apply planned updates to the original text, returning modified text.
    ///
    /// Must preserve formatting (comments, indentation, line endings).
    ///
    /// # Errors
    ///
    /// Returns an error if the updates cannot be applied.
    fn apply_updates(&self, text: &str, updates: &[PlannedUpdate]) -> Result<String, DcuError>;
}

/// The result of parsing a manifest file.
#[derive(Debug, Clone)]
pub struct ParsedManifest {
    /// Reference to the manifest file.
    pub manifest_ref: ManifestRef,
    /// The original raw text (preserved for patching).
    pub original_text: String,
    /// Collected dependencies.
    pub dependencies: Vec<DependencySpec>,
}

// ---------------------------------------------------------------------------
// RegistryClient — resolve versions from a package registry
// ---------------------------------------------------------------------------

/// A client for a package registry (npm, crates.io, `PyPI`).
///
/// Each language crate provides an implementation.
/// Uses async methods for network I/O.
pub trait RegistryClient: Send + Sync {
    /// Resolve the target version for a single dependency.
    fn resolve_version(
        &self,
        dep: &DependencySpec,
        target: TargetLevel,
    ) -> impl std::future::Future<Output = Result<ResolvedVersion, DcuError>> + Send;

    /// Resolve versions for a batch of dependencies concurrently.
    fn resolve_batch(
        &self,
        deps: &[DependencySpec],
        target: TargetLevel,
    ) -> impl std::future::Future<Output = Vec<(usize, Result<ResolvedVersion, DcuError>)>> + Send;
}

// ---------------------------------------------------------------------------
// Scanner — discover manifest files
// ---------------------------------------------------------------------------

/// Discover manifest files in a directory.
pub struct Scanner;

impl Scanner {
    /// Find manifest files in the given directory (non-recursive).
    ///
    /// Returns all recognized manifests at the root level (`package.json`,
    /// `Cargo.toml`, `pyproject.toml`, `action.yml` / `action.yaml`) and every
    /// `*.yml`/`*.yaml` directly under `.github/workflows/`. The root-level
    /// `action.yml` is included so that authors of single-action repos see
    /// their own manifest without needing `-d`.
    #[must_use]
    pub fn scan_dir(root: &Path) -> Vec<ManifestRef> {
        let mut manifests = Vec::new();

        let candidates = [
            "package.json",
            "Cargo.toml",
            "pyproject.toml",
            "action.yml",
            "action.yaml",
        ];

        for filename in &candidates {
            let path = root.join(filename);
            if path.is_file() {
                if let Some(kind) = ManifestKind::from_path(&path) {
                    manifests.push(ManifestRef { path, kind });
                }
            }
        }

        // GitHub Actions: enumerate `.github/workflows/*.yml`/`*.yaml`.
        let workflows_dir = root.join(".github").join("workflows");
        if let Ok(entries) = std::fs::read_dir(&workflows_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                if let Some(kind) = ManifestKind::from_path(&path) {
                    manifests.push(ManifestRef { path, kind });
                }
            }
            // Stable order so output is reproducible across platforms — `read_dir`
            // is OS-dependent (NTFS vs ext4 give different orderings).
            manifests.sort_by(|a, b| a.path.cmp(&b.path));
        }

        manifests
    }

    /// Find a specific manifest file.
    ///
    /// # Errors
    ///
    /// Returns an error if the file does not exist or is not a recognized manifest.
    pub fn from_path(path: &Path) -> Result<ManifestRef, DcuError> {
        if !path.is_file() {
            return Err(DcuError::NoManifest {
                path: path.to_path_buf(),
            });
        }

        let kind = ManifestKind::from_path(path).ok_or_else(|| DcuError::NoManifest {
            path: path.to_path_buf(),
        })?;

        Ok(ManifestRef {
            path: path.to_path_buf(),
            kind,
        })
    }

    /// Recursively find manifest files using the `ignore` crate.
    ///
    /// Respects `.gitignore`, `.ignore`, and skips common directories
    /// (`node_modules`, `target`, `.venv`, `dist`, `build`, `vendor`).
    /// Walks INTO `.github` even though it is a hidden directory because
    /// workflow YAMLs live there; without this exception deep scan would miss
    /// every GitHub Actions manifest.
    #[must_use]
    pub fn scan_deep(root: &Path) -> Vec<ManifestRef> {
        use ignore::WalkBuilder;

        let manifest_names: &[&str] = &[
            "package.json",
            "Cargo.toml",
            "pyproject.toml",
            "action.yml",
            "action.yaml",
        ];

        let walker = WalkBuilder::new(root)
            // `hidden(false)` so `.github/` is traversed. The filter_entry
            // below still skips other hidden dirs that are not interesting.
            .hidden(false)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .filter_entry(|entry| {
                let name = entry.file_name().to_string_lossy();
                // Skip common dependency/build directories and hidden dirs that
                // are NOT `.github`. The leading-dot check lets `.github` and
                // any descendants through while still pruning `.git`, `.venv`,
                // `.idea`, etc.
                if name.starts_with('.') && name.as_ref() != "." && name.as_ref() != ".github" {
                    return false;
                }
                !matches!(
                    name.as_ref(),
                    "node_modules" | "target" | "dist" | "build" | "vendor" | "__pycache__"
                )
            })
            .build();

        let mut manifests = Vec::new();

        for entry in walker.flatten() {
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }

            let path = entry.path();
            let file_name = entry.file_name().to_string_lossy();
            let is_workflow_yaml = matches!(
                path.extension().and_then(|s| s.to_str()),
                Some("yml" | "yaml")
            ) && path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                == Some("workflows")
                && path
                    .parent()
                    .and_then(Path::parent)
                    .and_then(|p| p.file_name())
                    .and_then(|s| s.to_str())
                    == Some(".github");

            if manifest_names.contains(&file_name.as_ref()) || is_workflow_yaml {
                let path = entry.into_path();
                if let Some(kind) = ManifestKind::from_path(&path) {
                    debug!(path = %path.display(), kind = %kind, "deep scan: found manifest");
                    manifests.push(ManifestRef { path, kind });
                }
            }
        }

        manifests.sort_by(|a, b| a.path.cmp(&b.path));
        manifests
    }

    /// Find manifests, either from a specific path or by scanning the directory.
    ///
    /// When `deep` is true, recursively scans subdirectories.
    ///
    /// # Errors
    ///
    /// Returns an error if `manifest_path` is set but invalid, or if no manifests
    /// are found in `root`.
    pub fn discover(
        root: &Path,
        manifest_path: Option<&Path>,
        deep: bool,
    ) -> Result<Vec<ManifestRef>, DcuError> {
        if let Some(path) = manifest_path {
            let resolved = if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            };
            return Ok(vec![Self::from_path(&resolved)?]);
        }

        let manifests = if deep {
            Self::scan_deep(root)
        } else {
            Self::scan_dir(root)
        };

        if manifests.is_empty() {
            return Err(DcuError::NoManifest {
                path: root.to_path_buf(),
            });
        }

        Ok(manifests)
    }
}

// ---------------------------------------------------------------------------
// ScanResult — output of the scan+resolve pipeline
// ---------------------------------------------------------------------------

/// Result of scanning and resolving a single manifest file.
#[derive(Debug)]
pub struct ScanResult {
    /// The manifest file that was scanned.
    pub manifest_ref: ManifestRef,
    /// Path to the manifest.
    pub path: PathBuf,
    /// Updates that can be applied.
    pub updates: Vec<PlannedUpdate>,
    /// Whether the file was actually modified (only true after apply).
    pub modified: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_temp_manifest(dir: &Path, filename: &str, content: &str) {
        fs::write(dir.join(filename), content).unwrap();
    }

    #[test]
    fn test_scan_dir_finds_package_json() {
        let dir = TempDir::new().unwrap();
        create_temp_manifest(dir.path(), "package.json", r#"{"name":"test"}"#);

        let manifests = Scanner::scan_dir(dir.path());
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].kind, ManifestKind::PackageJson);
    }

    #[test]
    fn test_scan_dir_finds_cargo_toml() {
        let dir = TempDir::new().unwrap();
        create_temp_manifest(dir.path(), "Cargo.toml", "[package]\nname = \"test\"");

        let manifests = Scanner::scan_dir(dir.path());
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].kind, ManifestKind::CargoToml);
    }

    #[test]
    fn test_scan_dir_finds_pyproject_toml() {
        let dir = TempDir::new().unwrap();
        create_temp_manifest(dir.path(), "pyproject.toml", "[project]\nname = \"test\"");

        let manifests = Scanner::scan_dir(dir.path());
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].kind, ManifestKind::PyProjectToml);
    }

    #[test]
    fn test_scan_dir_finds_all_three() {
        let dir = TempDir::new().unwrap();
        create_temp_manifest(dir.path(), "package.json", "{}");
        create_temp_manifest(dir.path(), "Cargo.toml", "[package]");
        create_temp_manifest(dir.path(), "pyproject.toml", "[project]");

        let manifests = Scanner::scan_dir(dir.path());
        assert_eq!(manifests.len(), 3);
    }

    #[test]
    fn test_scan_dir_empty() {
        let dir = TempDir::new().unwrap();
        let manifests = Scanner::scan_dir(dir.path());
        assert!(manifests.is_empty());
    }

    #[test]
    fn test_scan_dir_ignores_unknown_files() {
        let dir = TempDir::new().unwrap();
        create_temp_manifest(dir.path(), "README.md", "# Hello");
        create_temp_manifest(dir.path(), "build.gradle", "");

        let manifests = Scanner::scan_dir(dir.path());
        assert!(manifests.is_empty());
    }

    #[test]
    fn test_from_path_valid() {
        let dir = TempDir::new().unwrap();
        create_temp_manifest(dir.path(), "package.json", "{}");

        let result = Scanner::from_path(&dir.path().join("package.json"));
        assert!(result.is_ok());
        assert_eq!(result.unwrap().kind, ManifestKind::PackageJson);
    }

    #[test]
    fn test_from_path_not_found() {
        let result = Scanner::from_path(Path::new("/nonexistent/package.json"));
        assert!(result.is_err());
    }

    #[test]
    fn test_from_path_unknown_file() {
        let dir = TempDir::new().unwrap();
        create_temp_manifest(dir.path(), "build.gradle", "");

        let result = Scanner::from_path(&dir.path().join("build.gradle"));
        assert!(result.is_err());
    }

    #[test]
    fn test_discover_with_explicit_path() {
        let dir = TempDir::new().unwrap();
        create_temp_manifest(dir.path(), "package.json", "{}");

        let result = Scanner::discover(dir.path(), Some(Path::new("package.json")), false);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 1);
    }

    #[test]
    fn test_discover_auto_scan() {
        let dir = TempDir::new().unwrap();
        create_temp_manifest(dir.path(), "package.json", "{}");
        create_temp_manifest(dir.path(), "Cargo.toml", "[package]");

        let result = Scanner::discover(dir.path(), None, false);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 2);
    }

    #[test]
    fn test_discover_empty_dir_errors() {
        let dir = TempDir::new().unwrap();
        let result = Scanner::discover(dir.path(), None, false);
        assert!(result.is_err());
    }

    #[test]
    fn test_discover_deep_scan() {
        let dir = TempDir::new().unwrap();
        create_temp_manifest(dir.path(), "package.json", "{}");
        std::fs::create_dir_all(dir.path().join("packages/app")).unwrap();
        create_temp_manifest(&dir.path().join("packages/app"), "package.json", "{}");

        let result = Scanner::discover(dir.path(), None, true);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 2);
    }

    #[test]
    fn test_scan_dir_finds_workflow_yml() {
        let dir = TempDir::new().unwrap();
        let workflows = dir.path().join(".github").join("workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        create_temp_manifest(
            &workflows,
            "CI.yml",
            "jobs:\n  test:\n    runs-on: ubuntu-latest\n",
        );

        let manifests = Scanner::scan_dir(dir.path());
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].kind, ManifestKind::GitHubWorkflow);
        assert!(manifests[0].path.ends_with("CI.yml"));
    }

    #[test]
    fn test_scan_dir_finds_workflow_yaml() {
        let dir = TempDir::new().unwrap();
        let workflows = dir.path().join(".github").join("workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        create_temp_manifest(&workflows, "release.yaml", "jobs: {}\n");

        let manifests = Scanner::scan_dir(dir.path());
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].kind, ManifestKind::GitHubWorkflow);
    }

    #[test]
    fn test_scan_dir_finds_root_action_yml() {
        // Composite action authors put `action.yml` at the repo root. scan_dir
        // (no -d) must surface it so they don't have to remember `-d`.
        let dir = TempDir::new().unwrap();
        create_temp_manifest(
            dir.path(),
            "action.yml",
            "name: test\nruns:\n  using: composite\n",
        );

        let manifests = Scanner::scan_dir(dir.path());
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].kind, ManifestKind::GitHubWorkflow);
    }

    #[test]
    fn test_scan_dir_workflow_files_sorted_alphabetically() {
        // read_dir order is OS-dependent (NTFS != ext4). Sort guarantees
        // reproducible CLI output.
        let dir = TempDir::new().unwrap();
        let workflows = dir.path().join(".github").join("workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        create_temp_manifest(&workflows, "z.yml", "jobs:");
        create_temp_manifest(&workflows, "a.yml", "jobs:");
        create_temp_manifest(&workflows, "m.yml", "jobs:");

        let manifests = Scanner::scan_dir(dir.path());
        assert_eq!(manifests.len(), 3);
        assert!(manifests[0].path.ends_with("a.yml"));
        assert!(manifests[1].path.ends_with("m.yml"));
        assert!(manifests[2].path.ends_with("z.yml"));
    }

    #[test]
    fn test_scan_dir_ignores_non_yml_files_in_workflows_dir() {
        // README.md / json artefacts inside .github/workflows/ must not pollute
        // the result.
        let dir = TempDir::new().unwrap();
        let workflows = dir.path().join(".github").join("workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        create_temp_manifest(&workflows, "README.md", "# ignored");
        create_temp_manifest(&workflows, "config.json", "{}");
        create_temp_manifest(&workflows, "CI.yml", "jobs:");

        let manifests = Scanner::scan_dir(dir.path());
        assert_eq!(manifests.len(), 1);
        assert!(manifests[0].path.ends_with("CI.yml"));
    }

    #[test]
    fn test_scan_dir_combines_workflows_and_traditional_manifests() {
        let dir = TempDir::new().unwrap();
        create_temp_manifest(dir.path(), "Cargo.toml", "[package]");
        create_temp_manifest(dir.path(), "package.json", "{}");
        let workflows = dir.path().join(".github").join("workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        create_temp_manifest(&workflows, "CI.yml", "jobs:");

        let manifests = Scanner::scan_dir(dir.path());
        assert_eq!(manifests.len(), 3);
        let kinds: std::collections::HashSet<_> = manifests.iter().map(|m| m.kind).collect();
        assert!(kinds.contains(&ManifestKind::CargoToml));
        assert!(kinds.contains(&ManifestKind::PackageJson));
        assert!(kinds.contains(&ManifestKind::GitHubWorkflow));
    }

    #[test]
    fn test_scan_dir_skips_subdirectories_in_workflows_dir() {
        // Some repos nest templates / shared workflow steps in subdirs of
        // `.github/workflows/`. read_dir yields these subdirs, and scan_dir
        // must skip them (only files are manifest candidates).
        let dir = TempDir::new().unwrap();
        let workflows = dir.path().join(".github").join("workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        std::fs::create_dir_all(workflows.join("templates")).unwrap();
        create_temp_manifest(&workflows, "CI.yml", "jobs:");

        let manifests = Scanner::scan_dir(dir.path());
        assert_eq!(manifests.len(), 1);
        assert!(manifests[0].path.ends_with("CI.yml"));
    }

    #[test]
    fn test_scan_deep_walks_into_dot_github() {
        // Deep scan must traverse into `.github` (hidden by convention) so it
        // finds workflow manifests. Other hidden dirs (`.git`, `.venv`) must
        // still be skipped.
        let dir = TempDir::new().unwrap();
        let workflows = dir.path().join(".github").join("workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        create_temp_manifest(&workflows, "CI.yml", "jobs:");

        // A hidden non-.github dir that must be ignored.
        std::fs::create_dir_all(dir.path().join(".secret")).unwrap();
        create_temp_manifest(&dir.path().join(".secret"), "package.json", "{}");

        let manifests = Scanner::scan_deep(dir.path());
        let workflow_count = manifests
            .iter()
            .filter(|m| m.kind == ManifestKind::GitHubWorkflow)
            .count();
        assert_eq!(workflow_count, 1, "must find workflow inside .github/");
        let secret_count = manifests
            .iter()
            .filter(|m| m.path.to_string_lossy().contains(".secret"))
            .count();
        assert_eq!(secret_count, 0, "other hidden dirs must stay hidden");
    }
}
