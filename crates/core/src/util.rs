//! Small cross-ecosystem helpers shared by the registry clients.
//!
//! These were previously copy-pasted byte-for-byte into each ecosystem crate
//! (npm, crates.io, `PyPI`). Centralising them keeps the version-string
//! handling in one place.

use std::borrow::Cow;

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

/// Pad a numeric version prefix to exactly three segments while preserving any
/// pre-release / build-metadata suffix.
///
/// The input is treated as `<numeric-prefix><suffix>`; the prefix is split on
/// `.`, empty segments are skipped, and the result is rebuilt with `.0` filled
/// in. Inputs with 0 or >= 3 numeric segments are returned unchanged (callers
/// pass these through `semver::Version::parse` / `node_semver::Version::parse`,
/// which decide whether they are accepted).
///
/// ```
/// use dependency_check_updates_core::pad_to_three_segments;
/// assert_eq!(pad_to_three_segments("5"), "5.0.0");
/// assert_eq!(pad_to_three_segments("5.1"), "5.1.0");
/// assert_eq!(pad_to_three_segments("5.1.0"), "5.1.0");
/// assert_eq!(pad_to_three_segments("1.2-beta"), "1.2.0-beta");
/// assert_eq!(pad_to_three_segments(""), "");
/// ```
#[must_use]
pub fn pad_to_three_segments(v: &str) -> Cow<'_, str> {
    if v.is_empty() {
        return Cow::Borrowed(v);
    }
    let (numeric, suffix) = v
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .map_or((v, ""), |i| v.split_at(i));
    let parts: Vec<&str> = numeric.split('.').filter(|s| !s.is_empty()).collect();
    match parts.len() {
        1 => Cow::Owned(format!("{}.0.0{}", parts[0], suffix)),
        2 => Cow::Owned(format!("{}.{}.0{}", parts[0], parts[1], suffix)),
        // 0 (no numeric prefix) or >= 3 (already padded / over-padded): leave
        // as-is (zero-cost borrow). Callers' parsers decide whether to accept
        // the result.
        _ => Cow::Borrowed(v),
    }
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

    #[rstest]
    #[case("5", "5.0.0")]
    #[case("5.1", "5.1.0")]
    #[case("5.1.0", "5.1.0")]
    #[case("5.1.2.3", "5.1.2.3")] // 4+ segments left as-is
    #[case("5.1.0-rc.1", "5.1.0-rc.1")]
    #[case("1.2-beta", "1.2.0-beta")]
    #[case("5-beta", "5.0.0-beta")]
    #[case("", "")]
    fn pad_to_three_segments_cases(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(pad_to_three_segments(input), expected);
    }
}
