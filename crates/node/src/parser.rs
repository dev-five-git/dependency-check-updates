//! Package.json parsing and dependency collection.

use dependency_check_updates_core::{DependencySection, DependencySpec};
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
/// git URLs, file paths, and link protocols. Also filters out unresolvable
/// "always-newest" specifiers like `latest`, `*`, `x`, and `X`, since
/// updating those would be a no-op (they already mean "the latest version").
fn is_version_spec(value: &str) -> bool {
    let trimmed = value.trim();
    if matches!(trimmed, "latest" | "*" | "x" | "X" | "") {
        return false;
    }
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
    use rstest::rstest;

    #[rstest]
    // JSON in which a single dependency section is populated. Verifies every
    // collected entry's `(name, current_req, section)` triple against the
    // expected first-entry fields and overall count.
    #[case::dependencies(
        r#"{
  "name": "test",
  "dependencies": {
    "react": "^18.2.0",
    "react-dom": "^18.2.0"
  }
}"#,
        2,
        "react",
        "^18.2.0",
        DependencySection::Dependencies
    )]
    #[case::dev_dependencies(
        r#"{
  "devDependencies": {
    "typescript": "^5.0.0",
    "eslint": "^8.0.0"
  }
}"#,
        2,
        "typescript",
        "^5.0.0",
        DependencySection::DevDependencies
    )]
    #[case::peer_dependencies(
        r#"{
  "peerDependencies": {
    "react": "^17.0.0 || ^18.0.0"
  }
}"#,
        1,
        "react",
        "^17.0.0 || ^18.0.0",
        DependencySection::PeerDependencies
    )]
    #[case::optional_dependencies(
        r#"{
  "optionalDependencies": {
    "fsevents": "^2.3.0"
  }
}"#,
        1,
        "fsevents",
        "^2.3.0",
        DependencySection::OptionalDependencies
    )]
    fn parses_single_section(
        #[case] json: &str,
        #[case] expected_count: usize,
        #[case] first_name: &str,
        #[case] first_req: &str,
        #[case] section: DependencySection,
    ) {
        let manifest = PackageJsonManifest::parse(json).unwrap();
        assert_eq!(manifest.dependencies.len(), expected_count);
        assert_eq!(manifest.dependencies[0].name, first_name);
        assert_eq!(manifest.dependencies[0].current_req, first_req);
        assert_eq!(manifest.dependencies[0].section, section);
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

    #[rstest]
    #[case::empty_dependencies(r#"{ "dependencies": {} }"#)]
    #[case::missing_dependencies_key(r#"{ "name": "test", "version": "1.0.0" }"#)]
    fn no_dependencies_collected(#[case] json: &str) {
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

    #[rstest]
    // JSON containing one resolvable spec plus various skipped specs.
    // Only the named survivor remains after parsing.
    #[case::workspace_protocol(
        r#"{
  "dependencies": {
    "my-lib": "workspace:*",
    "my-other": "workspace:^1.0.0",
    "react": "^18.0.0"
  }
}"#,
        "react"
    )]
    #[case::npm_alias(
        r#"{
  "dependencies": {
    "my-react": "npm:react@^18.0.0",
    "lodash": "^4.17.0"
  }
}"#,
        "lodash"
    )]
    #[case::git_url(
        r#"{
  "dependencies": {
    "my-fork": "git+https://github.com/user/repo.git",
    "other-fork": "github:user/repo",
    "react": "^18.0.0"
  }
}"#,
        "react"
    )]
    #[case::file_and_link(
        r#"{
  "dependencies": {
    "local-pkg": "file:../local-pkg",
    "linked": "link:../linked",
    "react": "^18.0.0"
  }
}"#,
        "react"
    )]
    #[case::object_form(
        r#"{
  "dependencies": {
    "complex": { "version": "^1.0.0", "optional": true },
    "react": "^18.0.0"
  }
}"#,
        "react"
    )]
    #[case::latest_and_wildcard(
        r#"{
  "dependencies": {
    "always-new": "latest",
    "any": "*",
    "x-any": "x",
    "X-any": "X",
    "react": "^18.0.0"
  }
}"#,
        "react"
    )]
    #[case::http_url(
        r#"{
  "dependencies": {
    "tarball-pkg": "https://example.com/pkg.tgz",
    "http-pkg": "http://example.com/pkg.tgz",
    "react": "^18.0.0"
  }
}"#,
        "react"
    )]
    fn skips_unresolvable_specs(#[case] json: &str, #[case] survivor: &str) {
        let manifest = PackageJsonManifest::parse(json).unwrap();
        assert_eq!(manifest.dependencies.len(), 1);
        assert_eq!(manifest.dependencies[0].name, survivor);
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
        // `*` and `latest` are filtered out as unresolvable always-newest specs.
        assert_eq!(manifest.dependencies.len(), 6);
        let names: Vec<_> = manifest
            .dependencies
            .iter()
            .map(|d| d.name.as_str())
            .collect();
        assert!(!names.contains(&"d"));
        assert!(!names.contains(&"e"));
    }

    #[test]
    fn test_invalid_json_returns_error() {
        let result = PackageJsonManifest::parse("not json");
        assert!(result.is_err());
    }
}
