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

        Ok(Self { doc, dependencies })
    }

    fn collect_dependencies(doc: &DocumentMut) -> Vec<DependencySpec> {
        let mut deps = Vec::new();

        // PEP 621: [project] dependencies = ["requests>=2.0", ...]
        if let Some(project) = doc.get("project").and_then(Item::as_table) {
            if let Some(dep_array) = project.get("dependencies").and_then(Item::as_array) {
                collect_pep508_array(dep_array, DependencySection::ProjectDependencies, &mut deps);
            }

            // [project.optional-dependencies]
            if let Some(opt_deps) = project
                .get("optional-dependencies")
                .and_then(Item::as_table)
            {
                collect_pep508_array_table(
                    opt_deps,
                    DependencySection::OptionalDependencies,
                    &mut deps,
                );
            }
        }

        // Poetry: [tool.poetry.dependencies] and [tool.poetry.dev-dependencies]
        // funnel through one shared `collect_poetry_table` helper — the two
        // loops were previously byte-for-byte identical except for the
        // `DependencySection` literal, and the dev-loop's `python` skip
        // comment already mirrored the main-loop guard, signalling the
        // duplication. See 0007-analyze.md F1.
        if let Some(tool) = doc.get("tool").and_then(Item::as_table) {
            if let Some(poetry) = tool.get("poetry").and_then(Item::as_table) {
                if let Some(t) = poetry.get("dependencies").and_then(Item::as_table) {
                    collect_poetry_table(t, DependencySection::Dependencies, &mut deps);
                }
                if let Some(t) = poetry.get("dev-dependencies").and_then(Item::as_table) {
                    collect_poetry_table(t, DependencySection::DevDependencies, &mut deps);
                }
            }
        }

        // PEP 735: [dependency-groups]
        if let Some(groups) = doc.get("dependency-groups").and_then(Item::as_table) {
            collect_pep508_array_table(groups, DependencySection::DevDependencies, &mut deps);
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
        // Try PEP 621 project.dependencies (and optional-dependencies)
        if let Some(project) = self.doc.get_mut("project").and_then(Item::as_table_mut) {
            if let Some(dep_array) = project.get_mut("dependencies").and_then(Item::as_array_mut) {
                if apply_to_pep508_array(dep_array, update) {
                    return;
                }
            }
            // PEP 621: [project.optional-dependencies] — one named array per
            // extra group; the matrix in 0027-analyze.md flagged this as a
            // silent drop. Each value is a PEP 508 array, identical shape to
            // the main `dependencies` array above.
            if let Some(opt) = project
                .get_mut("optional-dependencies")
                .and_then(Item::as_table_mut)
            {
                for (_group, items) in opt.iter_mut() {
                    if let Some(arr) = items.as_array_mut() {
                        if apply_to_pep508_array(arr, update) {
                            return;
                        }
                    }
                }
            }
        }

        // PEP 735: [dependency-groups] — modern standard for dev dep groups.
        if let Some(groups) = self
            .doc
            .get_mut("dependency-groups")
            .and_then(Item::as_table_mut)
        {
            for (_group, items) in groups.iter_mut() {
                if let Some(arr) = items.as_array_mut() {
                    if apply_to_pep508_array(arr, update) {
                        return;
                    }
                }
            }
        }

        // Try Poetry tool.poetry.dependencies (and dev-dependencies). All
        // three Poetry value shapes that `extract_poetry_version` recognises
        // (string, inline-table, full-table) are handled by the shared
        // `apply_to_poetry_table` helper — the previous string-only path
        // silently dropped inline/full-table updates that `compute_updates`
        // had already planned, so `dcu -u` printed the row but left the file
        // unchanged. See 0036-analyze.md F1.
        if let Some(tool) = self.doc.get_mut("tool").and_then(Item::as_table_mut) {
            if let Some(poetry) = tool.get_mut("poetry").and_then(Item::as_table_mut) {
                if let Some(deps) = poetry.get_mut("dependencies").and_then(Item::as_table_mut) {
                    if apply_to_poetry_table(deps, &update.name, &update.to) {
                        return;
                    }
                }
                if let Some(deps) = poetry
                    .get_mut("dev-dependencies")
                    .and_then(Item::as_table_mut)
                {
                    apply_to_poetry_table(deps, &update.name, &update.to);
                }
            }
        }

        // Silently skip if the dep is truly absent from every supported
        // section. This now only fires on real no-ops, not on the three
        // sections this method previously dropped.
    }
}

/// Walk a PEP 508 array, parsing each string element via
/// [`parse_pep508_spec`] and pushing every successfully-parsed
/// [`DependencySpec`] into `deps` under the given `section`.
///
/// Mirrors the patch-side [`apply_to_pep508_array`] so the parse and patch
/// sides share the same shape: both walk the array, both skip non-string
/// elements, both delegate the per-element work to a single helper. The
/// three PEP 508 array sites in [`PyProjectManifest::collect_dependencies`]
/// (PEP 621 `[project].dependencies`, PEP 621 `[project.optional-dependencies]`
/// groups, PEP 735 `[dependency-groups]` groups) all funnel through here.
fn collect_pep508_array(
    arr: &toml_edit::Array,
    section: DependencySection,
    deps: &mut Vec<DependencySpec>,
) {
    for item in arr {
        if let Some(spec_str) = item.as_str() {
            if let Some(dep) = parse_pep508_spec(spec_str, section) {
                deps.push(dep);
            }
        }
    }
}

/// Iterate a table of PEP 508 arrays (e.g., `[project.optional-dependencies]`
/// or `[dependency-groups]`), calling `collect_pep508_array` for each array value.
fn collect_pep508_array_table(
    table: &toml_edit::Table,
    section: DependencySection,
    deps: &mut Vec<DependencySpec>,
) {
    for (_group, items) in table {
        if let Some(arr) = items.as_array() {
            collect_pep508_array(arr, section, deps);
        }
    }
}

/// Walk a Poetry dependency table (`[tool.poetry.dependencies]` or
/// `[tool.poetry.dev-dependencies]`), pushing every collected
/// [`DependencySpec`] into `deps` under the given `section`.
///
/// Funnels the two previously-duplicated inner loops in
/// [`PyProjectManifest::collect_dependencies`] through one shared body so
/// the `python = "^…"` interpreter guard, the
/// [`extract_poetry_version`] extraction, the [`is_wildcard_req`] skip, and
/// the [`DependencySpec`] shape (including `path_version: None`) all live in
/// exactly one place. The `section` parameter is the only piece that
/// differed between the main- and dev-dep loops, mirroring the analyze
/// report's `collect_pep508_array` parallel.
fn collect_poetry_table(
    table: &toml_edit::Table,
    section: DependencySection,
    deps: &mut Vec<DependencySpec>,
) {
    for (name, item) in table {
        // `python` here is the interpreter version constraint Poetry tracks,
        // not a PyPI package; both the main- and dev-dep loops have always
        // skipped it (see 0004-analyze.md for the dev-loop addition).
        if name == "python" {
            continue;
        }
        let Some(version) = extract_poetry_version(item) else {
            continue;
        };
        if is_wildcard_req(&version) {
            continue;
        }
        deps.push(DependencySpec {
            name: name.to_owned(),
            current_req: version,
            section,
            path_version: None,
        });
    }
}

/// Walk a PEP 508 array; on the first element whose name matches
/// `update.name`, rewrite its version constraint via
/// [`replace_version_in_pep508`], preserving the element's decor exactly.
///
/// Returns `true` if a match was found (caller should stop searching).
///
/// Faithful extraction of the existing PEP 621 main-array inner loop — same
/// matching predicate, same decor preservation, no semantic drift.
fn apply_to_pep508_array(arr: &mut toml_edit::Array, update: &PlannedUpdate) -> bool {
    for item in arr.iter_mut() {
        let Some(spec_str) = item.as_str() else {
            continue;
        };
        if !spec_str_matches_name(spec_str, &update.name) {
            continue;
        }
        let new_spec = replace_version_in_pep508(spec_str, &update.to);
        // Preserve the element's surrounding decor (leading newline +
        // indentation, trailing whitespace/comment) — a fresh `Formatted::new`
        // carries empty decor, which would collapse a multi-line array onto a
        // single line.
        if let toml_edit::Value::String(s) = item {
            dependency_check_updates_core::replace_string_preserving_decor(s, new_spec);
        }
        return true;
    }
    false
}

/// Split a PEP 508 dependency spec into `(name, rest)` at the PEP 503 name
/// boundary. The first character outside `[A-Za-z0-9._-]` ends the name;
/// `rest` is everything from that offset onwards (extras, version, marker).
///
/// Borrow-only; no allocation. Single source of truth for "where does the
/// package-name head stop and the rest of the PEP 508 spec begin" — every
/// other helper in this module that needs that split calls this function
/// instead of open-coding the boundary scan again, so adding any future
/// PEP 508 / PEP 685 edge case (tightening quoting in environment markers,
/// accepting unicode-normalised names) only has to land here.
fn split_pep508_name(spec: &str) -> (&str, &str) {
    let spec = spec.trim();
    let name_end = spec
        .find(|c: char| !c.is_alphanumeric() && c != '-' && c != '_' && c != '.')
        .unwrap_or(spec.len());
    spec.split_at(name_end)
}

/// Parse a PEP 508 dependency spec like `"requests>=2.28.0"` or `"flask~=2.0"`.
///
/// Returns `None` for specs without version constraints (e.g., bare `"requests"`).
fn parse_pep508_spec(spec: &str, section: DependencySection) -> Option<DependencySpec> {
    let (name, rest) = split_pep508_name(spec);
    if name.is_empty() {
        return None;
    }
    let rest = rest.trim();

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
        path_version: None,
    })
}

/// Check if a PEP 508 spec string matches a given package name.
fn spec_str_matches_name(spec: &str, name: &str) -> bool {
    let (spec_name, _) = split_pep508_name(spec);

    // PEP 503 normalized comparison (case-insensitive, treat - _ . as equivalent)
    normalize_pep503(spec_name) == normalize_pep503(name)
}

fn normalize_pep503(name: &str) -> String {
    name.to_lowercase().replace(['-', '.'], "_")
}

/// Replace the version constraint in a PEP 508 spec string.
fn replace_version_in_pep508(spec: &str, new_version: &str) -> String {
    let (name, rest) = split_pep508_name(spec);

    // Check for extras
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

/// Patch a single Poetry dep entry across all three value shapes that
/// [`extract_poetry_version`] collects from:
///
/// 1. `foo = "^2.0"`                          → `Item::Value(String)`
/// 2. `foo = {version = "^2.0", extras=[…]}`  → `Item::Value(InlineTable)`
/// 3. `[tool.poetry.dependencies.foo]`/`version = …` → `Item::Table`
///
/// Returns `true` when the name was found AND a `version` field existed to
/// rewrite (caller should stop searching). For shapes 2 and 3 the entry's
/// sibling keys (`extras`, `optional`, `source`, …) are left untouched, and
/// the `version` value's surrounding decor (leading whitespace, trailing
/// comments) is preserved byte-for-byte so format-preservation guarantees
/// hold. Mirrors the cargo-side [`update_dep_in_table`](../../../rust/src/parser.rs)
/// triple-shape `match` for behavioural parity.
fn apply_to_poetry_table(table: &mut toml_edit::Table, name: &str, new_version: &str) -> bool {
    let Some(item) = table.get_mut(name) else {
        return false;
    };
    match item {
        Item::Value(toml_edit::Value::String(s)) => {
            dependency_check_updates_core::replace_string_preserving_decor(
                s,
                new_version.to_owned(),
            );
            true
        }
        Item::Value(toml_edit::Value::InlineTable(t)) => {
            let Some(v) = t.get_mut("version") else {
                return false;
            };
            if let toml_edit::Value::String(s) = v {
                dependency_check_updates_core::replace_string_preserving_decor(
                    s,
                    new_version.to_owned(),
                );
            } else {
                *v = toml_edit::Value::String(toml_edit::Formatted::new(new_version.to_owned()));
            }
            true
        }
        Item::Table(t) => {
            let Some(v) = t.get_mut("version") else {
                return false;
            };
            if let Item::Value(toml_edit::Value::String(s)) = v {
                dependency_check_updates_core::replace_string_preserving_decor(
                    s,
                    new_version.to_owned(),
                );
            } else {
                *v = toml_edit::value(new_version);
            }
            true
        }
        _ => false,
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
    // Regression: `python` is the interpreter version constraint Poetry
    // tracks, not a PyPI package. The main-deps loop has always skipped it;
    // the dev-deps loop now mirrors that guard so a `python = "^3.11"` pin
    // under `[tool.poetry.dev-dependencies]` no longer leaks into the
    // resolve pipeline (`pytest` remains the only surviving spec).
    #[case::poetry_dev_dependencies_skips_python(
        "\n[tool.poetry.dev-dependencies]\npython = \"^3.11\"\npytest = \"^7.0\"\n",
        1,
        Some((
            Some("pytest"),
            Some("^7.0"),
            Some(DependencySection::DevDependencies),
        )),
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

    // ---------- 0027: previously-dropped sections now patched ----------
    //
    // Before this iteration, `apply_single_update` only patched PEP 621 main
    // `dependencies` and Poetry main `dependencies`. The four tests below lock
    // in the fix for the three sections that were silently dropped, plus a
    // pure no-op guard so the new branches cannot accidentally panic or
    // mutate when the dep is truly absent.

    #[test]
    fn apply_updates_patches_pep621_optional_dependencies() {
        // [project.optional-dependencies] dev = [...] — F1 in 0027-analyze.md.
        let toml = "[project]\nname = \"demo\"\n\n[project.optional-dependencies]\ndev = [\n    \"pytest>=7.0\",\n    \"black>=23.0\",\n]\n";
        let mut manifest = PyProjectManifest::parse(toml).unwrap();
        let updates = vec![PlannedUpdate {
            name: "pytest".to_owned(),
            section: DependencySection::OptionalDependencies,
            from: ">=7.0".to_owned(),
            to: ">=8.0".to_owned(),
        }];
        let result = manifest.apply_updates(&updates);
        // Newlines + 4-space indentation of every element preserved exactly.
        let expected = "[project]\nname = \"demo\"\n\n[project.optional-dependencies]\ndev = [\n    \"pytest>=8.0\",\n    \"black>=23.0\",\n]\n";
        assert_eq!(result, expected);
    }

    #[test]
    fn apply_updates_patches_pep735_dependency_groups() {
        // PEP 735 [dependency-groups] — the modern standard for dev deps in
        // PEP 621 projects. F1 in 0027-analyze.md.
        let toml =
            "[dependency-groups]\ntest = [\n    \"pytest>=7.0\",\n    \"coverage>=7.0\",\n]\n";
        let mut manifest = PyProjectManifest::parse(toml).unwrap();
        let updates = vec![PlannedUpdate {
            name: "coverage".to_owned(),
            section: DependencySection::DevDependencies,
            from: ">=7.0".to_owned(),
            to: ">=7.5".to_owned(),
        }];
        let result = manifest.apply_updates(&updates);
        let expected =
            "[dependency-groups]\ntest = [\n    \"pytest>=7.0\",\n    \"coverage>=7.5\",\n]\n";
        assert_eq!(result, expected);
    }

    #[test]
    fn apply_updates_patches_poetry_dev_dependencies() {
        // [tool.poetry.dev-dependencies] string-form dep. F1 in 0027-analyze.md.
        let toml = "[tool.poetry.dev-dependencies]\npytest = \"^7.0\"\n";
        let mut manifest = PyProjectManifest::parse(toml).unwrap();
        let updates = vec![PlannedUpdate {
            name: "pytest".to_owned(),
            section: DependencySection::DevDependencies,
            from: "^7.0".to_owned(),
            to: "^8.0".to_owned(),
        }];
        let result = manifest.apply_updates(&updates);
        let expected = "[tool.poetry.dev-dependencies]\npytest = \"^8.0\"\n";
        assert_eq!(result, expected);
    }

    #[test]
    fn apply_updates_unknown_dep_remains_a_silent_noop() {
        // Guard the new branches: an update for a name that exists in NO
        // section must remain a pure silent no-op (no panic, no mutation,
        // byte-equal output).
        let toml = "[project]\nname = \"demo\"\ndependencies = [\n    \"requests>=2.28.0\",\n]\n\n[project.optional-dependencies]\ndev = [\"pytest>=7.0\"]\n\n[dependency-groups]\ntest = [\"coverage>=7.0\"]\n\n[tool.poetry.dev-dependencies]\nblack = \"^23.0\"\n";
        let mut manifest = PyProjectManifest::parse(toml).unwrap();
        let updates = vec![PlannedUpdate {
            name: "totally-not-here".to_owned(),
            section: DependencySection::ProjectDependencies,
            from: ">=1.0".to_owned(),
            to: ">=2.0".to_owned(),
        }];
        let result = manifest.apply_updates(&updates);
        assert_eq!(result, toml);
    }

    // ---------- 0036: Poetry inline-table / full-table dep updates ----------
    //
    // Before this iteration, `apply_single_update` only patched the
    // `Item::Value(String)` shape of Poetry deps even though
    // `extract_poetry_version` (and therefore `compute_updates`) also
    // recognises inline-table and full-table forms. The three tests below
    // lock in the fix for those two previously-dropped shapes across both
    // `[tool.poetry.dependencies]` and `[tool.poetry.dev-dependencies]`,
    // and prove that sibling keys (`extras`, `optional`) are preserved
    // byte-for-byte. See 0036-analyze.md F1.

    #[test]
    fn apply_updates_patches_poetry_inline_table_in_dependencies() {
        // `flask = {version = "^2.0", extras = ["async"], optional = true}` —
        // only the `version` value bumps; every sibling key survives intact.
        let toml = "[tool.poetry.dependencies]\npython = \"^3.8\"\nflask = {version = \"^2.0\", extras = [\"async\"], optional = true}\n";
        let mut manifest = PyProjectManifest::parse(toml).unwrap();
        let updates = vec![PlannedUpdate {
            name: "flask".to_owned(),
            section: DependencySection::Dependencies,
            from: "^2.0".to_owned(),
            to: "^3.0".to_owned(),
        }];
        let result = manifest.apply_updates(&updates);
        let expected = "[tool.poetry.dependencies]\npython = \"^3.8\"\nflask = {version = \"^3.0\", extras = [\"async\"], optional = true}\n";
        assert_eq!(result, expected);
    }

    #[test]
    fn apply_updates_patches_poetry_inline_table_in_dev_dependencies() {
        // Same inline-table shape but under `[tool.poetry.dev-dependencies]`,
        // proving the dev-deps Poetry branch now also handles inline tables.
        let toml =
            "[tool.poetry.dev-dependencies]\npytest = {version = \"^7.0\", extras = [\"toml\"]}\n";
        let mut manifest = PyProjectManifest::parse(toml).unwrap();
        let updates = vec![PlannedUpdate {
            name: "pytest".to_owned(),
            section: DependencySection::DevDependencies,
            from: "^7.0".to_owned(),
            to: "^8.0".to_owned(),
        }];
        let result = manifest.apply_updates(&updates);
        let expected =
            "[tool.poetry.dev-dependencies]\npytest = {version = \"^8.0\", extras = [\"toml\"]}\n";
        assert_eq!(result, expected);
    }

    #[test]
    fn apply_updates_patches_poetry_full_table_form() {
        // `[tool.poetry.dependencies.sqlalchemy]` with `version` + `extras`
        // sub-keys — the version bumps, `extras = ["asyncio"]` survives, and
        // the section header / blank line layout is preserved.
        let toml = "[tool.poetry.dependencies]\npython = \"^3.8\"\n\n[tool.poetry.dependencies.sqlalchemy]\nversion = \"^2.0\"\nextras = [\"asyncio\"]\n";
        let mut manifest = PyProjectManifest::parse(toml).unwrap();
        let updates = vec![PlannedUpdate {
            name: "sqlalchemy".to_owned(),
            section: DependencySection::Dependencies,
            from: "^2.0".to_owned(),
            to: "^3.0".to_owned(),
        }];
        let result = manifest.apply_updates(&updates);
        let expected = "[tool.poetry.dependencies]\npython = \"^3.8\"\n\n[tool.poetry.dependencies.sqlalchemy]\nversion = \"^3.0\"\nextras = [\"asyncio\"]\n";
        assert_eq!(result, expected);
    }
}
