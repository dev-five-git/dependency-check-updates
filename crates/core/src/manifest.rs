//! Shared manifest handling abstractions.
//!
//! Each language crate (dependency-check-updates-node, dependency-check-updates-rust, dependency-check-updates-python) implements these
//! traits for its specific manifest format and registry.

use std::path::Path;

use tracing::debug;

use crate::error::DcuError;
use crate::types::{DependencySpec, ManifestKind, ManifestRef, PlannedUpdate};

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
    /// Collected dependencies.
    pub dependencies: Vec<DependencySpec>,
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
    /// `Cargo.toml`, `pyproject.toml`, `action.yml` / `action.yaml`,
    /// `Dockerfile`, and the Compose project files) and every `*.yml`/`*.yaml`
    /// directly under `.github/workflows/`. The root-level `action.yml` is
    /// included so that authors of single-action repos see their own manifest
    /// without needing `-d`.
    ///
    /// Only the canonical Docker file names are probed here. The suffixed
    /// forms (`Dockerfile.dev`, `compose.prod.yaml`) are still recognised by
    /// [`ManifestKind::from_path`], so `-d` and `--manifest` pick them up —
    /// enumerating every possible variant at the root would mean a full
    /// `read_dir` on every invocation just to catch a rare layout.
    #[must_use]
    pub fn scan_dir(root: &Path) -> Vec<ManifestRef> {
        let mut manifests = Vec::new();

        let candidates = [
            "package.json",
            "Cargo.toml",
            "pyproject.toml",
            "action.yml",
            "action.yaml",
            "Dockerfile",
            "compose.yml",
            "compose.yaml",
            "docker-compose.yml",
            "docker-compose.yaml",
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
        }

        // Stable order so output is reproducible across platforms regardless
        // of whether `.github/workflows/` exists. The static `candidates`
        // array order (package.json → Cargo.toml → pyproject.toml → action.{yml,yaml})
        // is NOT alphabetical, and `read_dir` ordering is OS-dependent (NTFS
        // vs ext4 give different orderings), so we sort unconditionally here
        // to match `scan_deep`'s already-unconditional sort below.
        // Paths are unique (each manifest file appears at most once), so stable
        // ordering is unobservable; use sort_unstable_by for better performance.
        manifests.sort_unstable_by(|a, b| a.path.cmp(&b.path));
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
    /// (`node_modules`, `target`, Python local env/package dirs, `dist`,
    /// `build`, `vendor`).
    /// Walks INTO `.github` even though it is a hidden directory because
    /// workflow YAMLs live there; without this exception deep scan would miss
    /// every GitHub Actions manifest.
    #[must_use]
    pub fn scan_deep(root: &Path) -> Vec<ManifestRef> {
        use ignore::WalkBuilder;

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
                    "node_modules"
                        | "target"
                        | "__pypackages__"
                        | "dist"
                        | "build"
                        | "vendor"
                        | "__pycache__"
                )
            })
            .build();

        let mut manifests = Vec::new();

        // Single source of truth for what counts as a manifest:
        // `ManifestKind::from_path` already encodes the full decision tree
        // (the 5 named files + `.github/workflows/*.{yml,yaml}`). Delegating
        // here removes the previously-duplicated `manifest_names` list and
        // `is_workflow_yaml` parent-traversal block, and drops the per-file
        // `to_string_lossy()` allocation in the deep-walk hot path.
        for entry in walker.flatten() {
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }
            let Some(kind) = ManifestKind::from_path(entry.path()) else {
                continue;
            };
            let path = entry.into_path();
            debug!(path = %path.display(), kind = %kind, "deep scan: found manifest");
            manifests.push(ManifestRef { path, kind });
        }

        // Paths are unique (each manifest file appears at most once), so stable
        // ordering is unobservable; use sort_unstable_by for better performance.
        manifests.sort_unstable_by(|a, b| a.path.cmp(&b.path));
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

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::{fixture, rstest};
    use std::fs;
    use tempfile::TempDir;

    fn create_temp_manifest(dir: &Path, filename: &str, content: &str) {
        fs::write(dir.join(filename), content).unwrap();
    }

    /// Fresh isolated working dir per generated test. Returning the guard
    /// (not just the path) is required so the directory survives until the
    /// case finishes.
    #[fixture]
    fn tmp() -> TempDir {
        TempDir::new().expect("create temp dir")
    }

    /// `scan_dir` recognises every supported manifest layout when each is the
    /// sole file in the tree. Cases cover the three flat manifests at the
    /// root, both workflow YAML extensions under `.github/workflows/`, and
    /// the `action.yml` composite-action layout that lives at the repo root.
    #[rstest]
    #[case::package_json("package.json", r#"{"name":"test"}"#, ManifestKind::PackageJson)]
    #[case::cargo_toml("Cargo.toml", "[package]\nname = \"test\"", ManifestKind::CargoToml)]
    #[case::pyproject_toml(
        "pyproject.toml",
        "[project]\nname = \"test\"",
        ManifestKind::PyProjectToml
    )]
    #[case::workflow_yml(
        ".github/workflows/CI.yml",
        "jobs:\n  test:\n    runs-on: ubuntu-latest\n",
        ManifestKind::GitHubWorkflow
    )]
    #[case::workflow_yaml(
        ".github/workflows/release.yaml",
        "jobs: {}\n",
        ManifestKind::GitHubWorkflow
    )]
    // Composite action authors put `action.yml` at the repo root. scan_dir
    // (no -d) must surface it so they don't have to remember `-d`.
    #[case::root_action_yml(
        "action.yml",
        "name: test\nruns:\n  using: composite\n",
        ManifestKind::GitHubWorkflow
    )]
    // Docker manifests must surface without `-d` for the common single-service
    // repo layout (Dockerfile + compose file at the root).
    #[case::root_dockerfile("Dockerfile", "FROM node:20-alpine\n", ManifestKind::Dockerfile)]
    #[case::root_compose_yaml(
        "compose.yaml",
        "services:\n  web:\n    image: nginx:1.27\n",
        ManifestKind::DockerCompose
    )]
    #[case::root_docker_compose_yml(
        "docker-compose.yml",
        "services:\n  web:\n    image: nginx:1.27\n",
        ManifestKind::DockerCompose
    )]
    fn scan_dir_finds_single_manifest(
        tmp: TempDir,
        #[case] rel_path: &str,
        #[case] content: &str,
        #[case] kind: ManifestKind,
    ) {
        let full = tmp.path().join(rel_path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&full, content).unwrap();

        let manifests = Scanner::scan_dir(tmp.path());
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].kind, kind);
        // Preserves the `path.ends_with("CI.yml")` guarantee from the original
        // workflow_yml test; every case satisfies it since the scanner returns
        // the file we created.
        let filename = std::path::Path::new(rel_path)
            .file_name()
            .expect("rel_path has a file name");
        assert!(manifests[0].path.ends_with(filename));
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

    /// `Scanner::from_path` for files that exist on disk: recognised manifest
    /// → `Ok(kind)`, anything else → `Err`. The "file does not exist at all"
    /// scenario stays a plain `#[test]` below because its setup (absolute,
    /// non-existent path) does not share the fixture.
    #[rstest]
    #[case::valid_package_json("package.json", "{}", Some(ManifestKind::PackageJson))]
    #[case::unknown_file("build.gradle", "", None)]
    fn from_path_existing_file(
        tmp: TempDir,
        #[case] filename: &str,
        #[case] content: &str,
        #[case] expected: Option<ManifestKind>,
    ) {
        create_temp_manifest(tmp.path(), filename, content);
        let result = Scanner::from_path(&tmp.path().join(filename));
        match expected {
            Some(k) => assert_eq!(result.expect("from_path should succeed").kind, k),
            None => assert!(result.is_err(), "expected Err for {filename}"),
        }
    }

    #[test]
    fn test_from_path_not_found() {
        let result = Scanner::from_path(Path::new("/nonexistent/package.json"));
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

    /// Regression: `scan_dir`'s sort used to fire only when
    /// `.github/workflows/` existed, leaving non-workflow projects on the
    /// static `candidates` array order
    /// (`package.json` → `Cargo.toml` → `pyproject.toml` → `action.{yml,yaml}`)
    /// — which is NOT alphabetical. `scan_deep` always sorted, so the same
    /// layout produced different ordering between `dcu` and `dcu -d`. The
    /// CI-consumable `--format json` inherited that inconsistency. This test
    /// fails on the pre-fix code (Cargo.toml appears at index 1, package.json
    /// at index 0) and passes after the sort moves out of the `if let`.
    #[test]
    fn test_scan_dir_sorts_when_no_workflows_dir() {
        let dir = TempDir::new().unwrap();
        create_temp_manifest(dir.path(), "package.json", "{}");
        create_temp_manifest(dir.path(), "Cargo.toml", "[package]");
        // No `.github/workflows/` directory — the sort must still fire.
        assert!(!dir.path().join(".github").join("workflows").exists());

        let manifests = Scanner::scan_dir(dir.path());
        assert_eq!(manifests.len(), 2);
        // Alphabetical: 'C' (0x43) < 'p' (0x70), so Cargo.toml sorts first.
        assert!(
            manifests[0].path.ends_with("Cargo.toml"),
            "Cargo.toml should sort first: {:?}",
            manifests.iter().map(|m| &m.path).collect::<Vec<_>>()
        );
        assert!(
            manifests[1].path.ends_with("package.json"),
            "package.json should sort second: {:?}",
            manifests.iter().map(|m| &m.path).collect::<Vec<_>>()
        );
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

    /// Deep scan must prune installed/generated directories yet still descend
    /// into normal nested directories. Exercises the `!matches!` filter
    /// closure on both branches: `node_modules` → false (pruned),
    /// `pkgs`/`app` → true (kept).
    #[test]
    fn test_scan_deep_prunes_excluded_dirs_but_keeps_nested() {
        let dir = TempDir::new().unwrap();

        // Excluded: dependency/env directories with manifests inside that must
        // NOT surface.
        for rel in ["node_modules/foo", "__pypackages__/3.13/lib/pkg"] {
            let excluded = dir.path().join(rel);
            std::fs::create_dir_all(&excluded).unwrap();
            create_temp_manifest(&excluded, "package.json", "{}");
        }

        // Kept: normal nested workspace member.
        let app = dir.path().join("pkgs").join("app");
        std::fs::create_dir_all(&app).unwrap();
        create_temp_manifest(&app, "Cargo.toml", "[package]\nname = \"app\"");
        // Ordinary files sit beside manifests everywhere; the walker must skip
        // the ones `ManifestKind::from_path` does not recognise instead of
        // trying to parse them.
        create_temp_manifest(&app, "README.md", "# app");
        create_temp_manifest(&app, "build.gradle", "");

        let manifests = Scanner::scan_deep(dir.path());

        // The nested Cargo.toml must be found.
        assert!(
            manifests
                .iter()
                .any(|m| m.path.ends_with("pkgs/app/Cargo.toml")
                    || m.path.ends_with("pkgs\\app\\Cargo.toml")),
            "expected pkgs/app/Cargo.toml in results: {:?}",
            manifests.iter().map(|m| &m.path).collect::<Vec<_>>()
        );
        // The excluded dependency/env manifests must NOT be found.
        assert!(
            !manifests.iter().any(|m| matches!(
                m.path.to_string_lossy().as_ref(),
                p if p.contains("node_modules") || p.contains("__pypackages__")
            )),
            "dependency/env dirs must be pruned: {:?}",
            manifests.iter().map(|m| &m.path).collect::<Vec<_>>()
        );
    }

    /// `discover(root, Some(absolute), false)` must take the absolute branch
    /// (`path.to_path_buf()`) and resolve the manifest as-is rather than
    /// joining it with `root`. Covers the `path.is_absolute()` true arm.
    #[test]
    fn test_discover_with_absolute_manifest_path() {
        let dir = TempDir::new().unwrap();
        create_temp_manifest(dir.path(), "Cargo.toml", "[package]\nname = \"x\"");
        let abs = dir.path().join("Cargo.toml");
        assert!(abs.is_absolute(), "tempfile path must be absolute");

        // Pass a DIFFERENT root to prove the absolute path is used verbatim,
        // not joined with `root`.
        let other_root = TempDir::new().unwrap();
        let result = Scanner::discover(other_root.path(), Some(&abs), false)
            .expect("absolute manifest path must resolve");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].path, abs);
        assert_eq!(result[0].kind, ManifestKind::CargoToml);
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
