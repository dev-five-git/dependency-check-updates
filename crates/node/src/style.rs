use dependency_check_updates_core::{FileStyle, IndentStyle, LineEnding};

/// Detects file formatting style from raw text.
pub struct StyleDetector;

impl StyleDetector {
    /// Detect the formatting style of a JSON file from its raw text.
    #[must_use]
    pub fn detect(text: &str) -> FileStyle {
        FileStyle {
            indent: Self::detect_indent(text),
            line_ending: Self::detect_line_ending(text),
            trailing_newline: text.ends_with('\n'),
        }
    }

    fn detect_indent(text: &str) -> IndentStyle {
        // Strategy: look at lines that start with whitespace
        // Count occurrences of each indent pattern
        // The most common one wins

        let mut space_2 = 0u32;
        let mut space_4 = 0u32;
        let mut tab = 0u32;

        for line in text.lines() {
            let trimmed = line.trim_start();
            if trimmed.is_empty() || trimmed == line {
                continue; // skip empty lines and non-indented lines
            }

            let indent = &line[..line.len() - trimmed.len()];

            if indent.starts_with('\t') {
                tab += 1;
            } else if indent.starts_with("    ") {
                space_4 += 1;
            } else if indent.starts_with("  ") {
                space_2 += 1;
            }
        }

        if tab > space_2 && tab > space_4 {
            IndentStyle::Tab
        } else if space_4 > space_2 {
            IndentStyle::Spaces(4)
        } else if space_2 > 0 {
            IndentStyle::Spaces(2)
        } else {
            // Default to 2 spaces if no indent detected
            IndentStyle::default()
        }
    }

    fn detect_line_ending(text: &str) -> LineEnding {
        // Count CRLF vs LF occurrences
        let crlf_count = text.matches("\r\n").count();
        let lf_only_count = text.matches('\n').count().saturating_sub(crlf_count);

        if crlf_count > lf_only_count {
            LineEnding::CrLf
        } else {
            LineEnding::Lf
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_indent_2_spaces() {
        let json = r#"{
  "name": "test",
  "dependencies": {
    "react": "^18.0.0"
  }
}"#;
        let style = StyleDetector::detect(json);
        assert_eq!(style.indent, IndentStyle::Spaces(2));
    }

    #[test]
    fn test_detect_indent_4_spaces() {
        let json = r#"{
    "name": "test",
    "dependencies": {
        "react": "^18.0.0"
    }
}"#;
        let style = StyleDetector::detect(json);
        assert_eq!(style.indent, IndentStyle::Spaces(4));
    }

    #[test]
    fn test_detect_indent_tab() {
        let json =
            "{\n\t\"name\": \"test\",\n\t\"dependencies\": {\n\t\t\"react\": \"^18.0.0\"\n\t}\n}";
        let style = StyleDetector::detect(json);
        assert_eq!(style.indent, IndentStyle::Tab);
    }

    #[test]
    fn test_detect_line_ending_lf() {
        let json = "{\n  \"name\": \"test\"\n}";
        let style = StyleDetector::detect(json);
        assert_eq!(style.line_ending, LineEnding::Lf);
    }

    #[test]
    fn test_detect_line_ending_crlf() {
        let json = "{\r\n  \"name\": \"test\"\r\n}";
        let style = StyleDetector::detect(json);
        assert_eq!(style.line_ending, LineEnding::CrLf);
    }

    #[test]
    fn test_trailing_newline_present() {
        let json = "{\n  \"name\": \"test\"\n}\n";
        let style = StyleDetector::detect(json);
        assert!(style.trailing_newline);
    }

    #[test]
    fn test_trailing_newline_absent() {
        let json = "{\n  \"name\": \"test\"\n}";
        let style = StyleDetector::detect(json);
        assert!(!style.trailing_newline);
    }

    #[test]
    fn test_detect_indent_mixed_majority_wins() {
        // Mostly 2-space with one 4-space indent
        let json = r#"{
  "a": 1,
  "b": 2,
  "c": 3,
    "d": 4,
  "e": 5
}"#;
        let style = StyleDetector::detect(json);
        assert_eq!(style.indent, IndentStyle::Spaces(2));
    }

    #[test]
    fn test_detect_indent_minified_json() {
        let json = r#"{"name":"test","dependencies":{"react":"^18.0.0"}}"#;
        let style = StyleDetector::detect(json);
        assert_eq!(style.indent, IndentStyle::Spaces(2)); // default
    }

    #[test]
    fn test_default_file_style() {
        let json = "{}";
        let style = StyleDetector::detect(json);
        assert_eq!(style.indent, IndentStyle::Spaces(2));
        assert_eq!(style.line_ending, LineEnding::Lf);
        assert!(!style.trailing_newline);
    }

    #[test]
    fn test_complex_file_with_all_detections() {
        // 2-space indent, CRLF line endings, trailing newline
        let json = "{\r\n  \"name\": \"test\",\r\n  \"version\": \"1.0.0\"\r\n}\r\n";
        let style = StyleDetector::detect(json);
        assert_eq!(style.indent, IndentStyle::Spaces(2));
        assert_eq!(style.line_ending, LineEnding::CrLf);
        assert!(style.trailing_newline);
    }
}
