//! Node.js ecosystem support for dependency-check-updates.
//!
//! Handles `package.json` parsing, npm registry lookups, and version resolution.
//! Follows the changepacks pattern of one crate per language ecosystem.

pub mod parser;
pub mod patcher;
pub mod registry;
pub mod style;

use std::path::Path;

use dependency_check_updates_core::manifest::{ManifestHandler, ParsedManifest};
use dependency_check_updates_core::{
    DcuError, DependencySpec, ManifestKind, ManifestRef, PlannedUpdate,
};

pub use parser::{PackageJsonError, PackageJsonManifest};
pub use patcher::{JsonPatcher, Patch, PatchError, VersionLocation};
pub use registry::NpmRegistry;
pub use style::StyleDetector;

/// Node.js manifest handler for `package.json` files.
pub struct NodeHandler;

impl ManifestHandler for NodeHandler {
    fn parse(&self, text: &str, path: &Path) -> Result<ParsedManifest, DcuError> {
        let manifest = PackageJsonManifest::parse(text).map_err(|e| DcuError::ManifestParse {
            path: path.to_path_buf(),
            detail: e.to_string(),
        })?;

        Ok(ParsedManifest {
            manifest_ref: ManifestRef {
                path: path.to_path_buf(),
                kind: ManifestKind::PackageJson,
            },
            original_text: manifest.original_text,
            dependencies: manifest.dependencies,
        })
    }

    fn apply_updates(&self, text: &str, updates: &[PlannedUpdate]) -> Result<String, DcuError> {
        // Use scan_for_updates: skips full JSON parse, only locates deps we need.
        let locations =
            JsonPatcher::scan_for_updates(text, updates).map_err(|e| DcuError::PatchFailed {
                path: std::path::PathBuf::from("package.json"),
                detail: e.to_string(),
            })?;

        let patches: Vec<Patch> = updates
            .iter()
            .filter_map(|update| {
                locations
                    .iter()
                    .find(|loc| loc.name == update.name && loc.section == update.section)
                    .map(|loc| Patch {
                        start: loc.value_start,
                        end: loc.value_end,
                        new_value: update.to.clone(),
                    })
            })
            .collect();

        JsonPatcher::apply_patches(text, &patches).map_err(|e| DcuError::PatchFailed {
            path: std::path::PathBuf::from("package.json"),
            detail: e.to_string(),
        })
    }
}

/// Create a [`DependencySpec`] filter that skips non-version specs.
///
/// The parser already filters, but this is used when converting
/// between internal representations.
#[must_use]
pub fn is_node_ecosystem(dep: &DependencySpec) -> bool {
    matches!(
        dep.section,
        dependency_check_updates_core::DependencySection::Dependencies
            | dependency_check_updates_core::DependencySection::DevDependencies
            | dependency_check_updates_core::DependencySection::PeerDependencies
            | dependency_check_updates_core::DependencySection::OptionalDependencies
    )
}
