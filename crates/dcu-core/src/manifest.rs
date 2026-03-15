//! Shared manifest handling abstractions.
//!
//! Each language crate (dcu-node, dcu-rust, dcu-python) implements these
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
/// - `dcu-node`   → `package.json`
/// - `dcu-rust`   → `Cargo.toml`
/// - `dcu-python` → `pyproject.toml`
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
    /// Returns all recognized manifests: `package.json`, `Cargo.toml`, `pyproject.toml`.
    #[must_use]
    pub fn scan_dir(root: &Path) -> Vec<ManifestRef> {
        let mut manifests = Vec::new();

        let candidates = ["package.json", "Cargo.toml", "pyproject.toml"];

        for filename in &candidates {
            let path = root.join(filename);
            if path.is_file() {
                if let Some(kind) = ManifestKind::from_path(&path) {
                    manifests.push(ManifestRef { path, kind });
                }
            }
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
    #[must_use]
    pub fn scan_deep(root: &Path) -> Vec<ManifestRef> {
        use ignore::WalkBuilder;

        let manifest_names: &[&str] = &["package.json", "Cargo.toml", "pyproject.toml"];

        let walker = WalkBuilder::new(root)
            .hidden(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .filter_entry(|entry| {
                let name = entry.file_name().to_string_lossy();
                // Skip common dependency/build directories
                !matches!(
                    name.as_ref(),
                    "node_modules"
                        | "target"
                        | ".venv"
                        | "dist"
                        | "build"
                        | "vendor"
                        | "__pycache__"
                )
            })
            .build();

        let mut manifests = Vec::new();

        for entry in walker.flatten() {
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }

            let file_name = entry.file_name().to_string_lossy();
            if manifest_names.contains(&file_name.as_ref()) {
                let path = entry.into_path();
                if let Some(kind) = ManifestKind::from_path(&path) {
                    debug!(path = %path.display(), kind = %kind, "deep scan: found manifest");
                    manifests.push(ManifestRef { path, kind });
                }
            }
        }

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
}
