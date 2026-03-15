use miette::Diagnostic;
use std::path::PathBuf;
use thiserror::Error;

/// Main error type for dcu operations.
#[derive(Debug, Error, Diagnostic)]
pub enum DcuError {
    #[error("failed to read manifest at {path}")]
    #[diagnostic(code(dcu::io_error), help("make sure the file exists and is readable"))]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse manifest at {path}")]
    #[diagnostic(code(dcu::parse_error))]
    ManifestParse { path: PathBuf, detail: String },

    #[error("registry lookup failed for package `{package}`")]
    #[diagnostic(code(dcu::registry_error), help("check your internet connection"))]
    RegistryLookup { package: String, detail: String },

    #[error("failed to apply patch to {path}")]
    #[diagnostic(code(dcu::patch_error))]
    PatchFailed { path: PathBuf, detail: String },

    #[error("invalid semver: {input}")]
    #[diagnostic(code(dcu::semver_error))]
    SemverParse { input: String, detail: String },

    #[error("no manifest found in {path}")]
    #[diagnostic(
        code(dcu::no_manifest),
        help("run dcu in a directory containing package.json, or use --manifest")
    )]
    NoManifest { path: PathBuf },
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
        assert_eq!(
            err.to_string(),
            "registry lookup failed for package `lodash`"
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
