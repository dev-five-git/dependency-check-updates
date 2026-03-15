//! Core domain types and orchestration for dcu.
//!
//! Defines shared traits that each language crate implements:
//! - [`ManifestHandler`] — parse manifests and apply format-preserving updates
//! - [`RegistryClient`] — resolve versions from package registries
//! - [`Scanner`] — discover manifest files in a directory

pub mod error;
pub mod manifest;
pub mod style;
pub mod types;

// Re-export commonly used types
pub use error::DcuError;
pub use manifest::{ManifestHandler, ParsedManifest, RegistryClient, ScanResult, Scanner};
pub use style::{FileStyle, IndentStyle, LineEnding};
pub use types::{
    BumpType, DependencySection, DependencySpec, ManifestKind, ManifestRef, PlannedUpdate,
    ResolvedVersion, TargetLevel,
};
