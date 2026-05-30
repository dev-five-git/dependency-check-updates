//! Node.js ecosystem support for dependency-check-updates.
//!
//! Handles `package.json` parsing, npm registry lookups, and version resolution.
//! Follows the changepacks pattern of one crate per language ecosystem.

#![warn(missing_docs)]

mod parser;
mod patcher;
mod registry;

use std::path::Path;

use dependency_check_updates_core::manifest::{ManifestHandler, ParsedManifest};
use dependency_check_updates_core::{DcuError, ManifestKind, ManifestRef, PlannedUpdate};

use parser::PackageJsonManifest;
use patcher::{JsonPatcher, Patch};
pub use registry::NpmRegistry;

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
        let locations = JsonPatcher::scan_for_updates(text, updates);

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

#[cfg(test)]
mod tests {
    use super::*;
    use dependency_check_updates_core::manifest::ManifestHandler;
    use dependency_check_updates_core::{DependencySection, DependencySpec, PlannedUpdate};
    use rstest::rstest;
    use std::path::Path;

    /// Test-only helper: true when the section belongs to the Node ecosystem.
    fn is_node_ecosystem(dep: &DependencySpec) -> bool {
        matches!(
            dep.section,
            DependencySection::Dependencies
                | DependencySection::DevDependencies
                | DependencySection::PeerDependencies
                | DependencySection::OptionalDependencies
        )
    }

    #[rstest]
    // raw JSON, expected dependency count (None ⇒ parse must error).
    #[case::with_deps(
        r#"{"dependencies": {"react": "^18.0.0", "lodash": "^4.17.0"}}"#,
        Some(2),
    )]
    #[case::empty_object(r#"{"name": "test"}"#, Some(0))]
    #[case::invalid_json("not json", None)]
    fn node_handler_parse_cases(#[case] text: &str, #[case] expected_count: Option<usize>) {
        let handler = NodeHandler;
        let result = handler.parse(text, Path::new("package.json"));
        match expected_count {
            Some(n) => {
                let parsed = result.unwrap();
                assert_eq!(parsed.dependencies.len(), n);
                assert_eq!(parsed.manifest_ref.kind, ManifestKind::PackageJson);
            }
            None => assert!(result.is_err()),
        }
    }

    #[test]
    fn node_handler_apply_updates_single() {
        let handler = NodeHandler;
        let text = "{\n  \"dependencies\": {\n    \"react\": \"^17.0.0\"\n  }\n}\n";
        let updates = vec![PlannedUpdate {
            name: "react".to_owned(),
            section: DependencySection::Dependencies,
            from: "^17.0.0".to_owned(),
            to: "^18.2.0".to_owned(),
        }];
        let result = handler.apply_updates(text, &updates).unwrap();
        assert!(result.contains("\"^18.2.0\""));
        assert!(!result.contains("\"^17.0.0\""));
    }

    #[test]
    fn node_handler_apply_updates_multiple() {
        let handler = NodeHandler;
        let text = r#"{
  "dependencies": {
    "react": "^17.0.0"
  },
  "devDependencies": {
    "typescript": "^4.0.0"
  }
}
"#;
        let updates = vec![
            PlannedUpdate {
                name: "react".to_owned(),
                section: DependencySection::Dependencies,
                from: "^17.0.0".to_owned(),
                to: "^18.2.0".to_owned(),
            },
            PlannedUpdate {
                name: "typescript".to_owned(),
                section: DependencySection::DevDependencies,
                from: "^4.0.0".to_owned(),
                to: "^5.3.0".to_owned(),
            },
        ];
        let result = handler.apply_updates(text, &updates).unwrap();
        assert!(result.contains("\"^18.2.0\""));
        assert!(result.contains("\"^5.3.0\""));
    }

    #[test]
    fn node_handler_apply_updates_empty() {
        let handler = NodeHandler;
        let text = "{\n  \"dependencies\": {\n    \"react\": \"^17.0.0\"\n  }\n}\n";
        let result = handler.apply_updates(text, &[]).unwrap();
        assert_eq!(result, text);
    }

    #[test]
    fn node_handler_apply_updates_patch_failed_validation() {
        // `to` contains a raw `"` which, after byte-range substitution into
        // the JSON, breaks the document. `apply_patches` then fails its
        // `serde_json` re-validation and returns `ValidationFailed`, which
        // `apply_updates` maps to `DcuError::PatchFailed` — covering the
        // `.map_err(DcuError::PatchFailed)` arm.
        let handler = NodeHandler;
        let text = "{\n  \"dependencies\": {\n    \"react\": \"^17.0.0\"\n  }\n}\n";
        let updates = vec![PlannedUpdate {
            name: "react".to_owned(),
            section: DependencySection::Dependencies,
            from: "^17.0.0".to_owned(),
            to: "\"".to_owned(),
        }];
        let result = handler.apply_updates(text, &updates);
        assert!(result.is_err());
        let err_str = result.unwrap_err().to_string();
        assert!(
            err_str.contains("package.json"),
            "expected PatchFailed with package.json path, got: {err_str}"
        );
    }

    #[rstest]
    #[case::node_dependencies(DependencySection::Dependencies, true)]
    #[case::non_node_build_dependencies(DependencySection::BuildDependencies, false)]
    fn is_node_ecosystem_cases(#[case] section: DependencySection, #[case] expected: bool) {
        let dep = DependencySpec {
            name: "pkg".to_owned(),
            current_req: "^1.0.0".to_owned(),
            section,
        };
        assert_eq!(is_node_ecosystem(&dep), expected);
    }
}
