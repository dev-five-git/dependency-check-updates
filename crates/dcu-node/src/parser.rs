//! Package.json parsing and dependency collection.

use dcu_core::{DependencySection, DependencySpec};
use serde_json::Value;

/// Sections to collect dependencies from in a package.json.
pub const DEPENDENCY_SECTIONS: &[(DependencySection, &str)] = &[
    (DependencySection::Dependencies, "dependencies"),
    (DependencySection::DevDependencies, "devDependencies"),
    (DependencySection::PeerDependencies, "peerDependencies"),
    (
        DependencySection::OptionalDependencies,
        "optionalDependencies",
    ),
];

/// A parsed package.json file.
#[derive(Debug)]
pub struct PackageJsonManifest {
    /// The original raw text (preserved for surgical patching).
    pub original_text: String,
    /// The parsed JSON value.
    pub parsed: Value,
    /// All collected dependency specs.
    pub dependencies: Vec<DependencySpec>,
}

impl PackageJsonManifest {
    /// Parse a package.json from raw text.
    ///
    /// # Errors
    ///
    /// Returns an error if the text is not valid JSON.
    pub fn parse(text: &str) -> Result<Self, PackageJsonError> {
        let parsed: Value =
            serde_json::from_str(text).map_err(|e| PackageJsonError::ParseFailed(e.to_string()))?;

        let dependencies = Self::collect_dependencies(&parsed);

        Ok(Self {
            original_text: text.to_owned(),
            parsed,
            dependencies,
        })
    }

    fn collect_dependencies(root: &Value) -> Vec<DependencySpec> {
        let mut deps = Vec::new();

        for &(section, key) in DEPENDENCY_SECTIONS {
            if let Some(Value::Object(map)) = root.get(key) {
                for (name, value) in map {
                    if let Some(version_str) = value.as_str() {
                        if is_version_spec(version_str) {
                            deps.push(DependencySpec {
                                name: name.clone(),
                                current_req: version_str.to_owned(),
                                section,
                            });
                        }
                    }
                    // Non-string values (object form like { "version": "^1.0" }) are skipped.
                }
            }
        }

        deps
    }
}

/// Check if a dependency value is a resolvable version spec.
///
/// Filters out non-semver specifiers like workspace protocols, npm aliases,
/// git URLs, file paths, and link protocols.
fn is_version_spec(value: &str) -> bool {
    !value.starts_with("workspace:")
        && !value.starts_with("npm:")
        && !value.starts_with("git+")
        && !value.starts_with("git:")
        && !value.starts_with("github:")
        && !value.starts_with("http:")
        && !value.starts_with("https:")
        && !value.starts_with("file:")
        && !value.starts_with("link:")
}

/// Errors from package.json parsing.
#[derive(Debug, thiserror::Error)]
pub enum PackageJsonError {
    #[error("failed to parse package.json: {0}")]
    ParseFailed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_dependencies() {
        let json = r#"{
  "name": "test",
  "dependencies": {
    "react": "^18.2.0",
    "react-dom": "^18.2.0"
  }
}"#;
        let manifest = PackageJsonManifest::parse(json).unwrap();
        assert_eq!(manifest.dependencies.len(), 2);
        assert_eq!(manifest.dependencies[0].name, "react");
        assert_eq!(manifest.dependencies[0].current_req, "^18.2.0");
        assert_eq!(
            manifest.dependencies[0].section,
            DependencySection::Dependencies
        );
    }

    #[test]
    fn test_dev_dependencies() {
        let json = r#"{
  "devDependencies": {
    "typescript": "^5.0.0",
    "eslint": "^8.0.0"
  }
}"#;
        let manifest = PackageJsonManifest::parse(json).unwrap();
        assert_eq!(manifest.dependencies.len(), 2);
        assert_eq!(
            manifest.dependencies[0].section,
            DependencySection::DevDependencies
        );
    }

    #[test]
    fn test_peer_dependencies() {
        let json = r#"{
  "peerDependencies": {
    "react": "^17.0.0 || ^18.0.0"
  }
}"#;
        let manifest = PackageJsonManifest::parse(json).unwrap();
        assert_eq!(manifest.dependencies.len(), 1);
        assert_eq!(
            manifest.dependencies[0].section,
            DependencySection::PeerDependencies
        );
    }

    #[test]
    fn test_optional_dependencies() {
        let json = r#"{
  "optionalDependencies": {
    "fsevents": "^2.3.0"
  }
}"#;
        let manifest = PackageJsonManifest::parse(json).unwrap();
        assert_eq!(manifest.dependencies.len(), 1);
        assert_eq!(
            manifest.dependencies[0].section,
            DependencySection::OptionalDependencies
        );
    }

    #[test]
    fn test_all_sections_mixed() {
        let json = r#"{
  "dependencies": { "react": "^18.0.0" },
  "devDependencies": { "typescript": "^5.0.0" },
  "peerDependencies": { "vue": "^3.0.0" },
  "optionalDependencies": { "fsevents": "^2.3.0" }
}"#;
        let manifest = PackageJsonManifest::parse(json).unwrap();
        assert_eq!(manifest.dependencies.len(), 4);

        let sections: Vec<_> = manifest.dependencies.iter().map(|d| d.section).collect();
        assert!(sections.contains(&DependencySection::Dependencies));
        assert!(sections.contains(&DependencySection::DevDependencies));
        assert!(sections.contains(&DependencySection::PeerDependencies));
        assert!(sections.contains(&DependencySection::OptionalDependencies));
    }

    #[test]
    fn test_empty_dependencies() {
        let json = r#"{ "dependencies": {} }"#;
        let manifest = PackageJsonManifest::parse(json).unwrap();
        assert!(manifest.dependencies.is_empty());
    }

    #[test]
    fn test_missing_dependencies_key() {
        let json = r#"{ "name": "test", "version": "1.0.0" }"#;
        let manifest = PackageJsonManifest::parse(json).unwrap();
        assert!(manifest.dependencies.is_empty());
    }

    #[test]
    fn test_scoped_packages() {
        let json = r#"{
  "dependencies": {
    "@types/react": "^18.0.0",
    "@babel/core": "^7.20.0",
    "@scope/nested-pkg": "^1.0.0"
  }
}"#;
        let manifest = PackageJsonManifest::parse(json).unwrap();
        assert_eq!(manifest.dependencies.len(), 3);
        assert_eq!(manifest.dependencies[0].name, "@types/react");
        assert_eq!(manifest.dependencies[1].name, "@babel/core");
        assert_eq!(manifest.dependencies[2].name, "@scope/nested-pkg");
    }

    #[test]
    fn test_workspace_protocol_skipped() {
        let json = r#"{
  "dependencies": {
    "my-lib": "workspace:*",
    "my-other": "workspace:^1.0.0",
    "react": "^18.0.0"
  }
}"#;
        let manifest = PackageJsonManifest::parse(json).unwrap();
        assert_eq!(manifest.dependencies.len(), 1);
        assert_eq!(manifest.dependencies[0].name, "react");
    }

    #[test]
    fn test_npm_alias_skipped() {
        let json = r#"{
  "dependencies": {
    "my-react": "npm:react@^18.0.0",
    "lodash": "^4.17.0"
  }
}"#;
        let manifest = PackageJsonManifest::parse(json).unwrap();
        assert_eq!(manifest.dependencies.len(), 1);
        assert_eq!(manifest.dependencies[0].name, "lodash");
    }

    #[test]
    fn test_git_url_skipped() {
        let json = r#"{
  "dependencies": {
    "my-fork": "git+https://github.com/user/repo.git",
    "other-fork": "github:user/repo",
    "react": "^18.0.0"
  }
}"#;
        let manifest = PackageJsonManifest::parse(json).unwrap();
        assert_eq!(manifest.dependencies.len(), 1);
        assert_eq!(manifest.dependencies[0].name, "react");
    }

    #[test]
    fn test_file_and_link_skipped() {
        let json = r#"{
  "dependencies": {
    "local-pkg": "file:../local-pkg",
    "linked": "link:../linked",
    "react": "^18.0.0"
  }
}"#;
        let manifest = PackageJsonManifest::parse(json).unwrap();
        assert_eq!(manifest.dependencies.len(), 1);
        assert_eq!(manifest.dependencies[0].name, "react");
    }

    #[test]
    fn test_object_form_skipped() {
        let json = r#"{
  "dependencies": {
    "complex": { "version": "^1.0.0", "optional": true },
    "react": "^18.0.0"
  }
}"#;
        let manifest = PackageJsonManifest::parse(json).unwrap();
        assert_eq!(manifest.dependencies.len(), 1);
        assert_eq!(manifest.dependencies[0].name, "react");
    }

    #[test]
    fn test_original_text_preserved() {
        let json = "{\n  \"name\": \"test\",\n  \"version\": \"1.0.0\"\n}\n";
        let manifest = PackageJsonManifest::parse(json).unwrap();
        assert_eq!(manifest.original_text, json);
    }

    #[test]
    fn test_range_prefixes_collected() {
        let json = r#"{
  "dependencies": {
    "a": "^1.0.0",
    "b": "~1.0.0",
    "c": ">=1.0.0",
    "d": "*",
    "e": "latest",
    "f": "1.0.0",
    "g": "1.x",
    "h": ">=1.0.0 <2.0.0"
  }
}"#;
        let manifest = PackageJsonManifest::parse(json).unwrap();
        assert_eq!(manifest.dependencies.len(), 8);
    }

    #[test]
    fn test_invalid_json_returns_error() {
        let result = PackageJsonManifest::parse("not json");
        assert!(result.is_err());
    }

    #[test]
    fn test_http_url_skipped() {
        let json = r#"{
  "dependencies": {
    "tarball-pkg": "https://example.com/pkg.tgz",
    "http-pkg": "http://example.com/pkg.tgz",
    "react": "^18.0.0"
  }
}"#;
        let manifest = PackageJsonManifest::parse(json).unwrap();
        assert_eq!(manifest.dependencies.len(), 1);
        assert_eq!(manifest.dependencies[0].name, "react");
    }
}
