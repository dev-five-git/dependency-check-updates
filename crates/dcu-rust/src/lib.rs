//! Rust ecosystem support for dcu.
//!
//! Handles `Cargo.toml` parsing via `toml_edit` (format-preserving),
//! `crates.io` registry lookups, and Rust semver resolution.

pub mod parser;
pub mod registry;

use std::path::Path;

use dcu_core::manifest::{ManifestHandler, ParsedManifest};
use dcu_core::{DcuError, ManifestKind, ManifestRef, PlannedUpdate};

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
