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
pub mod types;
pub mod util;
pub mod version;

// Re-export commonly used types
pub use error::DcuError;
pub use http::{
    DEFAULT_MAX_CONCURRENT_REQUESTS, DEFAULT_REQUEST_TIMEOUT_SECS, build_client,
    resolve_batch_concurrent,
};
pub use manifest::{ManifestHandler, ParsedManifest, Scanner};
pub use patch::{Patch, PatchError as BytePatchError, apply_byte_patches};
pub use types::{
    BumpType, DependencySection, DependencySpec, ManifestKind, ManifestRef, PlannedUpdate,
    ResolvedVersion, TargetLevel,
};
pub use util::{pad_to_three_segments, split_numeric_head, strip_range_prefix};
pub use version::{SelectableVersion, parse_and_select, select_version};
