//! Cargo.toml parsing and format-preserving dependency updates via `toml_edit`.

use dcu_core::{DependencySection, DependencySpec, PlannedUpdate};
use toml_edit::{DocumentMut, Item, Table, Value};

/// Known Cargo.toml dependency sections.
const CARGO_SECTIONS: &[(DependencySection, &str)] = &[
    (DependencySection::Dependencies, "dependencies"),
    (DependencySection::DevDependencies, "dev-dependencies"),
    (DependencySection::BuildDependencies, "build-dependencies"),
];

/// A parsed Cargo.toml file.
#[derive(Debug)]
pub struct CargoTomlManifest {
    /// The original raw text.
    pub original_text: String,
    /// The `toml_edit` document (format-preserving).
    pub doc: DocumentMut,
    /// Collected dependencies.
    pub dependencies: Vec<DependencySpec>,
}

impl CargoTomlManifest {
    /// Parse a Cargo.toml from raw text.
    ///
    /// # Errors
    ///
    /// Returns an error if the text is not valid TOML.
    pub fn parse(text: &str) -> Result<Self, CargoTomlError> {
        let doc: DocumentMut = text
            .parse()
            .map_err(|e: toml_edit::TomlError| CargoTomlError::ParseFailed(e.to_string()))?;

        let dependencies = Self::collect_dependencies(&doc);

        Ok(Self {
            original_text: text.to_owned(),
            doc,
            dependencies,
        })
    }

    fn collect_dependencies(doc: &DocumentMut) -> Vec<DependencySpec> {
        let mut deps = Vec::new();

        for &(section, key) in CARGO_SECTIONS {
            if let Some(table) = doc.get(key).and_then(Item::as_table) {
                Self::collect_from_table(table, section, &mut deps);
            }
        }

        // Also check [workspace.dependencies]
        if let Some(ws) = doc.get("workspace").and_then(Item::as_table) {
            if let Some(ws_deps) = ws.get("dependencies").and_then(Item::as_table) {
                Self::collect_from_table(
                    ws_deps,
                    DependencySection::WorkspaceDependencies,
                    &mut deps,
                );
            }
        }

        deps
    }

    fn collect_from_table(
        table: &Table,
        section: DependencySection,
        deps: &mut Vec<DependencySpec>,
    ) {
        for (name, item) in table {
            if let Some(version) = Self::extract_version(item) {
                // Skip path/git dependencies without a version
                if !version.is_empty() {
                    deps.push(DependencySpec {
                        name: name.to_owned(),
                        current_req: version,
                        section,
                    });
                }
            }
        }
    }

    /// Extract the version string from a dependency item.
    ///
    /// Handles:
    /// - `dep = "1.0"` (string form)
    /// - `dep = { version = "1.0", features = [...] }` (table form)
    /// - `dep = { workspace = true }` → skipped
    /// - `dep = { git = "..." }` → skipped (no version)
    fn extract_version(item: &Item) -> Option<String> {
        match item {
            Item::Value(Value::String(s)) => Some(s.value().to_owned()),
            Item::Value(Value::InlineTable(t)) => {
                // Skip workspace = true
                if t.get("workspace").and_then(Value::as_bool).unwrap_or(false) {
                    return None;
                }
                // Skip git/path-only deps
                t.get("version").and_then(Value::as_str).map(String::from)
            }
            Item::Table(t) => {
                if t.get("workspace")
                    .and_then(Item::as_value)
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    return None;
                }
                t.get("version").and_then(Item::as_str).map(String::from)
            }
            _ => None,
        }
    }

    /// Apply planned updates to the document, returning the modified text.
    ///
    /// Uses `toml_edit` for format-preserving modifications.
    ///
    /// # Errors
    ///
    /// Returns an error if a dependency cannot be found in the document.
    pub fn apply_updates(&mut self, updates: &[PlannedUpdate]) -> Result<String, CargoTomlError> {
        for update in updates {
            let section_key = match update.section {
                DependencySection::Dependencies => "dependencies",
                DependencySection::DevDependencies => "dev-dependencies",
                DependencySection::BuildDependencies => "build-dependencies",
                DependencySection::WorkspaceDependencies => {
                    // Handle workspace.dependencies separately
                    if let Some(ws) = self.doc.get_mut("workspace").and_then(Item::as_table_mut) {
                        if let Some(ws_deps) =
                            ws.get_mut("dependencies").and_then(Item::as_table_mut)
                        {
                            Self::update_dep_in_table(ws_deps, &update.name, &update.to)?;
                        }
                    }
                    continue;
                }
                _ => continue, // Other sections not applicable to Cargo.toml
            };

            if let Some(table) = self.doc.get_mut(section_key).and_then(Item::as_table_mut) {
                Self::update_dep_in_table(table, &update.name, &update.to)?;
            }
        }

        Ok(self.doc.to_string())
    }

    fn update_dep_in_table(
        table: &mut Table,
        name: &str,
        new_version: &str,
    ) -> Result<(), CargoTomlError> {
        let Some(item) = table.get_mut(name) else {
            return Err(CargoTomlError::DependencyNotFound(name.to_owned()));
        };

        match item {
            Item::Value(Value::String(s)) => {
                let decor = s.decor().clone();
                let mut new_s = toml_edit::Formatted::new(new_version.to_owned());
                *new_s.decor_mut() = decor;
                *s = new_s;
            }
            Item::Value(Value::InlineTable(t)) => {
                if let Some(v) = t.get_mut("version") {
                    *v = Value::String(toml_edit::Formatted::new(new_version.to_owned()));
                }
            }
            Item::Table(t) => {
                t["version"] = toml_edit::value(new_version);
            }
            _ => {}
        }

        Ok(())
    }
}

/// Errors from Cargo.toml operations.
#[derive(Debug, thiserror::Error)]
pub enum CargoTomlError {
    #[error("failed to parse Cargo.toml: {0}")]
    ParseFailed(String),
    #[error("dependency not found: {0}")]
    DependencyNotFound(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_deps() {
        let toml = r#"
[package]
name = "my-crate"
version = "0.1.0"

[dependencies]
serde = "1.0"
tokio = "1.0"
"#;
        let manifest = CargoTomlManifest::parse(toml).unwrap();
        assert_eq!(manifest.dependencies.len(), 2);
        assert_eq!(manifest.dependencies[0].name, "serde");
        assert_eq!(manifest.dependencies[0].current_req, "1.0");
        assert_eq!(
            manifest.dependencies[0].section,
            DependencySection::Dependencies
        );
    }

    #[test]
    fn test_parse_table_form_deps() {
        let toml = r#"
[dependencies]
serde = { version = "1.0", features = ["derive"] }
tokio = { version = "1.0", features = ["full"] }
"#;
        let manifest = CargoTomlManifest::parse(toml).unwrap();
        assert_eq!(manifest.dependencies.len(), 2);
        assert_eq!(manifest.dependencies[0].current_req, "1.0");
    }

    #[test]
    fn test_parse_dev_dependencies() {
        let toml = r#"
[dev-dependencies]
insta = "1.0"
"#;
        let manifest = CargoTomlManifest::parse(toml).unwrap();
        assert_eq!(manifest.dependencies.len(), 1);
        assert_eq!(
            manifest.dependencies[0].section,
            DependencySection::DevDependencies
        );
    }

    #[test]
    fn test_parse_build_dependencies() {
        let toml = r#"
[build-dependencies]
cc = "1.0"
"#;
        let manifest = CargoTomlManifest::parse(toml).unwrap();
        assert_eq!(manifest.dependencies.len(), 1);
        assert_eq!(
            manifest.dependencies[0].section,
            DependencySection::BuildDependencies
        );
    }

    #[test]
    fn test_parse_workspace_dependencies() {
        let toml = r#"
[workspace.dependencies]
serde = "1.0"
tokio = { version = "1.0", features = ["full"] }
"#;
        let manifest = CargoTomlManifest::parse(toml).unwrap();
        assert_eq!(manifest.dependencies.len(), 2);
        assert_eq!(
            manifest.dependencies[0].section,
            DependencySection::WorkspaceDependencies
        );
    }

    #[test]
    fn test_skip_workspace_true() {
        let toml = r#"
[dependencies]
serde = { workspace = true }
tokio = "1.0"
"#;
        let manifest = CargoTomlManifest::parse(toml).unwrap();
        assert_eq!(manifest.dependencies.len(), 1);
        assert_eq!(manifest.dependencies[0].name, "tokio");
    }

    #[test]
    fn test_skip_git_deps() {
        let toml = r#"
[dependencies]
my-fork = { git = "https://github.com/user/repo" }
tokio = "1.0"
"#;
        let manifest = CargoTomlManifest::parse(toml).unwrap();
        assert_eq!(manifest.dependencies.len(), 1);
        assert_eq!(manifest.dependencies[0].name, "tokio");
    }

    #[test]
    fn test_skip_path_deps() {
        let toml = r#"
[dependencies]
my-local = { path = "../my-local" }
tokio = "1.0"
"#;
        let manifest = CargoTomlManifest::parse(toml).unwrap();
        assert_eq!(manifest.dependencies.len(), 1);
    }

    #[test]
    fn test_apply_updates_string_form() {
        let toml = r#"
[dependencies]
serde = "1.0"
tokio = "1.0"
"#;
        let mut manifest = CargoTomlManifest::parse(toml).unwrap();
        let updates = vec![PlannedUpdate {
            name: "serde".to_owned(),
            section: DependencySection::Dependencies,
            from: "1.0".to_owned(),
            to: "1.0.228".to_owned(),
        }];

        let result = manifest.apply_updates(&updates).unwrap();
        assert!(result.contains("\"1.0.228\""));
        assert!(result.contains("tokio = \"1.0\""));
    }

    #[test]
    fn test_apply_updates_table_form() {
        let toml = r#"
[dependencies]
serde = { version = "1.0", features = ["derive"] }
"#;
        let mut manifest = CargoTomlManifest::parse(toml).unwrap();
        let updates = vec![PlannedUpdate {
            name: "serde".to_owned(),
            section: DependencySection::Dependencies,
            from: "1.0".to_owned(),
            to: "1.0.228".to_owned(),
        }];

        let result = manifest.apply_updates(&updates).unwrap();
        assert!(result.contains("\"1.0.228\""));
        assert!(result.contains("features = [\"derive\"]"));
    }

    #[test]
    fn test_comments_preserved() {
        let toml = r#"
# This is an important comment
[dependencies]
# Serialization
serde = "1.0"
# Async runtime
tokio = "1.0"
"#;
        let mut manifest = CargoTomlManifest::parse(toml).unwrap();
        let updates = vec![PlannedUpdate {
            name: "serde".to_owned(),
            section: DependencySection::Dependencies,
            from: "1.0".to_owned(),
            to: "2.0".to_owned(),
        }];

        let result = manifest.apply_updates(&updates).unwrap();
        assert!(result.contains("# This is an important comment"));
        assert!(result.contains("# Serialization"));
        assert!(result.contains("# Async runtime"));
    }

    #[test]
    fn test_no_deps_empty() {
        let toml = r#"
[package]
name = "empty"
version = "0.1.0"
"#;
        let manifest = CargoTomlManifest::parse(toml).unwrap();
        assert!(manifest.dependencies.is_empty());
    }

    #[test]
    fn test_mixed_sections() {
        let toml = r#"
[dependencies]
serde = "1.0"

[dev-dependencies]
insta = "1.0"

[build-dependencies]
cc = "1.0"
"#;
        let manifest = CargoTomlManifest::parse(toml).unwrap();
        assert_eq!(manifest.dependencies.len(), 3);
    }
}
