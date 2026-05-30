//! Cargo.toml parsing and format-preserving dependency updates via `toml_edit`.

use dependency_check_updates_core::{DependencySection, DependencySpec, PlannedUpdate};
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
                // Skip path/git dependencies without a version, and skip
                // wildcard-only requirements like `*` which already mean
                // "any version" — updating them would be a meaningless no-op.
                if !version.is_empty() && version.trim() != "*" {
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
    use rstest::rstest;

    /// Expected parsed dependencies: list of `(name, current_req, section)`.
    type ExpectedDeps<'a> = &'a [(&'a str, &'a str, DependencySection)];

    /// Planned updates expressed as `(name, section, to)`; `from` is always
    /// `"1.0"` in these tests so it is filled in by the helper.
    type UpdateSpecs<'a> = &'a [(&'a str, DependencySection, &'a str)];

    fn build_updates(specs: UpdateSpecs<'_>) -> Vec<PlannedUpdate> {
        specs
            .iter()
            .map(|(name, section, to)| PlannedUpdate {
                name: (*name).to_owned(),
                section: *section,
                from: "1.0".to_owned(),
                to: (*to).to_owned(),
            })
            .collect()
    }

    #[rstest]
    #[case::simple_deps(
        r#"
[package]
name = "my-crate"
version = "0.1.0"

[dependencies]
serde = "1.0"
tokio = "1.0"
"#,
        &[
            ("serde", "1.0", DependencySection::Dependencies),
            ("tokio", "1.0", DependencySection::Dependencies),
        ]
    )]
    #[case::table_form_deps(
        r#"
[dependencies]
serde = { version = "1.0", features = ["derive"] }
tokio = { version = "1.0", features = ["full"] }
"#,
        &[
            ("serde", "1.0", DependencySection::Dependencies),
            ("tokio", "1.0", DependencySection::Dependencies),
        ]
    )]
    #[case::dev_dependencies(
        r#"
[dev-dependencies]
insta = "1.0"
"#,
        &[("insta", "1.0", DependencySection::DevDependencies)]
    )]
    #[case::build_dependencies(
        r#"
[build-dependencies]
cc = "1.0"
"#,
        &[("cc", "1.0", DependencySection::BuildDependencies)]
    )]
    #[case::workspace_dependencies(
        r#"
[workspace.dependencies]
serde = "1.0"
tokio = { version = "1.0", features = ["full"] }
"#,
        &[
            ("serde", "1.0", DependencySection::WorkspaceDependencies),
            ("tokio", "1.0", DependencySection::WorkspaceDependencies),
        ]
    )]
    #[case::skip_workspace_true(
        r#"
[dependencies]
serde = { workspace = true }
tokio = "1.0"
"#,
        &[("tokio", "1.0", DependencySection::Dependencies)]
    )]
    #[case::skip_git_deps(
        r#"
[dependencies]
my-fork = { git = "https://github.com/user/repo" }
tokio = "1.0"
"#,
        &[("tokio", "1.0", DependencySection::Dependencies)]
    )]
    #[case::skip_path_deps(
        r#"
[dependencies]
my-local = { path = "../my-local" }
tokio = "1.0"
"#,
        &[("tokio", "1.0", DependencySection::Dependencies)]
    )]
    #[case::no_deps_empty(
        r#"
[package]
name = "empty"
version = "0.1.0"
"#,
        &[]
    )]
    #[case::mixed_sections(
        r#"
[dependencies]
serde = "1.0"

[dev-dependencies]
insta = "1.0"

[build-dependencies]
cc = "1.0"
"#,
        &[
            ("serde", "1.0", DependencySection::Dependencies),
            ("insta", "1.0", DependencySection::DevDependencies),
            ("cc", "1.0", DependencySection::BuildDependencies),
        ]
    )]
    #[case::array_dep_value_skipped(
        r#"
[dependencies]
serde = "1.0"
weird = [1, 2, 3]
"#,
        &[("serde", "1.0", DependencySection::Dependencies)]
    )]
    #[case::skip_workspace_true_full_table_form(
        r#"
[dependencies.serde]
workspace = true

[dependencies]
tokio = "1.0"
"#,
        &[("tokio", "1.0", DependencySection::Dependencies)]
    )]
    #[case::full_table_form_version(
        r#"
[dependencies.serde]
version = "1.0"
features = ["derive"]
"#,
        &[("serde", "1.0", DependencySection::Dependencies)]
    )]
    // `test_extract_version_none_for_unknown_item` parses the same simple TOML
    // and only asserts the first dependency's `current_req`. Covered by the
    // identical assertions in `simple_deps` / this minimal case.
    #[case::extract_version_returns_simple_string(
        r#"
[dependencies]
serde = "1.0"
"#,
        &[("serde", "1.0", DependencySection::Dependencies)]
    )]
    // Pre-apply parse coverage for `apply_updates_unhandled_value_type`:
    // weird-dep first, then serde. Only serde is collected (len == 1).
    #[case::array_first_then_string(
        r#"
[dependencies]
weird-dep = [1, 2, 3]
serde = "1.0"
"#,
        &[("serde", "1.0", DependencySection::Dependencies)]
    )]
    fn parse_dependencies_cases(#[case] toml: &str, #[case] expected: ExpectedDeps<'_>) {
        let manifest = CargoTomlManifest::parse(toml).unwrap();
        assert_eq!(
            manifest.dependencies.len(),
            expected.len(),
            "dep count mismatch"
        );
        for (i, (name, req, section)) in expected.iter().enumerate() {
            assert_eq!(manifest.dependencies[i].name, *name);
            assert_eq!(manifest.dependencies[i].current_req, *req);
            assert_eq!(manifest.dependencies[i].section, *section);
        }
    }

    #[test]
    fn invalid_toml_returns_error() {
        let result = CargoTomlManifest::parse("not valid toml [[[");
        assert!(result.is_err());
    }

    #[rstest]
    #[case::string_form(
        r#"
[dependencies]
serde = "1.0"
tokio = "1.0"
"#,
        &[("serde", DependencySection::Dependencies, "1.0.228")],
        true,
        &["\"1.0.228\"", "tokio = \"1.0\""]
    )]
    #[case::table_form(
        r#"
[dependencies]
serde = { version = "1.0", features = ["derive"] }
"#,
        &[("serde", DependencySection::Dependencies, "1.0.228")],
        true,
        &["\"1.0.228\"", "features = [\"derive\"]"]
    )]
    #[case::comments_preserved(
        r#"
# This is an important comment
[dependencies]
# Serialization
serde = "1.0"
# Async runtime
tokio = "1.0"
"#,
        &[("serde", DependencySection::Dependencies, "2.0")],
        true,
        &[
            "# This is an important comment",
            "# Serialization",
            "# Async runtime",
        ]
    )]
    #[case::workspace_deps(
        r#"
[workspace.dependencies]
serde = "1.0"
tokio = { version = "1.0", features = ["full"] }
"#,
        &[("serde", DependencySection::WorkspaceDependencies, "2.0")],
        true,
        &["\"2.0\""]
    )]
    #[case::full_table_form(
        r#"
[dependencies.serde]
version = "1.0"
features = ["derive"]
"#,
        &[("serde", DependencySection::Dependencies, "1.0.228")],
        true,
        &["1.0.228"]
    )]
    #[case::dev_and_build_deps(
        r#"
[dev-dependencies]
insta = "1.0"

[build-dependencies]
cc = "1.0"
"#,
        &[
            ("insta", DependencySection::DevDependencies, "1.46"),
            ("cc", DependencySection::BuildDependencies, "1.2"),
        ],
        true,
        &["\"1.46\"", "\"1.2\""]
    )]
    #[case::non_applicable_section(
        r#"
[dependencies]
serde = "1.0"
"#,
        // ProjectDependencies is not applicable to Cargo.toml — silently skipped.
        // The original test used `from: ">=2.28.0"` / `to: ">=2.31.0"`; since
        // `apply_updates` ignores non-applicable sections entirely, the
        // request values are irrelevant — using the helper's `from = "1.0"` is
        // semantically identical (no edit occurs either way).
        &[("requests", DependencySection::ProjectDependencies, ">=2.31.0")],
        true,
        &["serde = \"1.0\""]
    )]
    #[case::workspace_table_form(
        r#"
[workspace.dependencies]
serde = { version = "1.0", features = ["derive"] }
"#,
        &[("serde", DependencySection::WorkspaceDependencies, "1.0.228")],
        true,
        &["\"1.0.228\""]
    )]
    #[case::unhandled_value_type(
        // Array-valued entry hits the catch-all `_ => {}` arm in
        // `update_dep_in_table` — apply succeeds silently, value unchanged.
        r#"
[dependencies]
weird-dep = [1, 2, 3]
serde = "1.0"
"#,
        &[("weird-dep", DependencySection::Dependencies, "2.0")],
        true,
        &["weird-dep = [1, 2, 3]"]
    )]
    #[case::dep_not_found(
        r#"
[dependencies]
serde = "1.0"
"#,
        &[("nonexistent", DependencySection::Dependencies, "2.0")],
        false,
        // Error path: substrings ignored.
        &[]
    )]
    fn apply_updates_cases(
        #[case] toml: &str,
        #[case] updates: UpdateSpecs<'_>,
        #[case] should_succeed: bool,
        #[case] expected_contains: &[&str],
    ) {
        let mut manifest = CargoTomlManifest::parse(toml).unwrap();
        let planned = build_updates(updates);
        let result = manifest.apply_updates(&planned);
        if should_succeed {
            let output = result.unwrap();
            for s in expected_contains {
                assert!(
                    output.contains(s),
                    "expected output to contain {s:?}, got:\n{output}"
                );
            }
        } else {
            assert!(result.is_err());
        }
    }
}
