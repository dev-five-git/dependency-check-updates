//! GitHub Actions ecosystem support for dependency-check-updates.
//!
//! Handles `.github/workflows/*.yml` / `*.yaml` and composite `action.yml`
//! files. Parses `uses: owner/repo[/path]@ref` directives via line-based
//! scanning (full YAML round-tripping would clobber comments, anchors, and
//! whitespace), resolves the latest tag from the GitHub Tags API, and applies
//! surgical byte-range patches that touch only the version ref.
//!
//! Refs that do not look like version numbers (`@main`, `@master`, branch
//! names, commit SHAs) are intentionally skipped: tracking the moving target
//! they point at is the caller's responsibility.

pub mod parser;
pub mod patcher;
pub mod registry;

use std::path::Path;

use dependency_check_updates_core::manifest::{ManifestHandler, ParsedManifest};
use dependency_check_updates_core::{DcuError, ManifestKind, ManifestRef, PlannedUpdate};

pub use parser::{UsesLocation, WorkflowManifest, WorkflowParseError, is_version_ref};
pub use patcher::{Patch, PatchError, WorkflowPatcher};
pub use registry::GitHubActionsRegistry;

/// Handler for GitHub Actions workflow / action manifest files.
pub struct GitHubHandler;

impl ManifestHandler for GitHubHandler {
    fn parse(&self, text: &str, path: &Path) -> Result<ParsedManifest, DcuError> {
        let manifest = WorkflowManifest::parse(text).map_err(|e| DcuError::ManifestParse {
            path: path.to_path_buf(),
            detail: e.to_string(),
        })?;

        Ok(ParsedManifest {
            manifest_ref: ManifestRef {
                path: path.to_path_buf(),
                kind: ManifestKind::GitHubWorkflow,
            },
            original_text: manifest.original_text,
            dependencies: manifest.dependencies,
        })
    }

    fn apply_updates(&self, text: &str, updates: &[PlannedUpdate]) -> Result<String, DcuError> {
        WorkflowPatcher::apply(text, updates).map_err(|e| DcuError::PatchFailed {
            path: std::path::PathBuf::from("workflow"),
            detail: e.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dependency_check_updates_core::DependencySection;
    use std::path::Path;

    #[test]
    fn test_handler_parse_simple_workflow() {
        let yaml = "jobs:\n  test:\n    steps:\n      - uses: actions/checkout@v4\n";
        let parsed = GitHubHandler
            .parse(yaml, Path::new(".github/workflows/CI.yml"))
            .unwrap();
        assert_eq!(parsed.dependencies.len(), 1);
        assert_eq!(parsed.dependencies[0].name, "actions/checkout");
        assert_eq!(parsed.dependencies[0].current_req, "v4");
        assert_eq!(
            parsed.dependencies[0].section,
            DependencySection::GitHubActions
        );
    }

    #[test]
    fn test_handler_parse_skips_branches_and_shas() {
        let yaml = concat!(
            "jobs:\n  test:\n    steps:\n",
            "      - uses: actions/checkout@v4\n",
            "      - uses: changepacks/action@main\n",
            "      - uses: foo/bar@8e5e7e5a3b4c1234abcdef0123456789abcdef01\n",
        );
        let parsed = GitHubHandler
            .parse(yaml, Path::new(".github/workflows/CI.yml"))
            .unwrap();
        // Only the v4 ref is parsed; @main + 40-char SHA are skipped.
        assert_eq!(parsed.dependencies.len(), 1);
        assert_eq!(parsed.dependencies[0].name, "actions/checkout");
    }

    #[test]
    fn test_handler_apply_updates_preserves_unrelated_text() {
        let yaml = concat!(
            "jobs:\n  test:\n    steps:\n",
            "      - uses: actions/checkout@v4\n",
            "      - uses: changepacks/action@main # keep this one\n",
        );
        let updates = vec![PlannedUpdate {
            name: "actions/checkout".to_owned(),
            section: DependencySection::GitHubActions,
            from: "v4".to_owned(),
            to: "v5".to_owned(),
        }];
        let result = GitHubHandler.apply_updates(yaml, &updates).unwrap();
        assert!(result.contains("actions/checkout@v5"));
        assert!(result.contains("changepacks/action@main # keep this one"));
        assert!(!result.contains("actions/checkout@v4"));
    }
}
