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

/// Split `v` between its numeric head (ASCII digits + `.`) and the rest.
///
/// Returns `(numeric, rest)`. Borrow-only; no allocation. Used by every site
/// that needs to find where the bare numeric prefix of a version string ends
/// (`1.2.3-beta` → `("1.2.3", "-beta")`, `v5` → `("", "v5")`, `5` →
/// `("5", "")`). Centralises the predicate previously duplicated across
/// `pad_to_three_segments`, `cli::pipeline::{count_version_segments,
/// truncate_version}`, and `github::registry::{tag_numeric_str, ref_precision,
/// pick_existing_ref}` so future tightenings (Unicode digit handling, treating
/// `+` build-metadata bytes as part of the head, etc.) land in one place.
///
/// ```
/// use dependency_check_updates_core::split_numeric_head;
/// assert_eq!(split_numeric_head("1.2.3"), ("1.2.3", ""));
/// assert_eq!(split_numeric_head("1.2.3-beta"), ("1.2.3", "-beta"));
/// assert_eq!(split_numeric_head("1.2.3+build"), ("1.2.3", "+build"));
/// assert_eq!(split_numeric_head("v5"), ("", "v5"));
/// assert_eq!(split_numeric_head(""), ("", ""));
/// ```
#[must_use]
pub fn split_numeric_head(v: &str) -> (&str, &str) {
    let i = v
        .bytes()
        .position(|b| !(b.is_ascii_digit() || b == b'.'))
        .unwrap_or(v.len());
    v.split_at(i)
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
    let (numeric, suffix) = split_numeric_head(v);
    // Walk the segment iterator directly instead of collecting into a
    // throwaway `Vec<&str>`: this helper sits on per-tag / per-dependency hot
    // paths (`github::registry::normalize_tag`, `cli::pipeline::compute_updates`),
    // so every call previously allocated a small `Vec` just to read its
    // length and the first one or two elements.
    let mut parts = numeric.split('.').filter(|s| !s.is_empty());
    let Some(p0) = parts.next() else {
        // 0 numeric segments — no padding possible.
        return Cow::Borrowed(v);
    };
    let Some(p1) = parts.next() else {
        // 1 segment: pad to `<p0>.0.0<suffix>`.
        return Cow::Owned(format!("{p0}.0.0{suffix}"));
    };
    if parts.next().is_none() {
        // 2 segments: pad to `<p0>.<p1>.0<suffix>`.
        return Cow::Owned(format!("{p0}.{p1}.0{suffix}"));
    }
    // 3+ segments: already padded / over-padded; leave as-is (zero-cost
    // borrow). Callers' parsers decide whether to accept the result.
    Cow::Borrowed(v)
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

    #[rstest]
    // Pure numeric — entire string is the head.
    #[case::pure_numeric_three("1.2.3", "1.2.3", "")]
    #[case::pure_numeric_two("1.2", "1.2", "")]
    #[case::pure_numeric_one("5", "5", "")]
    // Pre-release tail starts at `-`.
    #[case::pre_release("1.2.3-beta.1", "1.2.3", "-beta.1")]
    #[case::pre_release_short("1.2-beta", "1.2", "-beta")]
    // Build-metadata tail starts at `+`.
    #[case::build_metadata("1.2.3+build.7", "1.2.3", "+build.7")]
    // Leading non-digit (e.g. `v5` GitHub tag) → empty head.
    #[case::leading_non_digit("v5", "", "v5")]
    #[case::all_non_digit("main", "", "main")]
    // Empty input → empty halves.
    #[case::empty("", "", "")]
    fn split_numeric_head_cases(
        #[case] input: &str,
        #[case] expected_numeric: &str,
        #[case] expected_rest: &str,
    ) {
        assert_eq!(split_numeric_head(input), (expected_numeric, expected_rest));
    }
}
