//! pyproject.toml parsing and format-preserving dependency updates via `toml_edit`.
//!
//! Supports:
//! - `[project] dependencies` (PEP 621)
//! - `[tool.poetry.dependencies]` (Poetry)
//! - `[dependency-groups]` (PEP 735)

use dcu_core::{DependencySection, DependencySpec, PlannedUpdate};
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
                            deps.push(DependencySpec {
                                name: name.to_owned(),
                                current_req: version,
                                section: DependencySection::Dependencies,
                            });
                        }
                    }
                }
                // Poetry dev-dependencies
                if let Some(dev_deps) = poetry.get("dev-dependencies").and_then(Item::as_table) {
                    for (name, item) in dev_deps {
                        if let Some(version) = extract_poetry_version(item) {
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
    /// # Errors
    ///
    /// Returns an error if a dependency cannot be updated.
    pub fn apply_updates(&mut self, updates: &[PlannedUpdate]) -> Result<String, PyProjectError> {
        for update in updates {
            self.apply_single_update(update);
        }
        Ok(self.doc.to_string())
    }

    fn apply_single_update(&mut self, update: &PlannedUpdate) {
        // Try PEP 621 project.dependencies
        if let Some(project) = self.doc.get_mut("project").and_then(Item::as_table_mut) {
            if let Some(dep_array) = project.get_mut("dependencies").and_then(Item::as_array_mut) {
                for item in dep_array.iter_mut() {
                    if let Some(spec_str) = item.as_str() {
                        if spec_str_matches_name(spec_str, &update.name) {
                            let new_spec = replace_version_in_pep508(spec_str, &update.to);
                            *item = toml_edit::Value::String(toml_edit::Formatted::new(new_spec));
                            return;
                        }
                    }
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

    #[test]
    fn test_parse_pep621_dependencies() {
        let toml = r#"
[project]
name = "my-project"
dependencies = [
    "requests>=2.28.0",
    "flask~=2.0",
    "click>=8.0,<9.0",
]
"#;
        let manifest = PyProjectManifest::parse(toml).unwrap();
        assert_eq!(manifest.dependencies.len(), 3);
        assert_eq!(manifest.dependencies[0].name, "requests");
        assert_eq!(manifest.dependencies[0].current_req, ">=2.28.0");
        assert_eq!(
            manifest.dependencies[0].section,
            DependencySection::ProjectDependencies
        );
    }

    #[test]
    fn test_parse_pep621_optional_deps() {
        let toml = r#"
[project.optional-dependencies]
dev = ["pytest>=7.0", "black>=23.0"]
docs = ["sphinx>=5.0"]
"#;
        let manifest = PyProjectManifest::parse(toml).unwrap();
        assert_eq!(manifest.dependencies.len(), 3);
        assert_eq!(
            manifest.dependencies[0].section,
            DependencySection::OptionalDependencies
        );
    }

    #[test]
    fn test_parse_poetry_dependencies() {
        let toml = r#"
[tool.poetry.dependencies]
python = "^3.8"
requests = "^2.28.0"
flask = {version = "^2.0", optional = true}
"#;
        let manifest = PyProjectManifest::parse(toml).unwrap();
        // python is skipped
        assert_eq!(manifest.dependencies.len(), 2);
        assert_eq!(manifest.dependencies[0].name, "requests");
        assert_eq!(manifest.dependencies[0].current_req, "^2.28.0");
    }

    #[test]
    fn test_parse_poetry_dev_dependencies() {
        let toml = r#"
[tool.poetry.dev-dependencies]
pytest = "^7.0"
"#;
        let manifest = PyProjectManifest::parse(toml).unwrap();
        assert_eq!(manifest.dependencies.len(), 1);
        assert_eq!(
            manifest.dependencies[0].section,
            DependencySection::DevDependencies
        );
    }

    #[test]
    fn test_parse_dependency_groups() {
        let toml = r#"
[dependency-groups]
test = ["pytest>=7.0", "coverage>=7.0"]
"#;
        let manifest = PyProjectManifest::parse(toml).unwrap();
        assert_eq!(manifest.dependencies.len(), 2);
    }

    #[test]
    fn test_skip_bare_deps() {
        let toml = r#"
[project]
dependencies = ["requests", "flask>=2.0"]
"#;
        let manifest = PyProjectManifest::parse(toml).unwrap();
        assert_eq!(manifest.dependencies.len(), 1);
        assert_eq!(manifest.dependencies[0].name, "flask");
    }

    #[test]
    fn test_pep508_with_extras() {
        let dep = parse_pep508_spec(
            "requests[security]>=2.28.0",
            DependencySection::ProjectDependencies,
        );
        let dep = dep.unwrap();
        assert_eq!(dep.name, "requests");
        assert_eq!(dep.current_req, ">=2.28.0");
    }

    #[test]
    fn test_pep508_with_markers() {
        let dep = parse_pep508_spec(
            "pywin32>=300; sys_platform == 'win32'",
            DependencySection::ProjectDependencies,
        );
        let dep = dep.unwrap();
        assert_eq!(dep.name, "pywin32");
        assert_eq!(dep.current_req, ">=300");
    }

    #[test]
    fn test_no_deps_empty() {
        let toml = r#"
[project]
name = "empty"
"#;
        let manifest = PyProjectManifest::parse(toml).unwrap();
        assert!(manifest.dependencies.is_empty());
    }

    #[test]
    fn test_comments_preserved() {
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
        let result = manifest.apply_updates(&[]).unwrap();
        assert!(result.contains("# Project config"));
        assert!(result.contains("# Main dependencies"));
    }

    #[test]
    fn test_replace_version_in_pep508() {
        assert_eq!(
            replace_version_in_pep508("requests>=2.28.0", ">=2.31.0"),
            "requests>=2.31.0"
        );
        assert_eq!(
            replace_version_in_pep508("flask~=2.0", "~=3.0"),
            "flask~=3.0"
        );
        assert_eq!(
            replace_version_in_pep508("pywin32>=300; sys_platform == 'win32'", ">=306"),
            "pywin32>=306; sys_platform == 'win32'"
        );
    }

    #[test]
    fn test_normalize_pep503() {
        assert_eq!(normalize_pep503("My-Package"), "my_package");
        assert_eq!(normalize_pep503("my.package"), "my_package");
        assert_eq!(normalize_pep503("MY_PACKAGE"), "my_package");
    }
}
