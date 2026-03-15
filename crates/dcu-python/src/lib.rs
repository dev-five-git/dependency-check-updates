//! Python ecosystem support for dcu.
//!
//! Handles `pyproject.toml` parsing via `toml_edit` (format-preserving),
//! `PyPI` registry lookups, and PEP 440 version resolution.

pub mod parser;
pub mod registry;

use std::path::Path;

use dcu_core::manifest::{ManifestHandler, ParsedManifest};
use dcu_core::{DcuError, ManifestKind, ManifestRef, PlannedUpdate};

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
