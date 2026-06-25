//! Small cross-ecosystem helpers shared by the registry clients.
//!
//! These were previously copy-pasted byte-for-byte into each ecosystem crate
//! (npm, crates.io, `PyPI`). Centralising them keeps the version-string
//! handling in one place.

/// Strip a leading semver range operator from a requirement string, returning
/// the bare numeric version portion.
///
/// Trims every leading character that is not an ASCII digit, so `^1.2.3`,
/// `~1.2.3`, `>=1.0.0`, and `=2.0.0` all collapse to their numeric tail. A
/// spec with no digits (e.g. `*`) yields an empty string.
///
/// ```
/// use dependency_check_updates_core::strip_range_prefix;
/// assert_eq!(strip_range_prefix("^1.2.3"), "1.2.3");
/// assert_eq!(strip_range_prefix(">=2.0.0"), "2.0.0");
/// assert_eq!(strip_range_prefix("*"), "");
/// ```
#[must_use]
pub fn strip_range_prefix(req_str: &str) -> &str {
    req_str.trim_start_matches(|c: char| !c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case::caret("^1.2.3", "1.2.3")]
    #[case::tilde("~1.0", "1.0")]
    #[case::gte(">=2.0.0", "2.0.0")]
    #[case::exact("=1.0.0", "1.0.0")]
    #[case::plain("1.0.0", "1.0.0")]
    #[case::star_yields_empty("*", "")]
    #[case::empty_input("", "")]
    fn strip_range_prefix_cases(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(strip_range_prefix(input), expected);
    }
}
