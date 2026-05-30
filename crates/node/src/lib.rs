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

    #[test]
    fn test_node_handler_parse() {
        let handler = NodeHandler;
        let text = r#"{"dependencies": {"react": "^18.0.0", "lodash": "^4.17.0"}}"#;
        let result = handler.parse(text, Path::new("package.json")).unwrap();
        assert_eq!(result.dependencies.len(), 2);
        assert_eq!(result.manifest_ref.kind, ManifestKind::PackageJson);
    }

    #[test]
    fn test_node_handler_parse_empty() {
        let handler = NodeHandler;
        let text = r#"{"name": "test"}"#;
        let result = handler.parse(text, Path::new("package.json")).unwrap();
        assert!(result.dependencies.is_empty());
    }

    #[test]
    fn test_node_handler_parse_invalid_json() {
        let handler = NodeHandler;
        let result = handler.parse("not json", Path::new("package.json"));
        assert!(result.is_err());
    }

    #[test]
    fn test_node_handler_apply_updates() {
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
    fn test_node_handler_apply_updates_multiple() {
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
    fn test_node_handler_apply_updates_empty() {
        let handler = NodeHandler;
        let text = "{\n  \"dependencies\": {\n    \"react\": \"^17.0.0\"\n  }\n}\n";
        let result = handler.apply_updates(text, &[]).unwrap();
        assert_eq!(result, text);
    }

    #[test]
    fn test_is_node_ecosystem() {
        let dep = DependencySpec {
            name: "react".to_owned(),
            current_req: "^18.0.0".to_owned(),
            section: DependencySection::Dependencies,
        };
        assert!(is_node_ecosystem(&dep));

        let dep = DependencySpec {
            name: "pkg".to_owned(),
            current_req: "^1.0".to_owned(),
            section: DependencySection::BuildDependencies,
        };
        assert!(!is_node_ecosystem(&dep));
    }
}
