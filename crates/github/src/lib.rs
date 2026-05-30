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

#![warn(missing_docs)]

mod parser;
mod patcher;
mod registry;

use std::path::Path;

use dependency_check_updates_core::manifest::{ManifestHandler, ParsedManifest};
use dependency_check_updates_core::{DcuError, ManifestKind, ManifestRef, PlannedUpdate};

use parser::WorkflowManifest;
use patcher::WorkflowPatcher;
pub use registry::GitHubActionsRegistry;

/// Handler for GitHub Actions workflow / action manifest files.
pub struct GitHubHandler;

impl ManifestHandler for GitHubHandler {
    fn parse(&self, text: &str, path: &Path) -> Result<ParsedManifest, DcuError> {
        let manifest = WorkflowManifest::parse(text);

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
        // `WorkflowPatcher::apply` only fails on overlapping patches, which the
        // line-by-line scanner can never produce (each `uses:` ref occupies a
        // distinct, byte-disjoint span and every update consumes a location at
        // most once). The error arm is therefore unreachable through this path.
        Ok(WorkflowPatcher::apply(text, updates).expect("workflow patches never overlap"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dependency_check_updates_core::DependencySection;
    use rstest::rstest;
    use std::path::Path;

    #[rstest]
    // Plain workflow with a single version-like ref.
    #[case::simple_workflow(
        "jobs:\n  test:\n    steps:\n      - uses: actions/checkout@v4\n",
        1,
        "actions/checkout",
        "v4"
    )]
    // The @main branch ref and the 40-char SHA are intentionally skipped;
    // only the v4 ref survives into `dependencies`.
    #[case::skips_branches_and_shas(
        concat!(
            "jobs:\n  test:\n    steps:\n",
            "      - uses: actions/checkout@v4\n",
            "      - uses: changepacks/action@main\n",
            "      - uses: foo/bar@8e5e7e5a3b4c1234abcdef0123456789abcdef01\n",
        ),
        1,
        "actions/checkout",
        "v4"
    )]
    fn handler_parse_workflow(
        #[case] yaml: &str,
        #[case] expected_len: usize,
        #[case] expected_first_name: &str,
        #[case] expected_first_req: &str,
    ) {
        let parsed = GitHubHandler
            .parse(yaml, Path::new(".github/workflows/CI.yml"))
            .unwrap();
        assert_eq!(parsed.dependencies.len(), expected_len);
        assert_eq!(parsed.dependencies[0].name, expected_first_name);
        assert_eq!(parsed.dependencies[0].current_req, expected_first_req);
        assert_eq!(
            parsed.dependencies[0].section,
            DependencySection::GitHubActions
        );
    }

    #[test]
    fn handler_apply_updates_preserves_unrelated_text() {
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
