//! Rust ecosystem support for dependency-check-updates.
//!
//! Handles `Cargo.toml` parsing via `toml_edit` (format-preserving),
//! `crates.io` registry lookups, and Rust semver resolution.

#![warn(missing_docs)]

mod parser;
mod registry;

use std::path::Path;

use dependency_check_updates_core::manifest::{ManifestHandler, ParsedManifest};
use dependency_check_updates_core::{DcuError, ManifestKind, ManifestRef, PlannedUpdate};

use parser::CargoTomlManifest;
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
    use rstest::rstest;
    use std::path::Path;

    #[rstest]
    // text, expected dependency count (None = parse must fail).
    #[case::valid("[dependencies]\nserde = \"1.0\"\n", Some(1))]
    #[case::invalid("invalid [[[toml", None)]
    fn rust_handler_parse_cases(#[case] text: &str, #[case] expected_dep_count: Option<usize>) {
        let handler = RustHandler;
        let result = handler.parse(text, Path::new("Cargo.toml"));
        match expected_dep_count {
            Some(n) => {
                let parsed = result.unwrap();
                assert_eq!(parsed.dependencies.len(), n);
                assert_eq!(parsed.manifest_ref.kind, ManifestKind::CargoToml);
            }
            None => assert!(result.is_err()),
        }
    }

    #[test]
    fn rust_handler_apply_updates() {
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

    /// Covers the inner `CargoTomlManifest::parse(text).map_err(...)` arm of
    /// `apply_updates` (lib.rs lines 41-42). Invalid TOML routes through the
    /// `PatchFailed` mapper instead of `ManifestParse` because this is the
    /// patch path, not the initial parse path.
    #[test]
    fn rust_handler_apply_updates_invalid_toml_is_patch_failed() {
        let handler = RustHandler;
        let updates: Vec<PlannedUpdate> = Vec::new();
        let result = handler.apply_updates("not valid toml [[[", &updates);
        assert!(matches!(result, Err(DcuError::PatchFailed { .. })));
    }

    /// Covers the `manifest.apply_updates(updates).map_err(...)` arm of
    /// `apply_updates` (lib.rs lines 48-49). A `PlannedUpdate` whose `name`
    /// does not exist in the parsed document makes
    /// `CargoTomlManifest::apply_updates` return
    /// `CargoTomlError::DependencyNotFound`, which the handler maps to
    /// `DcuError::PatchFailed`.
    #[test]
    fn rust_handler_apply_updates_missing_dep_is_patch_failed() {
        let handler = RustHandler;
        let text = "[dependencies]\nserde = \"1.0\"\n";
        let updates = vec![PlannedUpdate {
            name: "nonexistent".to_owned(),
            section: DependencySection::Dependencies,
            from: "1.0".to_owned(),
            to: "2.0".to_owned(),
        }];
        let result = handler.apply_updates(text, &updates);
        assert!(matches!(result, Err(DcuError::PatchFailed { .. })));
    }
}
