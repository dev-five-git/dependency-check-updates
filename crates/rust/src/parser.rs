//! Cargo.toml parsing and format-preserving dependency updates via `toml_edit`.

use std::path::Path;

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
    /// The `toml_edit` document (format-preserving).
    pub doc: DocumentMut,
    /// Collected dependencies.
    pub dependencies: Vec<DependencySpec>,
}

impl CargoTomlManifest {
    /// Parse a Cargo.toml from raw text, without resolving local path
    /// dependencies (used by the patch path, which only needs the document).
    ///
    /// # Errors
    ///
    /// Returns an error if the text is not valid TOML.
    pub fn parse(text: &str) -> Result<Self, CargoTomlError> {
        Self::parse_in_dir(text, None)
    }

    /// Parse a Cargo.toml from raw text, resolving local path dependencies
    /// relative to `manifest_dir` when provided.
    ///
    /// A dependency such as `dep = { path = "../dep", version = "0.2.0" }`
    /// has its target version read from `../dep/Cargo.toml` (the crate on
    /// disk) instead of crates.io, so the declared `version` can be synced to
    /// the local crate. When `manifest_dir` is `None`, path dependencies are
    /// skipped entirely (never resolved against the registry).
    ///
    /// # Errors
    ///
    /// Returns an error if the text is not valid TOML.
    pub fn parse_in_dir(text: &str, manifest_dir: Option<&Path>) -> Result<Self, CargoTomlError> {
        let doc: DocumentMut = text
            .parse()
            .map_err(|e: toml_edit::TomlError| CargoTomlError::ParseFailed(e.to_string()))?;

        let dependencies = Self::collect_dependencies(&doc, manifest_dir);

        Ok(Self { doc, dependencies })
    }

    fn collect_dependencies(doc: &DocumentMut, manifest_dir: Option<&Path>) -> Vec<DependencySpec> {
        let mut deps = Vec::new();

        for &(section, key) in CARGO_SECTIONS {
            if let Some(table) = doc.get(key).and_then(Item::as_table) {
                Self::collect_from_table(table, section, manifest_dir, &mut deps);
            }
        }

        // Also check [workspace.dependencies]
        if let Some(ws) = doc.get("workspace").and_then(Item::as_table) {
            if let Some(ws_deps) = ws.get("dependencies").and_then(Item::as_table) {
                Self::collect_from_table(
                    ws_deps,
                    DependencySection::WorkspaceDependencies,
                    manifest_dir,
                    &mut deps,
                );
            }
        }

        deps
    }

    fn collect_from_table(
        table: &Table,
        section: DependencySection,
        manifest_dir: Option<&Path>,
        deps: &mut Vec<DependencySpec>,
    ) {
        for (name, item) in table {
            match classify_dependency(item, manifest_dir) {
                Some(DepKind::Registry(version)) => {
                    // Skip wildcard-only requirements like `*` which already
                    // mean "any version" — updating them would be a no-op.
                    if !version.is_empty() && version.trim() != "*" {
                        deps.push(DependencySpec {
                            name: name.to_owned(),
                            current_req: version,
                            section,
                            path_version: None,
                        });
                    }
                }
                Some(DepKind::Path {
                    current_req,
                    local_version,
                }) => {
                    deps.push(DependencySpec {
                        name: name.to_owned(),
                        current_req,
                        section,
                        path_version: Some(local_version),
                    });
                }
                None => {}
            }
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
                dependency_check_updates_core::replace_string_preserving_decor(
                    s,
                    new_version.to_owned(),
                );
            }
            Item::Value(Value::InlineTable(t)) => {
                if let Some(v) = t.get_mut("version") {
                    if let Value::String(s) = v {
                        dependency_check_updates_core::replace_string_preserving_decor(
                            s,
                            new_version.to_owned(),
                        );
                    } else {
                        *v = Value::String(toml_edit::Formatted::new(new_version.to_owned()));
                    }
                }
            }
            Item::Table(t) => {
                if let Some(v) = t.get_mut("version") {
                    if let Item::Value(Value::String(s)) = v {
                        dependency_check_updates_core::replace_string_preserving_decor(
                            s,
                            new_version.to_owned(),
                        );
                    } else {
                        *v = toml_edit::value(new_version);
                    }
                } else {
                    t["version"] = toml_edit::value(new_version);
                }
            }
            _ => {}
        }

        Ok(())
    }
}

/// How a single dependency entry should be resolved.
enum DepKind {
    /// Ordinary dependency resolved against crates.io. Carries the current
    /// version requirement string.
    Registry(String),
    /// Local `path` dependency that also declares a `version`. The `version`
    /// field is synced to `local_version` (the version of the crate on disk).
    Path {
        current_req: String,
        local_version: String,
    },
}

/// The relevant fields of a table-form dependency, normalised across the
/// inline (`{ ... }`) and full-table (`[deps.x]`) representations.
struct DepFields<'a> {
    workspace: bool,
    git: bool,
    path: Option<&'a str>,
    version: Option<&'a str>,
}

impl<'a> DepFields<'a> {
    fn from_item(item: &'a Item) -> Option<Self> {
        match item {
            Item::Value(Value::InlineTable(t)) => Some(Self {
                workspace: t.get("workspace").and_then(Value::as_bool).unwrap_or(false),
                git: t.get("git").is_some(),
                path: t.get("path").and_then(Value::as_str),
                version: t.get("version").and_then(Value::as_str),
            }),
            Item::Table(t) => Some(Self {
                workspace: t.get("workspace").and_then(Item::as_bool).unwrap_or(false),
                git: t.get("git").is_some(),
                path: t.get("path").and_then(Item::as_str),
                version: t.get("version").and_then(Item::as_str),
            }),
            _ => None,
        }
    }
}

/// Classify a dependency entry into how its version should be resolved.
///
/// Handles:
/// - `dep = "1.0"` (string form) → registry
/// - `dep = { version = "1.0", features = [...] }` → registry
/// - `dep = { path = "../dep", version = "0.2.0" }` → path (synced to the
///   local crate's version, resolved relative to `manifest_dir`)
/// - `dep = { workspace = true }` → skipped (resolved from `[workspace.dependencies]`)
/// - `dep = { git = "..." }` / `dep = { path = "../dep" }` → skipped (no version)
///
/// A path dependency is **never** resolved against crates.io: if its local
/// version cannot be determined (no `version` key, no `manifest_dir`, or the
/// crate on disk is unreadable) the entry is skipped entirely.
fn classify_dependency(item: &Item, manifest_dir: Option<&Path>) -> Option<DepKind> {
    if let Item::Value(Value::String(s)) = item {
        return Some(DepKind::Registry(s.value().to_owned()));
    }

    let fields = DepFields::from_item(item)?;
    if fields.workspace {
        return None;
    }
    if fields.git {
        return None;
    }

    if let Some(path) = fields.path {
        let version = fields.version?;
        let dir = manifest_dir?;
        let local_version = resolve_path_dep_version(dir, path)?;
        return Some(DepKind::Path {
            current_req: version.to_owned(),
            local_version,
        });
    }

    Some(DepKind::Registry(fields.version?.to_owned()))
}

/// A `[package].version` value: either a literal string or inherited from the
/// workspace (`version.workspace = true`).
enum PackageVersion {
    Literal(String),
    Inherited,
}

/// Resolve the version of a local path dependency from its own `Cargo.toml`.
///
/// `manifest_dir` is the directory of the manifest that declares the
/// dependency; `dep_path` is the dependency's `path` value (relative or
/// absolute). Returns `None` when the crate cannot be read or has no
/// resolvable `[package].version`.
fn resolve_path_dep_version(manifest_dir: &Path, dep_path: &str) -> Option<String> {
    let crate_dir = manifest_dir.join(dep_path);
    let cargo_path = crate_dir.join("Cargo.toml");
    let text = std::fs::read_to_string(&cargo_path).ok()?;
    let doc: DocumentMut = text.parse().ok()?;

    match package_version(&doc)? {
        PackageVersion::Literal(v) => Some(v),
        PackageVersion::Inherited => resolve_workspace_version(&crate_dir),
    }
}

/// Read `[package].version` from a parsed `Cargo.toml`.
fn package_version(doc: &DocumentMut) -> Option<PackageVersion> {
    let version = doc
        .get("package")
        .and_then(Item::as_table)?
        .get("version")?;

    if let Some(s) = version.as_str() {
        return Some(PackageVersion::Literal(s.to_owned()));
    }
    if is_workspace_inherited(version) {
        return Some(PackageVersion::Inherited);
    }
    None
}

/// Whether a `version` item is `{ workspace = true }` / `version.workspace = true`.
fn is_workspace_inherited(item: &Item) -> bool {
    let workspace = match item {
        Item::Value(Value::InlineTable(t)) => t.get("workspace").and_then(Value::as_bool),
        Item::Table(t) => t.get("workspace").and_then(Item::as_bool),
        _ => None,
    };
    workspace.unwrap_or(false)
}

/// Walk up from `crate_dir` to find the nearest workspace root and read its
/// `[workspace.package].version` (the value a crate inherits via
/// `version.workspace = true`).
fn resolve_workspace_version(crate_dir: &Path) -> Option<String> {
    let mut dir = std::fs::canonicalize(crate_dir).ok()?;
    loop {
        let cargo_path = dir.join("Cargo.toml");
        if let Ok(text) = std::fs::read_to_string(&cargo_path) {
            if let Ok(doc) = text.parse::<DocumentMut>() {
                if let Some(version) = doc
                    .get("workspace")
                    .and_then(Item::as_table)
                    .and_then(|w| w.get("package"))
                    .and_then(Item::as_table)
                    .and_then(|p| p.get("version"))
                    .and_then(Item::as_str)
                {
                    return Some(version.to_owned());
                }
            }
        }
        if !dir.pop() {
            return None;
        }
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
    #[case::skip_git_deps_with_registry_fallback(
        r#"
[dependencies]
local = { git = "https://github.com/user/repo", version = "1.0" }
tokio = "1.0"
"#,
        &[("tokio", "1.0", DependencySection::Dependencies)]
    )]
    #[case::skip_git_deps_with_registry_fallback_full_table(
        r#"
[dependencies.local]
git = "https://github.com/user/repo"
version = "1.0"

[dependencies]
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
    #[case::inline_table_non_string_version_replaced(
        // Inline-table `version = 1` (integer, not a string) hits the `else`
        // branch that force-wraps the new version in a fresh `Value::String`;
        // without it, updating a non-string version would leave the manifest
        // with a numeric (or otherwise malformed) version value.
        r"
[dependencies]
dep = { version = 1 }
",
        &[("dep", DependencySection::Dependencies, "2.0")],
        true,
        &["\"2.0\""]
    )]
    #[case::full_table_non_string_version_replaced(
        // Full-table `[dependencies.dep]` with `version = 1` (integer) hits
        // the analogous `else` branch on the `Item::Table` side; without it
        // the same numeric-version bug would exist for the full-table form.
        r"
[dependencies.dep]
version = 1
",
        &[("dep", DependencySection::Dependencies, "2.0")],
        true,
        &["\"2.0\""]
    )]
    #[case::full_table_missing_version_key_inserted(
        // `[dependencies.dep]` with no `version` key at all exercises the
        // branch that inserts a brand-new `version` entry; without it, a
        // versionless full-table dependency could never be updated at all.
        r"
[dependencies.dep]
features = []
",
        &[("dep", DependencySection::Dependencies, "2.0")],
        true,
        &["\"2.0\""]
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

    // ----- Local path dependency resolution -------------------------------

    use tempfile::TempDir;

    /// Write `content` to `<dir>/<rel>`, creating parent directories.
    fn write_file(dir: &Path, rel: &str, content: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    /// `{ path = "../hwp", version = "0.2.0" }` resolves `path_version` from the
    /// local crate's literal `[package].version` (here a higher 0.3.0), keeping
    /// `current_req` as the manifest's declared version.
    #[test]
    fn path_dep_syncs_to_local_literal_version() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "hwp/Cargo.toml",
            "[package]\nname = \"hwp\"\nversion = \"0.3.0\"\n",
        );
        let app_dir = tmp.path().join("app");
        std::fs::create_dir_all(&app_dir).unwrap();

        let manifest = "[dependencies]\nhwp = { path = \"../hwp\", version = \"0.2.0\" }\n";
        let parsed = CargoTomlManifest::parse_in_dir(manifest, Some(&app_dir)).unwrap();

        assert_eq!(parsed.dependencies.len(), 1);
        let dep = &parsed.dependencies[0];
        assert_eq!(dep.name, "hwp");
        assert_eq!(dep.current_req, "0.2.0");
        assert_eq!(dep.path_version.as_deref(), Some("0.3.0"));
    }

    /// A path dep whose local crate is *older* than the declared version still
    /// resolves — exact-sync (and its allowed downgrade) is decided later in the
    /// pipeline; the parser just reports the on-disk version.
    #[test]
    fn path_dep_reports_lower_local_version() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "hwp/Cargo.toml",
            "[package]\nname = \"hwp\"\nversion = \"0.1.0\"\n",
        );
        let app_dir = tmp.path().join("app");
        std::fs::create_dir_all(&app_dir).unwrap();

        let manifest = "[dependencies]\nhwp = { path = \"../hwp\", version = \"0.2.0\" }\n";
        let parsed = CargoTomlManifest::parse_in_dir(manifest, Some(&app_dir)).unwrap();

        assert_eq!(
            parsed.dependencies[0].path_version.as_deref(),
            Some("0.1.0")
        );
    }

    /// `version.workspace = true` in the local crate resolves by walking up to
    /// the workspace root's `[workspace.package].version`.
    #[test]
    fn path_dep_resolves_workspace_inherited_version() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/hwp\", \"crates/app\"]\n\n[workspace.package]\nversion = \"1.2.3\"\n",
        );
        write_file(
            tmp.path(),
            "crates/hwp/Cargo.toml",
            "[package]\nname = \"hwp\"\nversion.workspace = true\n",
        );
        let app_dir = tmp.path().join("crates/app");
        std::fs::create_dir_all(&app_dir).unwrap();

        let manifest = "[dependencies]\nhwp = { path = \"../hwp\", version = \"1.0.0\" }\n";
        let parsed = CargoTomlManifest::parse_in_dir(manifest, Some(&app_dir)).unwrap();

        assert_eq!(
            parsed.dependencies[0].path_version.as_deref(),
            Some("1.2.3")
        );
    }

    /// The full-table form `[dependencies.hwp]` with `path` + `version` is
    /// resolved the same way as the inline form.
    #[test]
    fn path_dep_full_table_form_resolved() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "hwp/Cargo.toml",
            "[package]\nname = \"hwp\"\nversion = \"0.5.0\"\n",
        );
        let app_dir = tmp.path().join("app");
        std::fs::create_dir_all(&app_dir).unwrap();

        let manifest = "[dependencies.hwp]\npath = \"../hwp\"\nversion = \"0.2.0\"\n";
        let parsed = CargoTomlManifest::parse_in_dir(manifest, Some(&app_dir)).unwrap();

        assert_eq!(parsed.dependencies.len(), 1);
        assert_eq!(
            parsed.dependencies[0].path_version.as_deref(),
            Some("0.5.0")
        );
    }

    /// `[workspace.dependencies]` path deps (the publish-ready monorepo pattern)
    /// are resolved against the local crate too.
    #[test]
    fn workspace_dependency_path_resolved() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "crates/core/Cargo.toml",
            "[package]\nname = \"core\"\nversion = \"0.1.20\"\n",
        );

        let manifest =
            "[workspace.dependencies]\ncore = { path = \"crates/core\", version = \"0.1.15\" }\n";
        let parsed = CargoTomlManifest::parse_in_dir(manifest, Some(tmp.path())).unwrap();

        assert_eq!(parsed.dependencies.len(), 1);
        assert_eq!(
            parsed.dependencies[0].section,
            DependencySection::WorkspaceDependencies
        );
        assert_eq!(
            parsed.dependencies[0].path_version.as_deref(),
            Some("0.1.20")
        );
    }

    /// A path dep whose local crate cannot be found is skipped entirely — it is
    /// never resolved against crates.io. Sibling registry deps are unaffected.
    #[test]
    fn path_dep_missing_crate_is_skipped() {
        let tmp = TempDir::new().unwrap();
        let app_dir = tmp.path().join("app");
        std::fs::create_dir_all(&app_dir).unwrap();

        let manifest =
            "[dependencies]\nhwp = { path = \"../hwp\", version = \"0.2.0\" }\ntokio = \"1.0\"\n";
        let parsed = CargoTomlManifest::parse_in_dir(manifest, Some(&app_dir)).unwrap();

        assert_eq!(parsed.dependencies.len(), 1);
        assert_eq!(parsed.dependencies[0].name, "tokio");
    }

    /// Without a manifest directory (e.g. the patch path) path deps are skipped
    /// rather than resolved against the registry.
    #[test]
    fn path_dep_without_manifest_dir_is_skipped() {
        let manifest =
            "[dependencies]\nhwp = { path = \"../hwp\", version = \"0.2.0\" }\ntokio = \"1.0\"\n";
        let parsed = CargoTomlManifest::parse(manifest).unwrap();

        assert_eq!(parsed.dependencies.len(), 1);
        assert_eq!(parsed.dependencies[0].name, "tokio");
    }

    /// A `path` dep with no `version` key has nothing to sync and is skipped,
    /// even when the local crate exists.
    #[test]
    fn path_dep_without_version_key_is_skipped() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "hwp/Cargo.toml",
            "[package]\nname = \"hwp\"\nversion = \"0.3.0\"\n",
        );
        let app_dir = tmp.path().join("app");
        std::fs::create_dir_all(&app_dir).unwrap();

        let manifest = "[dependencies]\nhwp = { path = \"../hwp\" }\ntokio = \"1.0\"\n";
        let parsed = CargoTomlManifest::parse_in_dir(manifest, Some(&app_dir)).unwrap();

        assert_eq!(parsed.dependencies.len(), 1);
        assert_eq!(parsed.dependencies[0].name, "tokio");
    }

    /// Applying an update to an inline path dep replaces only `version`, leaving
    /// the `path` key intact (format-preserving).
    #[test]
    fn apply_update_preserves_path_key() {
        let toml = "[dependencies]\nhwp = { path = \"../hwp\", version = \"0.2.0\" }\n";
        let mut manifest = CargoTomlManifest::parse(toml).unwrap();
        let updates = vec![PlannedUpdate {
            name: "hwp".to_owned(),
            section: DependencySection::Dependencies,
            from: "0.2.0".to_owned(),
            to: "0.3.0".to_owned(),
        }];
        let out = manifest.apply_updates(&updates).unwrap();
        assert!(
            out.contains("path = \"../hwp\""),
            "path key dropped:\n{out}"
        );
        assert!(
            out.contains("version = \"0.3.0\""),
            "version not synced:\n{out}"
        );
    }

    /// Inline-table form: the `version` value's surrounding decor (the space
    /// after `=`) must survive an update, so the inline table stays
    /// byte-for-byte identical except for the bumped version. Without decor
    /// preservation the value collapses to `version ="1.0.228"`.
    #[test]
    fn apply_updates_inline_table_preserves_decor_byte_for_byte() {
        let toml = "[dependencies]\nserde = { version = \"1.0\", features = [\"derive\"] }\n";
        let mut manifest = CargoTomlManifest::parse(toml).unwrap();
        let updates = vec![PlannedUpdate {
            name: "serde".to_owned(),
            section: DependencySection::Dependencies,
            from: "1.0".to_owned(),
            to: "1.0.228".to_owned(),
        }];
        let out = manifest.apply_updates(&updates).unwrap();
        let expected =
            "[dependencies]\nserde = { version = \"1.0.228\", features = [\"derive\"] }\n";
        assert_eq!(out, expected);
    }

    /// Full-table form: `[dependencies.serde]\nversion = "1.0"\n…` must update
    /// the value while preserving leading/trailing decor on the `version`
    /// line, so the file stays byte-identical except for the bumped value.
    #[test]
    fn apply_updates_full_table_preserves_decor_byte_for_byte() {
        let toml = "[dependencies.serde]\nversion = \"1.0\"\nfeatures = [\"derive\"]\n";
        let mut manifest = CargoTomlManifest::parse(toml).unwrap();
        let updates = vec![PlannedUpdate {
            name: "serde".to_owned(),
            section: DependencySection::Dependencies,
            from: "1.0".to_owned(),
            to: "1.0.228".to_owned(),
        }];
        let out = manifest.apply_updates(&updates).unwrap();
        let expected = "[dependencies.serde]\nversion = \"1.0.228\"\nfeatures = [\"derive\"]\n";
        assert_eq!(out, expected);
    }

    // ----- package_version / is_workspace_inherited / resolve_workspace_version -----

    /// A `[package].version` that is neither a plain string nor
    /// `{ workspace = true }` (e.g. an integer) must resolve to `None` — the
    /// path-dependency resolver then treats the crate as having no usable
    /// version rather than panicking or silently coercing the value.
    #[test]
    fn package_version_returns_none_for_non_string_non_workspace() {
        let doc: DocumentMut = "[package]\nversion = 1\n".parse().unwrap();
        assert!(package_version(&doc).is_none());
    }

    /// `version = { workspace = true }` (inline-table form) must be
    /// recognised as workspace-inherited via the inline-table arm; without
    /// it, a version declared this way would be misread as a literal (or
    /// simply ignored), breaking workspace-inherited path deps that use the
    /// inline syntax instead of `version.workspace = true`.
    #[test]
    fn is_workspace_inherited_true_for_inline_table_form() {
        let doc: DocumentMut = "[package]\nversion = { workspace = true }\n"
            .parse()
            .unwrap();
        let version_item = doc
            .get("package")
            .unwrap()
            .as_table()
            .unwrap()
            .get("version")
            .unwrap();
        assert!(is_workspace_inherited(version_item));
    }

    /// A path dependency with `version.workspace = true` but no workspace
    /// root anywhere above it must resolve to `None` once the walk exhausts
    /// every ancestor directory up to the filesystem root, instead of
    /// looping forever or panicking when `Path::pop` finally fails.
    ///
    /// A `TempDir` lives under the OS temp directory, which — unlike this
    /// repo's own crates — has no ancestor `Cargo.toml` at all, so the walk
    /// is guaranteed to bottom out without finding a workspace.
    #[test]
    fn resolve_workspace_version_none_when_no_workspace_root_found() {
        let tmp = TempDir::new().unwrap();

        assert!(
            resolve_workspace_version(tmp.path()).is_none(),
            "expected no workspace root above a bare temp directory"
        );
    }
}
