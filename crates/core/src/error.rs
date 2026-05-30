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
    use rstest::rstest;

    /// Build an [`DcuError::Io`] case. Constructed via a helper because
    /// `std::io::Error` is not `Clone`, so it cannot live directly in an
    /// `#[rstest]` case literal that is materialised once per generated test.
    fn io_err() -> DcuError {
        DcuError::Io {
            path: PathBuf::from("test.json"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "file not found"),
        }
    }

    fn manifest_parse_err() -> DcuError {
        DcuError::ManifestParse {
            path: PathBuf::from("package.json"),
            detail: "unexpected token".to_owned(),
        }
    }

    fn no_manifest_err() -> DcuError {
        DcuError::NoManifest {
            path: PathBuf::from("/some/dir"),
        }
    }

    fn registry_lookup_err() -> DcuError {
        DcuError::RegistryLookup {
            package: "lodash".to_owned(),
            detail: "connection timeout".to_owned(),
        }
    }

    fn semver_parse_err() -> DcuError {
        DcuError::SemverParse {
            input: "not.a.version".to_owned(),
            detail: "invalid semver format".to_owned(),
        }
    }

    /// Verifies the [`std::fmt::Display`] output for every variant. Variants
    /// whose message embeds a path use `Path::display()` so the expected
    /// string is computed at case time to stay correct on every platform.
    ///
    /// The `RegistryLookup` case in particular asserts that `detail` is
    /// surfaced — without it, users hit a dead-end when GitHub Tags API
    /// rate-limits them (no hint to set `GITHUB_TOKEN`).
    #[rstest]
    #[case::io(
        io_err(),
        format!("failed to read manifest at {}", PathBuf::from("test.json").display())
    )]
    #[case::manifest_parse(
        manifest_parse_err(),
        format!("failed to parse manifest at {}", PathBuf::from("package.json").display())
    )]
    #[case::no_manifest(
        no_manifest_err(),
        format!("no manifest found in {}", PathBuf::from("/some/dir").display())
    )]
    #[case::registry_lookup(
        registry_lookup_err(),
        "registry lookup failed for package `lodash`: connection timeout".to_owned()
    )]
    #[case::semver_parse(
        semver_parse_err(),
        "invalid semver: not.a.version".to_owned()
    )]
    fn dcu_error_display(#[case] err: DcuError, #[case] expected: String) {
        assert_eq!(err.to_string(), expected);
    }
}
