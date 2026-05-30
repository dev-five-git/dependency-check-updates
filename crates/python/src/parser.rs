//! pyproject.toml parsing and format-preserving dependency updates via `toml_edit`.
//!
//! Supports:
//! - `[project] dependencies` (PEP 621)
//! - `[tool.poetry.dependencies]` (Poetry)
//! - `[dependency-groups]` (PEP 735)

use dependency_check_updates_core::{DependencySection, DependencySpec, PlannedUpdate};
use toml_edit::{DocumentMut, Item};

/// A parsed pyproject.toml file.
#[derive(Debug)]
pub struct PyProjectManifest {
    /// The original raw text.
    pub original_text: String,
    /// The `toml_edit` document (format-preserving).
    pub doc: DocumentMut,
    /// Collected dependencies.
    pub dependencies: Vec<DependencySpec>,
}

impl PyProjectManifest {
    /// Parse a pyproject.toml from raw text.
    ///
    /// # Errors
    ///
    /// Returns an error if the text is not valid TOML.
    pub fn parse(text: &str) -> Result<Self, PyProjectError> {
        let doc: DocumentMut = text
            .parse()
            .map_err(|e: toml_edit::TomlError| PyProjectError::ParseFailed(e.to_string()))?;

        let dependencies = Self::collect_dependencies(&doc);

        Ok(Self {
            original_text: text.to_owned(),
            doc,
            dependencies,
        })
    }

    fn collect_dependencies(doc: &DocumentMut) -> Vec<DependencySpec> {
        let mut deps = Vec::new();

        // PEP 621: [project] dependencies = ["requests>=2.0", ...]
        if let Some(project) = doc.get("project").and_then(Item::as_table) {
            if let Some(dep_array) = project.get("dependencies").and_then(Item::as_array) {
                for item in dep_array {
                    if let Some(spec_str) = item.as_str() {
                        if let Some(dep) =
                            parse_pep508_spec(spec_str, DependencySection::ProjectDependencies)
                        {
                            deps.push(dep);
                        }
                    }
                }
            }

            // [project.optional-dependencies]
            if let Some(opt_deps) = project
                .get("optional-dependencies")
                .and_then(Item::as_table)
            {
                for (_group, items) in opt_deps {
                    if let Some(arr) = items.as_array() {
                        for item in arr {
                            if let Some(spec_str) = item.as_str() {
                                if let Some(dep) = parse_pep508_spec(
                                    spec_str,
                                    DependencySection::OptionalDependencies,
                                ) {
                                    deps.push(dep);
                                }
                            }
                        }
                    }
                }
            }
        }

        // Poetry: [tool.poetry.dependencies]
        if let Some(tool) = doc.get("tool").and_then(Item::as_table) {
            if let Some(poetry) = tool.get("poetry").and_then(Item::as_table) {
                if let Some(poetry_deps) = poetry.get("dependencies").and_then(Item::as_table) {
                    for (name, item) in poetry_deps {
                        if name == "python" {
                            continue; // Skip python version constraint
                        }
                        if let Some(version) = extract_poetry_version(item) {
                            if !is_wildcard_req(&version) {
                                deps.push(DependencySpec {
                                    name: name.to_owned(),
                                    current_req: version,
                                    section: DependencySection::Dependencies,
                                });
                            }
                        }
                    }
                }
                // Poetry dev-dependencies
                if let Some(dev_deps) = poetry.get("dev-dependencies").and_then(Item::as_table) {
                    for (name, item) in dev_deps {
                        if let Some(version) = extract_poetry_version(item) {
                            if !is_wildcard_req(&version) {
                                deps.push(DependencySpec {
                                    name: name.to_owned(),
                                    current_req: version,
                                    section: DependencySection::DevDependencies,
                                });
                            }
                        }
                    }
                }
            }
        }

        // PEP 735: [dependency-groups]
        if let Some(groups) = doc.get("dependency-groups").and_then(Item::as_table) {
            for (_group_name, items) in groups {
                if let Some(arr) = items.as_array() {
                    for item in arr {
                        if let Some(spec_str) = item.as_str() {
                            if let Some(dep) =
                                parse_pep508_spec(spec_str, DependencySection::DevDependencies)
                            {
                                deps.push(dep);
                            }
                        }
                    }
                }
            }
        }

        deps
    }

    /// Apply planned updates to the document, returning the modified text.
    ///
    /// Infallible: updates that match no dependency in the document are
    /// silently skipped (see [`Self::apply_single_update`]).
    #[must_use]
    pub fn apply_updates(&mut self, updates: &[PlannedUpdate]) -> String {
        for update in updates {
            self.apply_single_update(update);
        }
        self.doc.to_string()
    }

    fn apply_single_update(&mut self, update: &PlannedUpdate) {
        // Try PEP 621 project.dependencies
        if let Some(project) = self.doc.get_mut("project").and_then(Item::as_table_mut) {
            if let Some(dep_array) = project.get_mut("dependencies").and_then(Item::as_array_mut) {
                for item in dep_array.iter_mut() {
                    let Some(spec_str) = item.as_str() else {
                        continue;
                    };
                    if !spec_str_matches_name(spec_str, &update.name) {
                        continue;
                    }
                    let new_spec = replace_version_in_pep508(spec_str, &update.to);
                    // Preserve the element's surrounding decor (leading newline +
                    // indentation, trailing whitespace/comment) instead of
                    // replacing the value wholesale — a fresh `Formatted::new`
                    // carries empty decor, which collapses a multi-line
                    // `dependencies` array onto a single line. Mirrors the
                    // decor-preserving Poetry path below.
                    if let toml_edit::Value::String(s) = item {
                        let mut new_s = toml_edit::Formatted::new(new_spec);
                        *new_s.decor_mut() = s.decor().clone();
                        *s = new_s;
                    }
                    return;
                }
            }
        }

        // Try Poetry tool.poetry.dependencies
        if let Some(tool) = self.doc.get_mut("tool").and_then(Item::as_table_mut) {
            if let Some(poetry) = tool.get_mut("poetry").and_then(Item::as_table_mut) {
                if let Some(deps) = poetry.get_mut("dependencies").and_then(Item::as_table_mut) {
                    if let Some(Item::Value(toml_edit::Value::String(s))) =
                        deps.get_mut(&update.name)
                    {
                        let decor = s.decor().clone();
                        let mut new_s = toml_edit::Formatted::new(update.to.clone());
                        *new_s.decor_mut() = decor;
                        *s = new_s;
                    }
                }
            }
        }

        // Silently skip if not found (may be in optional-deps or groups)
    }
}

/// Parse a PEP 508 dependency spec like `"requests>=2.28.0"` or `"flask~=2.0"`.
///
/// Returns `None` for specs without version constraints (e.g., bare `"requests"`).
fn parse_pep508_spec(spec: &str, section: DependencySection) -> Option<DependencySpec> {
    let spec = spec.trim();

    // Find where the version constraint starts (first non-alphanumeric, non-hyphen, non-dot, non-underscore)
    let name_end = spec
        .find(|c: char| !c.is_alphanumeric() && c != '-' && c != '_' && c != '.')
        .unwrap_or(spec.len());

    let name = spec[..name_end].trim();
    if name.is_empty() {
        return None;
    }

    let rest = spec[name_end..].trim();

    // Remove extras like [security] before version
    let rest = if rest.starts_with('[') {
        rest.find(']').map_or(rest, |i| rest[i + 1..].trim())
    } else {
        rest
    };

    // Remove environment markers like ; python_version >= "3.8"
    let rest = rest.split(';').next().unwrap_or("").trim();

    if rest.is_empty() {
        return None; // No version constraint
    }

    if is_wildcard_req(rest) {
        return None; // `*`, `==*`, etc. already mean "any version"
    }

    Some(DependencySpec {
        name: name.to_owned(),
        current_req: rest.to_owned(),
        section,
    })
}

/// Check if a PEP 508 spec string matches a given package name.
fn spec_str_matches_name(spec: &str, name: &str) -> bool {
    let spec = spec.trim();
    let name_end = spec
        .find(|c: char| !c.is_alphanumeric() && c != '-' && c != '_' && c != '.')
        .unwrap_or(spec.len());
    let spec_name = &spec[..name_end];

    // PEP 503 normalized comparison (case-insensitive, treat - _ . as equivalent)
    normalize_pep503(spec_name) == normalize_pep503(name)
}

fn normalize_pep503(name: &str) -> String {
    name.to_lowercase().replace(['-', '.'], "_")
}

/// Replace the version constraint in a PEP 508 spec string.
fn replace_version_in_pep508(spec: &str, new_version: &str) -> String {
    let spec = spec.trim();
    let name_end = spec
        .find(|c: char| !c.is_alphanumeric() && c != '-' && c != '_' && c != '.')
        .unwrap_or(spec.len());

    let name = &spec[..name_end];

    // Check for extras
    let rest = &spec[name_end..];
    let (extras, rest) = if rest.starts_with('[') {
        rest.find(']')
            .map_or(("", rest), |i| (&rest[..=i], rest[i + 1..].trim_start()))
    } else {
        ("", rest.trim_start())
    };

    // Check for environment markers
    let marker = rest.find(';').map_or("", |i| &rest[i..]);

    format!("{name}{extras}{new_version}{marker}")
}

/// Check if a version requirement is an unresolvable wildcard like `*` or `==*`.
///
/// Such requirements already mean "any/latest version", so updating them
/// would be a meaningless no-op and we filter them out at parse time.
fn is_wildcard_req(req: &str) -> bool {
    let stripped = req
        .trim()
        .trim_start_matches(['=', '~', '^', '>', '<'])
        .trim();
    matches!(stripped, "" | "*")
}

/// Extract a version string from a Poetry dependency value.
fn extract_poetry_version(item: &Item) -> Option<String> {
    match item {
        Item::Value(toml_edit::Value::String(s)) => Some(s.value().to_owned()),
        Item::Value(toml_edit::Value::InlineTable(t)) => t
            .get("version")
            .and_then(toml_edit::Value::as_str)
            .map(String::from),
        Item::Table(t) => t.get("version").and_then(Item::as_str).map(String::from),
        _ => None,
    }
}

/// Errors from pyproject.toml operations.
#[derive(Debug, thiserror::Error)]
pub enum PyProjectError {
    #[error("failed to parse pyproject.toml: {0}")]
    ParseFailed(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    // ---------- Pure-function table tests ----------

    #[rstest]
    #[case::basic("requests>=2.28.0", ">=2.31.0", "requests>=2.31.0")]
    #[case::tilde("flask~=2.0", "~=3.0", "flask~=3.0")]
    #[case::with_markers(
        "pywin32>=300; sys_platform == 'win32'",
        ">=306",
        "pywin32>=306; sys_platform == 'win32'"
    )]
    #[case::extras_and_markers(
        "requests[security]>=2.28.0; python_version >= '3.8'",
        ">=2.31.0",
        "requests[security]>=2.31.0; python_version >= '3.8'"
    )]
    fn replace_version_in_pep508_cases(
        #[case] spec: &str,
        #[case] new_version: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(replace_version_in_pep508(spec, new_version), expected);
    }

    #[rstest]
    #[case::dash_to_underscore("My-Package", "my_package")]
    #[case::dot_to_underscore("my.package", "my_package")]
    #[case::lowercase("MY_PACKAGE", "my_package")]
    fn normalize_pep503_cases(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(normalize_pep503(input), expected);
    }

    #[rstest]
    #[case::dash_underscore_equivalence("My-Package>=1.0", "my_package", true)]
    #[case::dot_dash_equivalence("my.package>=1.0", "my-package", true)]
    #[case::different_name("other>=1.0", "my-package", false)]
    fn spec_str_matches_name_cases(#[case] spec: &str, #[case] name: &str, #[case] expected: bool) {
        assert_eq!(spec_str_matches_name(spec, name), expected);
    }

    #[rstest]
    #[case::with_extras("requests[security]>=2.28.0", "requests", ">=2.28.0")]
    #[case::with_markers("pywin32>=300; sys_platform == 'win32'", "pywin32", ">=300")]
    fn parse_pep508_spec_with_constraint_cases(
        #[case] spec: &str,
        #[case] expected_name: &str,
        #[case] expected_req: &str,
    ) {
        let dep =
            parse_pep508_spec(spec, DependencySection::ProjectDependencies).expect("should parse");
        assert_eq!(dep.name, expected_name);
        assert_eq!(dep.current_req, expected_req);
    }

    #[rstest]
    #[case::bare_name("requests")]
    #[case::empty_string("")]
    #[case::equals_wildcard("requests==*")]
    #[case::bare_star("requests *")]
    fn parse_pep508_spec_without_constraint_cases(#[case] spec: &str) {
        assert!(parse_pep508_spec(spec, DependencySection::ProjectDependencies).is_none());
    }

    // ---------- Manifest parse / collect_dependencies scenarios ----------

    /// Optional `(name, req, section)` triple to assert on a particular dep slot
    /// after parsing. Each field is `Option` so a case asserts only the
    /// originally-checked fields without strengthening the test.
    type FieldsCheck = (
        Option<&'static str>,
        Option<&'static str>,
        Option<DependencySection>,
    );

    #[rstest]
    #[case::pep621_dependencies(
        "\n[project]\nname = \"my-project\"\ndependencies = [\n    \"requests>=2.28.0\",\n    \"flask~=2.0\",\n    \"click>=8.0,<9.0\",\n]\n",
        3,
        Some((
            Some("requests"),
            Some(">=2.28.0"),
            Some(DependencySection::ProjectDependencies),
        )),
    )]
    #[case::pep621_optional_deps(
        "\n[project.optional-dependencies]\ndev = [\"pytest>=7.0\", \"black>=23.0\"]\ndocs = [\"sphinx>=5.0\"]\n",
        3,
        Some((None, None, Some(DependencySection::OptionalDependencies))),
    )]
    #[case::poetry_dependencies(
        "\n[tool.poetry.dependencies]\npython = \"^3.8\"\nrequests = \"^2.28.0\"\nflask = {version = \"^2.0\", optional = true}\n",
        2,
        Some((Some("requests"), Some("^2.28.0"), None)),
    )]
    #[case::poetry_dev_dependencies(
        "\n[tool.poetry.dev-dependencies]\npytest = \"^7.0\"\n",
        1,
        Some((None, None, Some(DependencySection::DevDependencies))),
    )]
    #[case::dependency_groups(
        "\n[dependency-groups]\ntest = [\"pytest>=7.0\", \"coverage>=7.0\"]\n",
        2,
        None
    )]
    #[case::skip_bare_deps(
        "\n[project]\ndependencies = [\"requests\", \"flask>=2.0\"]\n",
        1,
        Some((Some("flask"), None, None)),
    )]
    #[case::no_deps_empty("\n[project]\nname = \"empty\"\n", 0, None)]
    #[case::poetry_bool_value_skipped(
        "\n[tool.poetry.dependencies]\npython = \"^3.8\"\nmy-pkg = true\n",
        0,
        None
    )]
    fn collect_dependencies_cases(
        #[case] toml: &str,
        #[case] expected_len: usize,
        #[case] expected_first: Option<FieldsCheck>,
    ) {
        let manifest = PyProjectManifest::parse(toml).expect("toml should parse");
        assert_eq!(manifest.dependencies.len(), expected_len);
        if let Some((name, req, section)) = expected_first {
            let first = &manifest.dependencies[0];
            if let Some(n) = name {
                assert_eq!(first.name, n);
            }
            if let Some(r) = req {
                assert_eq!(first.current_req, r);
            }
            if let Some(s) = section {
                assert_eq!(first.section, s);
            }
        }
    }

    #[rstest]
    #[case::poetry_table_form(
        "\n[tool.poetry.dependencies]\npython = \"^3.8\"\n\n[tool.poetry.dependencies.sqlalchemy]\nversion = \"^2.0\"\nextras = [\"asyncio\"]\n",
        "sqlalchemy",
        "^2.0"
    )]
    #[case::poetry_inline_table(
        "\n[tool.poetry.dependencies]\npython = \"^3.8\"\nflask = {version = \"^2.0\", optional = true}\n",
        "flask",
        "^2.0"
    )]
    fn collect_dependencies_finds_named_dep(
        #[case] toml: &str,
        #[case] name: &str,
        #[case] expected_req: &str,
    ) {
        let manifest = PyProjectManifest::parse(toml).expect("toml should parse");
        let found = manifest.dependencies.iter().find(|d| d.name == name);
        assert!(found.is_some());
        assert_eq!(found.unwrap().current_req, expected_req);
    }

    #[test]
    fn invalid_toml_returns_error() {
        let result = PyProjectManifest::parse("not valid [[[toml");
        assert!(result.is_err());
    }

    #[test]
    fn comments_preserved_through_noop_apply() {
        let toml = r#"
# Project config
[project]
name = "my-project"
# Main dependencies
dependencies = [
    "requests>=2.28.0",
]
"#;
        let mut manifest = PyProjectManifest::parse(toml).unwrap();
        let result = manifest.apply_updates(&[]);
        assert!(result.contains("# Project config"));
        assert!(result.contains("# Main dependencies"));
    }

    // ---------- apply_updates substring-based scenarios ----------

    /// `(name, section, from, to)` for a single planned update row.
    type UpdateRow = (&'static str, DependencySection, &'static str, &'static str);

    fn rows_to_updates(rows: &[UpdateRow]) -> Vec<PlannedUpdate> {
        rows.iter()
            .map(|(name, section, from, to)| PlannedUpdate {
                name: (*name).to_owned(),
                section: *section,
                from: (*from).to_owned(),
                to: (*to).to_owned(),
            })
            .collect()
    }

    #[rstest]
    #[case::pep621_basic(
        "\n[project]\nname = \"my-project\"\ndependencies = [\n    \"requests>=2.28.0\",\n    \"flask~=2.0\",\n]\n",
        &[("requests", DependencySection::ProjectDependencies, ">=2.28.0", ">=2.31.0")],
        &["requests>=2.31.0", "flask~=2.0"],
    )]
    #[case::poetry_basic(
        "\n[tool.poetry.dependencies]\npython = \"^3.8\"\nrequests = \"^2.28.0\"\nflask = \"^2.0\"\n",
        &[("requests", DependencySection::Dependencies, "^2.28.0", "^2.31.0")],
        &["\"^2.31.0\"", "flask = \"^2.0\""],
    )]
    #[case::empty_updates(
        "\n[project]\nname = \"my-project\"\ndependencies = [\n    \"requests>=2.28.0\",\n]\n",
        &[],
        &["requests>=2.28.0"],
    )]
    #[case::pep508_with_markers(
        "\n[project]\ndependencies = [\n    \"pywin32>=300; sys_platform == 'win32'\",\n]\n",
        &[("pywin32", DependencySection::ProjectDependencies, ">=300", ">=306")],
        &["pywin32>=306; sys_platform == 'win32'"],
    )]
    #[case::poetry_table_form(
        "\n[tool.poetry.dependencies]\npython = \"^3.8\"\nrequests = \"^2.28.0\"\n",
        &[("requests", DependencySection::Dependencies, "^2.28.0", "^2.31.0")],
        &["\"^2.31.0\"", "python = \"^3.8\""],
    )]
    #[case::nonexistent_dep_skipped(
        "\n[project]\ndependencies = [\"requests>=2.28.0\"]\n",
        &[("nonexistent", DependencySection::ProjectDependencies, ">=1.0", ">=2.0")],
        &["requests>=2.28.0"],
    )]
    fn apply_updates_substring_cases(
        #[case] toml: &str,
        #[case] update_rows: &[UpdateRow],
        #[case] expected_contains: &[&str],
    ) {
        let mut manifest = PyProjectManifest::parse(toml).expect("toml should parse");
        let updates = rows_to_updates(update_rows);
        let result = manifest.apply_updates(&updates);
        for needle in expected_contains {
            assert!(
                result.contains(needle),
                "expected substring `{needle}` in result:\n{result}"
            );
        }
    }

    #[test]
    fn apply_updates_pep508_with_extras_parses_and_replaces() {
        // Separate from the substring table because the original test also
        // asserts pre-apply dependency-list state (len + name).
        let toml = r#"
[project]
dependencies = [
    "requests[security]>=2.28.0",
]
"#;
        let mut manifest = PyProjectManifest::parse(toml).unwrap();
        assert_eq!(manifest.dependencies.len(), 1);
        assert_eq!(manifest.dependencies[0].name, "requests");

        let updates = vec![PlannedUpdate {
            name: "requests".to_owned(),
            section: DependencySection::ProjectDependencies,
            from: ">=2.28.0".to_owned(),
            to: ">=2.31.0".to_owned(),
        }];
        let result = manifest.apply_updates(&updates);
        assert!(result.contains("requests[security]>=2.31.0"));
    }

    /// Covers the `let Some(spec_str) = item.as_str() else { continue; }`
    /// branch in `apply_single_update` (parser.rs line 155). The
    /// `dependencies` array contains an inline-table element (TOML-valid but
    /// not a string), which `apply_single_update` must skip via `continue`
    /// before reaching the trailing string dep, which still gets updated.
    #[test]
    fn apply_update_skips_non_string_array_element() {
        let toml = "[project]\nname = \"demo\"\ndependencies = [\n    { name = \"weird\", version = \"1.0\" },\n    \"requests>=2.28.0\",\n]\n";
        let mut manifest = PyProjectManifest::parse(toml).expect("toml should parse");
        let updates = vec![PlannedUpdate {
            name: "requests".to_owned(),
            section: DependencySection::ProjectDependencies,
            from: ">=2.28.0".to_owned(),
            to: ">=2.31.0".to_owned(),
        }];
        let result = manifest.apply_updates(&updates);
        assert!(
            result.contains("requests>=2.31.0"),
            "string dep after the inline-table element should still be updated:\n{result}"
        );
        // Inline-table element survives unchanged (the loop skipped it).
        assert!(
            result.contains("name = \"weird\""),
            "non-string element should remain in the array:\n{result}"
        );
    }

    #[test]
    fn apply_updates_pep621_preserves_multiline_format() {
        // Regression: replacing a PEP 621 array element must keep the element's
        // surrounding decor (leading newline + indentation). Previously the
        // value was swapped with a fresh `Formatted::new` carrying empty decor,
        // which collapsed the whole `dependencies` array onto one line.
        let toml = "[project]\nname = \"demo\"\ndependencies = [\n    \"pytz>=2024.1\",\n    \"requests>=2.30.0\",\n]\n";
        let mut manifest = PyProjectManifest::parse(toml).unwrap();
        let updates = vec![PlannedUpdate {
            name: "pytz".to_owned(),
            section: DependencySection::ProjectDependencies,
            from: ">=2024.1".to_owned(),
            to: ">=2026.2".to_owned(),
        }];
        let result = manifest.apply_updates(&updates);
        // Byte-for-byte identical except the bumped version — newlines and the
        // 4-space indentation of every element are preserved.
        let expected = "[project]\nname = \"demo\"\ndependencies = [\n    \"pytz>=2026.2\",\n    \"requests>=2.30.0\",\n]\n";
        assert_eq!(result, expected);
    }
}
