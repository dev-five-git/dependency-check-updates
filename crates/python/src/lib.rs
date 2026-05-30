//! Python ecosystem support for dependency-check-updates.
//!
//! Handles `pyproject.toml` parsing via `toml_edit` (format-preserving),
//! `PyPI` registry lookups, and PEP 440 version resolution.

#![warn(missing_docs)]

mod parser;
mod registry;

use std::path::Path;

use dependency_check_updates_core::manifest::{ManifestHandler, ParsedManifest};
use dependency_check_updates_core::{DcuError, ManifestKind, ManifestRef, PlannedUpdate};

use parser::PyProjectManifest;
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

        Ok(manifest.apply_updates(updates))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dependency_check_updates_core::DependencySection;
    use dependency_check_updates_core::manifest::ManifestHandler;
    use rstest::rstest;
    use std::path::Path;

    /// Parse outcomes: a valid PEP 621 doc parses to one dep, invalid TOML
    /// fails. Same call shape → parametrize.
    #[rstest]
    #[case::valid_pep621(
        "[project]\nname = \"test\"\ndependencies = [\"requests>=2.28.0\"]\n",
        true
    )]
    #[case::invalid_toml("invalid [[[toml", false)]
    fn python_handler_parse_cases(#[case] text: &str, #[case] should_succeed: bool) {
        let handler = PythonHandler;
        let result = handler.parse(text, Path::new("pyproject.toml"));
        assert_eq!(result.is_ok(), should_succeed);
        if let Ok(parsed) = result {
            assert_eq!(parsed.dependencies.len(), 1);
            assert_eq!(parsed.manifest_ref.kind, ManifestKind::PyProjectToml);
        }
    }

    /// Covers the `PyProjectManifest::parse(...).map_err(...)` error path in
    /// `ManifestHandler::apply_updates` (lib.rs lines 40-43). Feeding invalid
    /// TOML forces the parser to fail, which the handler must wrap as
    /// `DcuError::PatchFailed`.
    #[test]
    fn python_handler_apply_updates_invalid_toml_returns_error() {
        let handler = PythonHandler;
        let result = handler.apply_updates("not valid [[[toml", &[]);
        assert!(result.is_err(), "invalid TOML must surface as PatchFailed");
        match result {
            Err(DcuError::PatchFailed { .. }) => {}
            other => panic!("expected DcuError::PatchFailed, got {other:?}"),
        }
    }

    #[test]
    fn python_handler_apply_updates_rewrites_version() {
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
