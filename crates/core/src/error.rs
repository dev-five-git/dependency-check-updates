//! The crate-wide error type [`DcuError`] returned by manifest, registry, and
//! patch operations.

use miette::Diagnostic;
use std::path::PathBuf;
use thiserror::Error;

/// Main error type for dependency-check-updates operations.
///
/// Marked `#[non_exhaustive]` so new error variants can be added in minor
/// releases without breaking downstream `match` arms. Construction of the
/// existing variants is unaffected; only exhaustive matching outside this
/// crate requires a wildcard arm.
#[derive(Debug, Error, Diagnostic)]
#[non_exhaustive]
pub enum DcuError {
    /// Failed to read a manifest file from disk.
    #[error("failed to read manifest at {path}")]
    #[diagnostic(
        code(dependency_check_updates::io_error),
        help("make sure the file exists and is readable")
    )]
    Io {
        /// Path of the file that could not be read.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Failed to parse a manifest's contents.
    #[error("failed to parse manifest at {path}")]
    #[diagnostic(code(dependency_check_updates::parse_error))]
    ManifestParse {
        /// Path of the manifest that failed to parse.
        path: PathBuf,
        /// Human-readable parser error detail.
        detail: String,
    },

    /// A package-registry lookup failed.
    #[error("registry lookup failed for package `{package}`: {detail}")]
    #[diagnostic(
        code(dependency_check_updates::registry_error),
        help("check your internet connection, or set GITHUB_TOKEN if scanning workflows")
    )]
    RegistryLookup {
        /// Name of the package being resolved.
        package: String,
        /// Human-readable failure detail.
        detail: String,
    },

    /// Failed to apply a version patch to a manifest.
    #[error("failed to apply patch to {path}")]
    #[diagnostic(code(dependency_check_updates::patch_error))]
    PatchFailed {
        /// Path of the manifest being patched.
        path: PathBuf,
        /// Human-readable failure detail.
        detail: String,
    },

    /// A version string could not be parsed as semver.
    #[error("invalid semver: {input}")]
    #[diagnostic(code(dependency_check_updates::semver_error))]
    SemverParse {
        /// The input that failed to parse.
        input: String,
        /// Human-readable failure detail.
        detail: String,
    },

    /// No recognized manifest was found at the given location.
    #[error("no manifest found in {path}")]
    #[diagnostic(
        code(dependency_check_updates::no_manifest),
        help(
            "run dependency-check-updates in a directory containing package.json, or use --manifest"
        )
    )]
    NoManifest {
        /// Directory or path that was searched.
        path: PathBuf,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dcu_error_io() {
        let path = PathBuf::from("test.json");
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err = DcuError::Io {
            path: path.clone(),
            source: io_err,
        };
        assert_eq!(
            err.to_string(),
            format!("failed to read manifest at {}", path.display())
        );
    }

    #[test]
    fn test_dcu_error_manifest_parse() {
        let path = PathBuf::from("package.json");
        let err = DcuError::ManifestParse {
            path: path.clone(),
            detail: "unexpected token".to_string(),
        };
        assert_eq!(
            err.to_string(),
            format!("failed to parse manifest at {}", path.display())
        );
    }

    #[test]
    fn test_dcu_error_no_manifest() {
        let path = PathBuf::from("/some/dir");
        let err = DcuError::NoManifest { path: path.clone() };
        assert_eq!(
            err.to_string(),
            format!("no manifest found in {}", path.display())
        );
    }

    #[test]
    fn test_dcu_error_registry_lookup() {
        let err = DcuError::RegistryLookup {
            package: "lodash".to_string(),
            detail: "connection timeout".to_string(),
        };
        // Display now surfaces `detail` — without it, users hit a dead-end
        // when GitHub Tags API rate-limits them (no hint to set GITHUB_TOKEN).
        assert_eq!(
            err.to_string(),
            "registry lookup failed for package `lodash`: connection timeout"
        );
    }

    #[test]
    fn test_dcu_error_semver_parse() {
        let err = DcuError::SemverParse {
            input: "not.a.version".to_string(),
            detail: "invalid semver format".to_string(),
        };
        assert_eq!(err.to_string(), "invalid semver: not.a.version");
    }
}
