//! Surgical byte-range JSON patch engine for format-preserving updates.
//!
//! Instead of re-serializing JSON (which destroys formatting), this module
//! finds the exact byte positions of dependency version strings in the original
//! text and replaces only those bytes.

use dependency_check_updates_core::DependencySection;

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
    /// The current version string (without quotes).
    pub current_value: String,
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
    /// # Errors
    ///
    /// Returns an error if the JSON cannot be parsed or section positions cannot be found.
    pub fn scan_version_locations(text: &str) -> Result<Vec<VersionLocation>, PatchError> {
        let parsed: serde_json::Value =
            serde_json::from_str(text).map_err(|e| PatchError::ScanFailed(e.to_string()))?;

        let mut locations = Vec::new();

        for &(section, section_key) in DEPENDENCY_SECTIONS {
            if let Some(serde_json::Value::Object(deps)) = parsed.get(section_key) {
                // Find the byte position of this section key in the text
                let Some(section_key_pos) = find_json_key_position(text, section_key, 0) else {
                    continue;
                };

                // Find the opening { of the section's value object
                let search_from = section_key_pos + section_key.len() + 2; // skip past `"key"`
                let Some(obj_start) = find_char_skipping_strings(text, '{', search_from) else {
                    continue;
                };

                // Find the matching closing }
                let Some(obj_end) = find_matching_brace(text, obj_start) else {
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
        sorted.sort_by(|a, b| b.start.cmp(&a.start));

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
fn find_char_skipping_whitespace(text: &str, ch: char, from: usize) -> Option<usize> {
    for (i, c) in text[from..].char_indices() {
        if c == ch {
            return Some(from + i);
        }
        if !c.is_whitespace() {
            return None;
        }
    }
    None
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
            current_value: version_str.to_owned(),
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(locations[0].current_value, "^17.0.0");

        let patches = vec![Patch {
            start: locations[0].value_start,
            end: locations[0].value_end,
            new_value: "^18.2.0".to_owned(),
        }];

        let result = JsonPatcher::apply_patches(input, &patches).unwrap();
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
        let input = "{\n  \"dependencies\": {\n    \"react\": \"^17.0.0\"\n  }\n}\n";
        let locations = JsonPatcher::scan_version_locations(input).unwrap();
        let patches = vec![Patch {
            start: locations[0].value_start,
            end: locations[0].value_end,
            new_value: "^18.2.0".to_owned(),
        }];
        let result = JsonPatcher::apply_patches(input, &patches).unwrap();

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

    #[test]
    fn test_4space_indent_preserved() {
        let input = "{\n    \"dependencies\": {\n        \"react\": \"^17.0.0\"\n    }\n}\n";
        let expected = "{\n    \"dependencies\": {\n        \"react\": \"^18.2.0\"\n    }\n}\n";

        let locations = JsonPatcher::scan_version_locations(input).unwrap();
        let patches = vec![Patch {
            start: locations[0].value_start,
            end: locations[0].value_end,
            new_value: "^18.2.0".to_owned(),
        }];
        let result = JsonPatcher::apply_patches(input, &patches).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_tab_indent_preserved() {
        let input = "{\n\t\"dependencies\": {\n\t\t\"react\": \"^17.0.0\"\n\t}\n}\n";
        let expected = "{\n\t\"dependencies\": {\n\t\t\"react\": \"^18.2.0\"\n\t}\n}\n";

        let locations = JsonPatcher::scan_version_locations(input).unwrap();
        let patches = vec![Patch {
            start: locations[0].value_start,
            end: locations[0].value_end,
            new_value: "^18.2.0".to_owned(),
        }];
        let result = JsonPatcher::apply_patches(input, &patches).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_crlf_preserved() {
        let input = "{\r\n  \"dependencies\": {\r\n    \"react\": \"^17.0.0\"\r\n  }\r\n}\r\n";
        let expected = "{\r\n  \"dependencies\": {\r\n    \"react\": \"^18.2.0\"\r\n  }\r\n}\r\n";

        let locations = JsonPatcher::scan_version_locations(input).unwrap();
        let patches = vec![Patch {
            start: locations[0].value_start,
            end: locations[0].value_end,
            new_value: "^18.2.0".to_owned(),
        }];
        let result = JsonPatcher::apply_patches(input, &patches).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_trailing_newline_preserved() {
        let input = "{\n  \"dependencies\": {\n    \"react\": \"^17.0.0\"\n  }\n}\n";
        let locations = JsonPatcher::scan_version_locations(input).unwrap();
        let patches = vec![Patch {
            start: locations[0].value_start,
            end: locations[0].value_end,
            new_value: "^18.2.0".to_owned(),
        }];
        let result = JsonPatcher::apply_patches(input, &patches).unwrap();
        assert!(result.ends_with('\n'));
    }

    #[test]
    fn test_no_trailing_newline_preserved() {
        let input = "{\n  \"dependencies\": {\n    \"react\": \"^17.0.0\"\n  }\n}";
        let locations = JsonPatcher::scan_version_locations(input).unwrap();
        let patches = vec![Patch {
            start: locations[0].value_start,
            end: locations[0].value_end,
            new_value: "^18.2.0".to_owned(),
        }];
        let result = JsonPatcher::apply_patches(input, &patches).unwrap();
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
        assert_eq!(types_react.current_value, "^18.0.0");

        let babel = locations.iter().find(|l| l.name == "@babel/core").unwrap();
        assert_eq!(babel.current_value, "^7.20.0");
    }

    #[test]
    fn test_range_prefix_preserved() {
        let input = "{\n  \"dependencies\": {\n    \"react\": \"~17.0.0\"\n  }\n}\n";

        let locations = JsonPatcher::scan_version_locations(input).unwrap();
        let patches = vec![Patch {
            start: locations[0].value_start,
            end: locations[0].value_end,
            new_value: "~18.2.0".to_owned(),
        }];
        let result = JsonPatcher::apply_patches(input, &patches).unwrap();
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

    #[test]
    fn test_find_matching_brace() {
        let text = r#"{ "a": { "b": 1 }, "c": 2 }"#;
        assert_eq!(find_matching_brace(text, 0), Some(text.len() - 1));
    }

    #[test]
    fn test_find_matching_brace_nested() {
        let text = r#"{ "a": { "b": {} } }"#;
        assert_eq!(find_matching_brace(text, 0), Some(text.len() - 1));
        assert_eq!(find_matching_brace(text, 7), Some(17));
    }

    #[test]
    fn test_version_with_different_length() {
        // Version string changes length: "^1.0.0" -> "^10.0.0"
        let input = "{\n  \"dependencies\": {\n    \"react\": \"^1.0.0\"\n  }\n}\n";

        let locations = JsonPatcher::scan_version_locations(input).unwrap();
        let patches = vec![Patch {
            start: locations[0].value_start,
            end: locations[0].value_end,
            new_value: "^10.0.0".to_owned(),
        }];
        let result = JsonPatcher::apply_patches(input, &patches).unwrap();
        assert!(result.contains("\"^10.0.0\""));
        // Verify it's still valid JSON
        let _: serde_json::Value = serde_json::from_str(&result).unwrap();
    }
}
