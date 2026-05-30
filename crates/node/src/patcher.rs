//! Surgical byte-range JSON patch engine for format-preserving updates.
//!
//! Instead of re-serializing JSON (which destroys formatting), this module
//! finds the exact byte positions of dependency version strings in the original
//! text and replaces only those bytes.

use dependency_check_updates_core::{DependencySection, PlannedUpdate};

use crate::parser::DEPENDENCY_SECTIONS;

/// A located version string within the JSON text.
#[derive(Debug, Clone)]
pub struct VersionLocation {
    /// The dependency section this belongs to.
    pub section: DependencySection,
    /// The package name.
    pub name: String,
    /// Byte offset of the first character INSIDE the quotes (after opening `"`).
    pub value_start: usize,
    /// Byte offset of the closing quote `"` (exclusive end of value content).
    pub value_end: usize,
}

/// A patch to apply: replace bytes `[start..end)` with `new_value`.
#[derive(Debug, Clone)]
pub struct Patch {
    pub start: usize,
    pub end: usize,
    pub new_value: String,
}

/// Errors from the patch engine.
#[derive(Debug, thiserror::Error)]
pub enum PatchError {
    /// JSON failed to parse during a full-document scan. Only the test-only
    /// [`JsonPatcher::scan_version_locations`] performs that parse; the
    /// production path ([`JsonPatcher::scan_for_updates`]) is infallible.
    #[cfg(test)]
    #[error("failed to scan JSON: {0}")]
    ScanFailed(String),
    #[error("overlapping patches detected")]
    OverlappingPatches,
    #[error("patched output is not valid JSON: {0}")]
    ValidationFailed(String),
}

/// Format-preserving JSON patcher.
pub struct JsonPatcher;

impl JsonPatcher {
    /// Scan the raw JSON text to find byte positions of all dependency version values.
    ///
    /// Test-only: the production path uses [`JsonPatcher::scan_for_updates`],
    /// which scans only the deps being updated. This full-document variant is
    /// retained purely to exercise the byte-scanning helpers in isolation.
    ///
    /// # Errors
    ///
    /// Returns an error if the JSON cannot be parsed or section positions cannot be found.
    #[cfg(test)]
    pub fn scan_version_locations(text: &str) -> Result<Vec<VersionLocation>, PatchError> {
        let parsed: serde_json::Value =
            serde_json::from_str(text).map_err(|e| PatchError::ScanFailed(e.to_string()))?;

        let mut locations = Vec::new();

        for &(section, section_key) in DEPENDENCY_SECTIONS {
            if let Some(serde_json::Value::Object(deps)) = parsed.get(section_key) {
                let Some((obj_start, obj_end)) = find_section_bounds(text, section_key) else {
                    continue;
                };

                // For each dependency in this section, find its value position
                for (dep_name, dep_value) in deps {
                    if let Some(version_str) = dep_value.as_str() {
                        if let Some(loc) = find_dep_value_position(
                            text,
                            obj_start,
                            obj_end,
                            dep_name,
                            version_str,
                            section,
                        ) {
                            locations.push(loc);
                        }
                    }
                }
            }
        }

        Ok(locations)
    }

    /// Find byte positions of specific dependencies without a full JSON parse.
    ///
    /// This is an optimized path for `apply_updates` where we already know which
    /// deps to look for. Scans only the relevant sections and deps, avoiding
    /// the cost of deserializing the entire JSON document.
    ///
    /// Infallible: deps whose section or value cannot be located are simply
    /// omitted from the result.
    #[must_use]
    pub fn scan_for_updates(text: &str, updates: &[PlannedUpdate]) -> Vec<VersionLocation> {
        use std::collections::HashMap;

        if updates.is_empty() {
            return Vec::new();
        }

        // Group updates by section for targeted scanning
        let mut by_section: HashMap<DependencySection, Vec<&PlannedUpdate>> = HashMap::new();
        for update in updates {
            by_section.entry(update.section).or_default().push(update);
        }

        let mut locations = Vec::with_capacity(updates.len());

        for &(section, section_key) in DEPENDENCY_SECTIONS {
            let Some(section_updates) = by_section.get(&section) else {
                continue;
            };

            let Some((obj_start, obj_end)) = find_section_bounds(text, section_key) else {
                continue;
            };

            // Only scan for deps we need to update
            for update in section_updates {
                if let Some(loc) = find_dep_value_position(
                    text,
                    obj_start,
                    obj_end,
                    &update.name,
                    &update.from,
                    section,
                ) {
                    locations.push(loc);
                }
            }
        }

        locations
    }

    /// Apply patches to the original text, replacing version strings.
    ///
    /// Patches are applied back-to-front (highest offset first) so that earlier
    /// byte offsets are not invalidated.
    ///
    /// # Errors
    ///
    /// Returns an error if patches overlap or the result is not valid JSON.
    pub fn apply_patches(original: &str, patches: &[Patch]) -> Result<String, PatchError> {
        if patches.is_empty() {
            return Ok(original.to_owned());
        }

        // Sort descending by start position
        let mut sorted: Vec<&Patch> = patches.iter().collect();
        sorted.sort_by_key(|p| std::cmp::Reverse(p.start));

        // Check for overlapping patches
        for window in sorted.windows(2) {
            // sorted is descending, so window[0].start >= window[1].start
            if window[1].end > window[0].start {
                return Err(PatchError::OverlappingPatches);
            }
        }

        let mut result = original.to_owned();
        for patch in &sorted {
            result.replace_range(patch.start..patch.end, &patch.new_value);
        }

        // Verify the result is still valid JSON
        serde_json::from_str::<serde_json::Value>(&result)
            .map_err(|e| PatchError::ValidationFailed(e.to_string()))?;

        Ok(result)
    }
}

/// Find the byte range `(obj_start, obj_end)` of a dependency-section object
/// in the raw JSON text.
///
/// Returns `None` if the section key cannot be located, the opening `{` is
/// missing, or the matching `}` is missing.
fn find_section_bounds(text: &str, section_key: &str) -> Option<(usize, usize)> {
    let section_key_pos = find_json_key_position(text, section_key, 0)?;
    let search_from = section_key_pos + section_key.len() + 2; // skip past `"key"`
    let obj_start = find_char_skipping_strings(text, '{', search_from)?;
    let obj_end = find_matching_brace(text, obj_start)?;
    Some((obj_start, obj_end))
}

/// Find the byte position of a JSON key string in the text.
///
/// Searches for `"key"` as a JSON key (followed by `:`), starting from `from`.
fn find_json_key_position(text: &str, key: &str, from: usize) -> Option<usize> {
    let needle = format!("\"{key}\"");
    let bytes = text.as_bytes();
    let needle_bytes = needle.as_bytes();
    let mut pos = from;

    while pos + needle_bytes.len() <= bytes.len() {
        if let Some(found) = text[pos..].find(&needle) {
            let abs_pos = pos + found;
            // Verify this is a key (followed by optional whitespace then `:`)
            let after = abs_pos + needle_bytes.len();
            if let Some(colon_pos) = find_char_skipping_whitespace(text, ':', after) {
                if colon_pos < text.len() {
                    return Some(abs_pos);
                }
            }
            pos = abs_pos + 1;
        } else {
            break;
        }
    }

    None
}

/// Find the next occurrence of `ch` skipping whitespace.
///
/// Scans only the leading whitespace run: it stops at the first non-whitespace
/// character and returns its offset only if it is `ch`. Written as an iterator
/// chain (rather than a loop with early returns) so every branch is a single
/// covered expression.
fn find_char_skipping_whitespace(text: &str, ch: char, from: usize) -> Option<usize> {
    text[from..]
        .char_indices()
        .take_while(|(_, c)| *c == ch || c.is_whitespace())
        .find(|(_, c)| *c == ch)
        .map(|(i, _)| from + i)
}

/// Find the next `"` character after skipping whitespace, starting from `from`.
fn find_next_quote(text: &str, from: usize) -> Option<usize> {
    for (i, c) in text[from..].char_indices() {
        if c == '"' {
            return Some(from + i);
        }
        if !c.is_whitespace() {
            return None; // Non-whitespace, non-quote character found
        }
    }
    None
}

/// Find the next occurrence of `ch` outside of JSON strings, starting from `from`.
fn find_char_skipping_strings(text: &str, ch: char, from: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = from;
    let mut in_string = false;

    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if b == b'\\' {
                i += 2; // skip escaped character
                continue;
            }
            if b == b'"' {
                in_string = false;
            }
        } else if b == b'"' {
            in_string = true;
        } else if b == ch as u8 {
            return Some(i);
        }
        i += 1;
    }

    None
}

/// Find the matching closing brace for an opening brace at `open_pos`.
///
/// Correctly handles nested braces and JSON strings with escaped characters.
fn find_matching_brace(text: &str, open_pos: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.get(open_pos) != Some(&b'{') {
        return None;
    }

    let mut depth = 0i32;
    let mut i = open_pos;
    let mut in_string = false;

    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_string = false;
            }
        } else {
            match b {
                b'"' => in_string = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }

    None
}

/// Find the byte position of a dependency's version value within a section span.
fn find_dep_value_position(
    text: &str,
    section_start: usize,
    section_end: usize,
    dep_name: &str,
    version_str: &str,
    section: DependencySection,
) -> Option<VersionLocation> {
    let section_text = &text[section_start..=section_end];

    // Find the dep key within this section
    let dep_key_needle = format!("\"{dep_name}\"");
    let dep_key_offset = section_text.find(&dep_key_needle)?;
    let abs_key_pos = section_start + dep_key_offset;

    // Find the colon after the key
    let after_key = abs_key_pos + dep_key_needle.len();
    let colon_pos = find_char_skipping_whitespace(text, ':', after_key)?;

    // Find the opening quote of the value string after the colon.
    // Skip whitespace then expect `"`.
    let value_quote_start = find_next_quote(text, colon_pos + 1)?;

    // The value content starts after the opening quote
    let value_start = value_quote_start + 1;
    let value_end = value_start + version_str.len();

    // Verify the content matches
    if text.get(value_start..value_end) == Some(version_str) {
        Some(VersionLocation {
            section,
            name: dep_name.to_owned(),
            value_start,
            value_end,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    /// Build a single-target patch list from the located version positions.
    fn replace_all(locations: &[VersionLocation], new_value: &str) -> Vec<Patch> {
        locations
            .iter()
            .map(|loc| Patch {
                start: loc.value_start,
                end: loc.value_end,
                new_value: new_value.to_owned(),
            })
            .collect()
    }

    /// Build a single-version `PlannedUpdate` in the `Dependencies` section.
    fn update(name: &str, from: &str, to: &str) -> PlannedUpdate {
        PlannedUpdate {
            name: name.to_owned(),
            section: DependencySection::Dependencies,
            from: from.to_owned(),
            to: to.to_owned(),
        }
    }

    #[test]
    fn test_roundtrip_empty_patches() {
        let input = "{\n  \"dependencies\": {\n    \"react\": \"^17.0.0\"\n  }\n}\n";
        let result = JsonPatcher::apply_patches(input, &[]).unwrap();
        assert_eq!(result, input);
    }

    #[test]
    fn test_single_dep_update() {
        let input = "{\n  \"dependencies\": {\n    \"react\": \"^17.0.0\"\n  }\n}\n";
        let expected = "{\n  \"dependencies\": {\n    \"react\": \"^18.2.0\"\n  }\n}\n";

        let locations = JsonPatcher::scan_version_locations(input).unwrap();
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].name, "react");
        assert_eq!(
            &input[locations[0].value_start..locations[0].value_end],
            "^17.0.0"
        );

        let result = JsonPatcher::apply_patches(input, &replace_all(&locations, "^18.2.0")).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_multiple_deps_same_section() {
        let input = r#"{
  "dependencies": {
    "react": "^17.0.0",
    "lodash": "^4.17.0"
  }
}
"#;
        let locations = JsonPatcher::scan_version_locations(input).unwrap();
        assert_eq!(locations.len(), 2);

        let patches: Vec<Patch> = locations
            .iter()
            .map(|loc| Patch {
                start: loc.value_start,
                end: loc.value_end,
                new_value: if loc.name == "react" {
                    "^18.2.0".to_owned()
                } else {
                    "^4.17.21".to_owned()
                },
            })
            .collect();

        let result = JsonPatcher::apply_patches(input, &patches).unwrap();
        assert!(result.contains("\"^18.2.0\""));
        assert!(result.contains("\"^4.17.21\""));
        // Verify structure is preserved (indentation, newlines)
        assert!(result.starts_with("{\n  \"dependencies\""));
    }

    #[test]
    fn test_cross_section_update() {
        let input = r#"{
  "dependencies": {
    "react": "^17.0.0"
  },
  "devDependencies": {
    "typescript": "^4.0.0"
  }
}
"#;
        let locations = JsonPatcher::scan_version_locations(input).unwrap();
        assert_eq!(locations.len(), 2);

        let react = locations.iter().find(|l| l.name == "react").unwrap();
        let ts = locations.iter().find(|l| l.name == "typescript").unwrap();

        assert_eq!(react.section, DependencySection::Dependencies);
        assert_eq!(ts.section, DependencySection::DevDependencies);

        let patches = vec![
            Patch {
                start: react.value_start,
                end: react.value_end,
                new_value: "^18.2.0".to_owned(),
            },
            Patch {
                start: ts.value_start,
                end: ts.value_end,
                new_value: "^5.3.0".to_owned(),
            },
        ];

        let result = JsonPatcher::apply_patches(input, &patches).unwrap();
        assert!(result.contains("\"^18.2.0\""));
        assert!(result.contains("\"^5.3.0\""));
    }

    #[test]
    fn test_2space_indent_preserved() {
        // Asserts the byte-diff stays within the value range — distinct from
        // the equality-based indent tests below, so kept as its own test.
        let input = "{\n  \"dependencies\": {\n    \"react\": \"^17.0.0\"\n  }\n}\n";
        let locations = JsonPatcher::scan_version_locations(input).unwrap();
        let result = JsonPatcher::apply_patches(input, &replace_all(&locations, "^18.2.0")).unwrap();

        // Verify only the version value changed
        let diff_bytes: Vec<usize> = input
            .bytes()
            .zip(result.bytes())
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .map(|(i, _)| i)
            .collect();

        // The diff should be exactly the version string bytes
        assert!(!diff_bytes.is_empty());
        // All changed bytes should be within the patch range
        for &pos in &diff_bytes {
            assert!(pos >= locations[0].value_start && pos < locations[0].value_end);
        }
    }

    #[rstest]
    // Equality-based format-preservation: patching `react: ^17.0.0 → ^18.2.0`
    // must leave every other byte untouched.
    #[case::four_space_indent(
        "{\n    \"dependencies\": {\n        \"react\": \"^17.0.0\"\n    }\n}\n",
        "{\n    \"dependencies\": {\n        \"react\": \"^18.2.0\"\n    }\n}\n",
    )]
    #[case::tab_indent(
        "{\n\t\"dependencies\": {\n\t\t\"react\": \"^17.0.0\"\n\t}\n}\n",
        "{\n\t\"dependencies\": {\n\t\t\"react\": \"^18.2.0\"\n\t}\n}\n",
    )]
    #[case::crlf_line_endings(
        "{\r\n  \"dependencies\": {\r\n    \"react\": \"^17.0.0\"\r\n  }\r\n}\r\n",
        "{\r\n  \"dependencies\": {\r\n    \"react\": \"^18.2.0\"\r\n  }\r\n}\r\n",
    )]
    fn format_preserved_when_patching(#[case] input: &str, #[case] expected: &str) {
        let locations = JsonPatcher::scan_version_locations(input).unwrap();
        let result = JsonPatcher::apply_patches(input, &replace_all(&locations, "^18.2.0")).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_trailing_newline_preserved() {
        let input = "{\n  \"dependencies\": {\n    \"react\": \"^17.0.0\"\n  }\n}\n";
        let locations = JsonPatcher::scan_version_locations(input).unwrap();
        let result = JsonPatcher::apply_patches(input, &replace_all(&locations, "^18.2.0")).unwrap();
        assert!(result.ends_with('\n'));
    }

    #[test]
    fn test_no_trailing_newline_preserved() {
        let input = "{\n  \"dependencies\": {\n    \"react\": \"^17.0.0\"\n  }\n}";
        let locations = JsonPatcher::scan_version_locations(input).unwrap();
        let result = JsonPatcher::apply_patches(input, &replace_all(&locations, "^18.2.0")).unwrap();
        assert!(result.ends_with('}'));
        assert!(!result.ends_with("}\n"));
    }

    #[test]
    fn test_scoped_package_names() {
        let input = r#"{
  "dependencies": {
    "@types/react": "^18.0.0",
    "@babel/core": "^7.20.0"
  }
}
"#;
        let locations = JsonPatcher::scan_version_locations(input).unwrap();
        assert_eq!(locations.len(), 2);

        let types_react = locations.iter().find(|l| l.name == "@types/react").unwrap();
        assert_eq!(
            &input[types_react.value_start..types_react.value_end],
            "^18.0.0"
        );

        let babel = locations.iter().find(|l| l.name == "@babel/core").unwrap();
        assert_eq!(
            &input[babel.value_start..babel.value_end],
            "^7.20.0"
        );
    }

    #[test]
    fn test_range_prefix_preserved() {
        let input = "{\n  \"dependencies\": {\n    \"react\": \"~17.0.0\"\n  }\n}\n";

        let locations = JsonPatcher::scan_version_locations(input).unwrap();
        let result = JsonPatcher::apply_patches(input, &replace_all(&locations, "~18.2.0")).unwrap();
        assert!(result.contains("\"~18.2.0\""));
    }

    #[test]
    fn test_scan_locations_correct_positions() {
        let input = "{\n  \"dependencies\": {\n    \"react\": \"^17.0.0\"\n  }\n}\n";

        let locations = JsonPatcher::scan_version_locations(input).unwrap();
        assert_eq!(locations.len(), 1);

        let loc = &locations[0];
        assert_eq!(&input[loc.value_start..loc.value_end], "^17.0.0");
    }

    #[test]
    fn test_validation_catches_corruption() {
        // If we somehow produce invalid JSON, it should error
        let patches = vec![Patch {
            start: 0,
            end: 1,
            new_value: "INVALID".to_owned(),
        }];
        let result = JsonPatcher::apply_patches("{}", &patches);
        assert!(result.is_err());
    }

    #[rstest]
    // text, open-brace position, expected matching-`}` position.
    #[case::flat(r#"{ "a": { "b": 1 }, "c": 2 }"#, 0, Some(26))]
    #[case::nested_outer(r#"{ "a": { "b": {} } }"#, 0, Some(19))]
    #[case::nested_inner(r#"{ "a": { "b": {} } }"#, 7, Some(17))]
    #[case::string_with_braces(r#"{ "a": "}{}{", "b": 1 }"#, 0, Some(22))]
    #[case::escaped_quotes_in_string(
        r#"{ "key": "value with \" escaped \" quotes", "num": 1 }"#,
        0,
        Some(53),
    )]
    #[case::escaped_backslash_in_string(r#"{ "key": "val\\", "num": 1 }"#, 0, Some(27))]
    #[case::not_a_brace("abc", 0, None)]
    #[case::unmatched("{ unclosed", 0, None)]
    fn find_matching_brace_cases(
        #[case] text: &str,
        #[case] open_pos: usize,
        #[case] expected: Option<usize>,
    ) {
        assert_eq!(find_matching_brace(text, open_pos), expected);
    }

    #[test]
    fn test_version_with_different_length() {
        // Version string changes length: "^1.0.0" -> "^10.0.0"
        let input = "{\n  \"dependencies\": {\n    \"react\": \"^1.0.0\"\n  }\n}\n";

        let locations = JsonPatcher::scan_version_locations(input).unwrap();
        let result = JsonPatcher::apply_patches(input, &replace_all(&locations, "^10.0.0")).unwrap();
        assert!(result.contains("\"^10.0.0\""));
        // Verify it's still valid JSON
        let _: serde_json::Value = serde_json::from_str(&result).unwrap();
    }

    #[test]
    fn test_scan_for_updates_basic() {
        let input = "{\n  \"dependencies\": {\n    \"react\": \"^17.0.0\"\n  }\n}\n";
        let updates = vec![update("react", "^17.0.0", "^18.2.0")];
        let locations = JsonPatcher::scan_for_updates(input, &updates);
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].name, "react");
        assert_eq!(
            &input[locations[0].value_start..locations[0].value_end],
            "^17.0.0"
        );
        assert_eq!(
            &input[locations[0].value_start..locations[0].value_end],
            "^17.0.0"
        );
    }

    #[rstest]
    // Each case feeds a single-update list to `scan_for_updates` against an
    // input where the update cannot be located. The returned locations must
    // be empty for every variant.
    #[case::empty_updates(
        "{\n  \"dependencies\": {\n    \"react\": \"^17.0.0\"\n  }\n}\n",
        vec![],
    )]
    #[case::missing_section(
        r#"{"dependencies": {"react": "^17.0.0"}}"#,
        vec![PlannedUpdate {
            name: "typescript".to_owned(),
            section: DependencySection::DevDependencies,
            from: "^4.0.0".to_owned(),
            to: "^5.3.0".to_owned(),
        }],
    )]
    #[case::dep_not_in_section(
        r#"{"dependencies": {"react": "^17.0.0"}}"#,
        vec![update("nonexistent", "^1.0.0", "^2.0.0")],
    )]
    #[case::version_mismatch(
        "{\n  \"dependencies\": {\n    \"react\": \"^18.0.0\"\n  }\n}\n",
        // `from` doesn't match the value in the JSON → no location.
        vec![update("react", "^17.0.0", "^19.0.0")],
    )]
    fn scan_for_updates_returns_empty(#[case] input: &str, #[case] updates: Vec<PlannedUpdate>) {
        let locations = JsonPatcher::scan_for_updates(input, &updates);
        assert!(locations.is_empty());
    }

    #[test]
    fn test_scan_for_updates_multiple_sections() {
        let input = r#"{
  "dependencies": {
    "react": "^17.0.0"
  },
  "devDependencies": {
    "typescript": "^4.0.0"
  }
}
"#;
        let updates = vec![
            update("react", "^17.0.0", "^18.2.0"),
            PlannedUpdate {
                name: "typescript".to_owned(),
                section: DependencySection::DevDependencies,
                from: "^4.0.0".to_owned(),
                to: "^5.3.0".to_owned(),
            },
        ];
        let locations = JsonPatcher::scan_for_updates(input, &updates);
        assert_eq!(locations.len(), 2);
        let react = locations.iter().find(|l| l.name == "react").unwrap();
        let ts = locations.iter().find(|l| l.name == "typescript").unwrap();
        assert_eq!(react.section, DependencySection::Dependencies);
        assert_eq!(ts.section, DependencySection::DevDependencies);
    }

    #[test]
    fn test_scan_for_updates_only_targets_requested() {
        let input = r#"{
  "dependencies": {
    "react": "^17.0.0",
    "lodash": "^4.17.0"
  }
}
"#;
        // Only update react, not lodash
        let updates = vec![update("react", "^17.0.0", "^18.2.0")];
        let locations = JsonPatcher::scan_for_updates(input, &updates);
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].name, "react");
    }

    #[test]
    fn test_scan_for_updates_apply_roundtrip() {
        let input = "{\n  \"dependencies\": {\n    \"react\": \"^17.0.0\"\n  }\n}\n";
        let expected = "{\n  \"dependencies\": {\n    \"react\": \"^18.2.0\"\n  }\n}\n";
        let updates = vec![update("react", "^17.0.0", "^18.2.0")];
        let locations = JsonPatcher::scan_for_updates(input, &updates);
        let result = JsonPatcher::apply_patches(input, &replace_all(&locations, "^18.2.0")).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_overlapping_patches_error() {
        let input = "{\n  \"dependencies\": {\n    \"react\": \"^17.0.0\"\n  }\n}\n";
        let patches = vec![
            Patch {
                start: 5,
                end: 15,
                new_value: "a".to_owned(),
            },
            Patch {
                start: 10,
                end: 20,
                new_value: "b".to_owned(),
            },
        ];
        let result = JsonPatcher::apply_patches(input, &patches);
        assert!(result.is_err());
    }

    #[rstest]
    // Each case calls `scan_version_locations` and expects a single hit for
    // `react` in the standard `dependencies` section despite JSON features
    // (escaped quotes, nested non-dep braces) that could trip up scanning.
    #[case::escaped_strings_in_other_fields(
        r#"{
  "name": "test \"project\"",
  "dependencies": {
    "react": "^17.0.0"
  }
}
"#,
    )]
    #[case::nested_non_dep_section_with_braces(
        r#"{
  "scripts": {
    "build": "echo {test}"
  },
  "dependencies": {
    "react": "^17.0.0"
  }
}
"#,
    )]
    #[case::escaped_quotes_in_description(
        r#"{
  "description": "A \"great\" package",
  "dependencies": {
    "react": "^17.0.0"
  }
}
"#,
    )]
    fn scan_version_locations_finds_only_react(#[case] input: &str) {
        let locations = JsonPatcher::scan_version_locations(input).unwrap();
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].name, "react");
    }

    #[test]
    fn test_scan_version_locations_no_dep_sections() {
        let input = r#"{"name": "test", "version": "1.0.0"}"#;
        let locations = JsonPatcher::scan_version_locations(input).unwrap();
        assert!(locations.is_empty());
    }

    #[test]
    fn test_find_char_skipping_strings_with_escaped_quotes() {
        // Find `:` while skipping a string that contains escaped quotes
        let text = r#""key with \" escaped": value"#;
        let result = find_char_skipping_strings(text, ':', 0);
        // The colon after the key string
        assert!(result.is_some());
    }

    #[test]
    fn test_find_json_key_position_skips_value_match() {
        // "dependencies" appears as a value, not a key - should be skipped
        let input = r#"{"name": "dependencies", "dependencies": {"react": "^17.0.0"}}"#;
        let pos = find_json_key_position(input, "dependencies", 0);
        assert!(pos.is_some());
        // Should find the key, not the value
        let found_pos = pos.unwrap();
        assert!(found_pos > 10); // Should be after the value occurrence
    }

    #[test]
    fn test_find_json_key_position_not_found() {
        let input = r#"{"name": "test"}"#;
        let pos = find_json_key_position(input, "nonexistent", 0);
        assert!(pos.is_none());
    }

    #[rstest]
    // text, target char, `from` index, expected return.
    #[case::non_target_first("abc:", ':', 0, None)]
    #[case::immediate_hit(":rest", ':', 0, Some(0))]
    #[case::empty_slice_from_end("abc", ':', 3, None)]
    // Whitespace is skipped, then a non-whitespace non-target char triggers
    // the early `return None` at the `!c.is_whitespace()` branch.
    #[case::whitespace_then_non_target("  x:", ':', 0, None)]
    fn find_char_skipping_whitespace_cases(
        #[case] text: &str,
        #[case] ch: char,
        #[case] from: usize,
        #[case] expected: Option<usize>,
    ) {
        assert_eq!(find_char_skipping_whitespace(text, ch, from), expected);
    }

    #[rstest]
    #[case::non_quote_char_first("abc\"", 0, None)]
    #[case::leading_whitespace("  \"hello\"", 0, Some(2))]
    #[case::empty_slice_from_end("abc", 3, None)]
    fn find_next_quote_cases(
        #[case] text: &str,
        #[case] from: usize,
        #[case] expected: Option<usize>,
    ) {
        assert_eq!(find_next_quote(text, from), expected);
    }

    #[rstest]
    // Single-section JSON: scan returns one location whose `section` matches.
    #[case::peer_dependencies(
        r#"{
  "peerDependencies": {
    "react": "^17.0.0 || ^18.0.0"
  }
}
"#,
        DependencySection::PeerDependencies,
    )]
    #[case::optional_dependencies(
        r#"{
  "optionalDependencies": {
    "fsevents": "^2.3.0"
  }
}
"#,
        DependencySection::OptionalDependencies,
    )]
    fn scan_version_locations_section_specific(
        #[case] input: &str,
        #[case] expected_section: DependencySection,
    ) {
        let locations = JsonPatcher::scan_version_locations(input).unwrap();
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].section, expected_section);
    }

    #[test]
    fn test_find_char_skipping_strings_no_match() {
        // Target char doesn't exist outside strings
        let text = r#""contains : colon""#;
        let result = find_char_skipping_strings(text, ':', 0);
        assert!(result.is_none());
    }

    #[rstest]
    // Each invalid case must produce `None` from `find_section_bounds`.
    #[case::key_not_found("{}")]
    #[case::no_brace_after_key(r#"{"dependencies": "#)]
    #[case::no_matching_close_brace(r#"{"dependencies": {"#)]
    fn find_section_bounds_invalid_cases(#[case] text: &str) {
        assert!(find_section_bounds(text, "dependencies").is_none());
    }

    #[test]
    fn test_find_section_bounds_valid() {
        let text = r#"{"dependencies": {"react": "^17.0.0"}}"#;
        let bounds = find_section_bounds(text, "dependencies");
        assert!(bounds.is_some());
        let (start, end) = bounds.unwrap();
        assert_eq!(&text[start..=end], r#"{"react": "^17.0.0"}"#);
    }

    #[test]
    fn test_scan_version_locations_unicode_escaped_section_key() {
        // serde_json decodes \u0065 → 'e', seeing "dependencies" as the key.
        // But find_section_bounds searches for literal "dependencies" in the
        // raw text and won't find "depend\u0065ncies" — exercises the continue.
        let input = "{\"depend\\u0065ncies\": {\"react\": \"^17.0.0\"}}";
        let locations = JsonPatcher::scan_version_locations(input).unwrap();
        assert!(locations.is_empty());
    }
}
