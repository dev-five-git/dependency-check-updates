/// Detected indentation style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndentStyle {
    Spaces(u8),
    Tab,
}

impl Default for IndentStyle {
    fn default() -> Self {
        Self::Spaces(2)
    }
}

/// Detected line ending style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineEnding {
    #[default]
    Lf,
    CrLf,
}

/// The detected formatting style of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStyle {
    pub indent: IndentStyle,
    pub line_ending: LineEnding,
    pub trailing_newline: bool,
}

impl Default for FileStyle {
    fn default() -> Self {
        Self {
            indent: IndentStyle::default(),
            line_ending: LineEnding::default(),
            trailing_newline: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_style_default() {
        let style = FileStyle::default();
        assert_eq!(style.indent, IndentStyle::Spaces(2));
        assert_eq!(style.line_ending, LineEnding::Lf);
        assert!(style.trailing_newline);
    }

    #[test]
    fn test_indent_style_default() {
        let indent = IndentStyle::default();
        assert_eq!(indent, IndentStyle::Spaces(2));
    }

    #[test]
    fn test_line_ending_default() {
        let ending = LineEnding::default();
        assert_eq!(ending, LineEnding::Lf);
    }
}
