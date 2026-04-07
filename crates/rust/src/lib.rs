//! Rust ecosystem support for dependency-check-updates.
//!
//! Handles `Cargo.toml` parsing via `toml_edit` (format-preserving),
//! `crates.io` registry lookups, and Rust semver resolution.

pub mod parser;
pub mod registry;

use std::path::Path;

use dependency_check_updates_core::manifest::{ManifestHandler, ParsedManifest};
use dependency_check_updates_core::{DcuError, ManifestKind, ManifestRef, PlannedUpdate};

pub use parser::{CargoTomlError, CargoTomlManifest};
pub use registry::CratesIoRegistry;

/// Rust manifest handler for `Cargo.toml` files.
pub struct RustHandler;

impl ManifestHandler for RustHandler {
    fn parse(&self, text: &str, path: &Path) -> Result<ParsedManifest, DcuError> {
        let manifest = CargoTomlManifest::parse(text).map_err(|e| DcuError::ManifestParse {
            path: path.to_path_buf(),
            detail: e.to_string(),
        })?;

        Ok(ParsedManifest {
            manifest_ref: ManifestRef {
                path: path.to_path_buf(),
                kind: ManifestKind::CargoToml,
            },
            original_text: manifest.original_text,
            dependencies: manifest.dependencies,
        })
    }

    fn apply_updates(&self, text: &str, updates: &[PlannedUpdate]) -> Result<String, DcuError> {
        let mut manifest = CargoTomlManifest::parse(text).map_err(|e| DcuError::PatchFailed {
            path: std::path::PathBuf::from("Cargo.toml"),
            detail: e.to_string(),
        })?;

        manifest
            .apply_updates(updates)
            .map_err(|e| DcuError::PatchFailed {
                path: std::path::PathBuf::from("Cargo.toml"),
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
    fn test_rust_handler_parse() {
        let handler = RustHandler;
        let text = "[dependencies]\nserde = \"1.0\"\n";
        let result = handler.parse(text, Path::new("Cargo.toml")).unwrap();
        assert_eq!(result.dependencies.len(), 1);
        assert_eq!(result.manifest_ref.kind, ManifestKind::CargoToml);
    }

    #[test]
    fn test_rust_handler_parse_invalid() {
        let handler = RustHandler;
        let result = handler.parse("invalid [[[toml", Path::new("Cargo.toml"));
        assert!(result.is_err());
    }

    #[test]
    fn test_rust_handler_apply_updates() {
        let handler = RustHandler;
        let text = "[dependencies]\nserde = \"1.0\"\n";
        let updates = vec![PlannedUpdate {
            name: "serde".to_owned(),
            section: DependencySection::Dependencies,
            from: "1.0".to_owned(),
            to: "2.0".to_owned(),
        }];
        let result = handler.apply_updates(text, &updates).unwrap();
        assert!(result.contains("\"2.0\""));
    }
}
