//! Python ecosystem support for dependency-check-updates.
//!
//! Handles `pyproject.toml` parsing via `toml_edit` (format-preserving),
//! `PyPI` registry lookups, and PEP 440 version resolution.

pub mod parser;
pub mod registry;

use std::path::Path;

use dependency_check_updates_core::manifest::{ManifestHandler, ParsedManifest};
use dependency_check_updates_core::{DcuError, ManifestKind, ManifestRef, PlannedUpdate};

pub use parser::{PyProjectError, PyProjectManifest};
pub use registry::PyPiRegistry;

/// Python manifest handler for `pyproject.toml` files.
pub struct PythonHandler;

impl ManifestHandler for PythonHandler {
    fn parse(&self, text: &str, path: &Path) -> Result<ParsedManifest, DcuError> {
        let manifest = PyProjectManifest::parse(text).map_err(|e| DcuError::ManifestParse {
            path: path.to_path_buf(),
            detail: e.to_string(),
        })?;

        Ok(ParsedManifest {
            manifest_ref: ManifestRef {
                path: path.to_path_buf(),
                kind: ManifestKind::PyProjectToml,
            },
            original_text: manifest.original_text,
            dependencies: manifest.dependencies,
        })
    }

    fn apply_updates(&self, text: &str, updates: &[PlannedUpdate]) -> Result<String, DcuError> {
        let mut manifest = PyProjectManifest::parse(text).map_err(|e| DcuError::PatchFailed {
            path: std::path::PathBuf::from("pyproject.toml"),
            detail: e.to_string(),
        })?;

        manifest
            .apply_updates(updates)
            .map_err(|e| DcuError::PatchFailed {
                path: std::path::PathBuf::from("pyproject.toml"),
                detail: e.to_string(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dependency_check_updates_core::DependencySection;
    use dependency_check_updates_core::manifest::ManifestHandler;
    use std::path::Path;

    #[test]
    fn test_python_handler_parse() {
        let handler = PythonHandler;
        let text = "[project]\nname = \"test\"\ndependencies = [\"requests>=2.28.0\"]\n";
        let result = handler.parse(text, Path::new("pyproject.toml")).unwrap();
        assert_eq!(result.dependencies.len(), 1);
        assert_eq!(result.manifest_ref.kind, ManifestKind::PyProjectToml);
    }

    #[test]
    fn test_python_handler_parse_invalid() {
        let handler = PythonHandler;
        let result = handler.parse("invalid [[[toml", Path::new("pyproject.toml"));
        assert!(result.is_err());
    }

    #[test]
    fn test_python_handler_apply_updates() {
        let handler = PythonHandler;
        let text = "[project]\nname = \"test\"\ndependencies = [\"requests>=2.28.0\"]\n";
        let updates = vec![PlannedUpdate {
            name: "requests".to_owned(),
            section: DependencySection::ProjectDependencies,
            from: ">=2.28.0".to_owned(),
            to: ">=2.31.0".to_owned(),
        }];
        let result = handler.apply_updates(text, &updates).unwrap();
        assert!(result.contains("requests>=2.31.0"));
    }
}
