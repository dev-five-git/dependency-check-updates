//! Core domain types and orchestration for dependency-check-updates.
//!
//! Defines shared traits that each language crate implements:
//! - [`ManifestHandler`] — parse manifests and apply format-preserving updates
//! - [`Scanner`] — discover manifest files in a directory

#![warn(missing_docs)]

pub mod error;
pub mod http;
pub mod manifest;
pub mod patch;
pub mod toml_decor;
pub mod types;
pub mod util;
pub mod version;

// Re-export commonly used types
pub use error::DcuError;
pub use http::{DEFAULT_MAX_CONCURRENT_REQUESTS, build_client, resolve_batch_concurrent};
pub use manifest::{ManifestHandler, ParsedManifest, Scanner};
pub use patch::{Patch, apply_byte_patches};
pub use toml_decor::replace_string_preserving_decor;
pub use types::{
    BumpType, DependencySection, DependencySpec, ManifestKind, ManifestRef, PlannedUpdate,
    ResolvedVersion, TargetLevel,
};
pub use util::{
    count_numeric_segments, pad_to_three_segments, split_numeric_head, strip_range_prefix,
};
pub use version::{SelectableVersion, highest_stable, parse_and_select, select_version};
