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

/// Return true if `git_ref` looks like a version we want to track, rather
/// than a moving pointer or a content hash.
///
/// Rules (both must hold):
/// 1. After stripping an optional leading `v`, the first char is a digit.
/// 2. The ref is NOT a commit SHA — heuristically defined as "all hex digits,
///    length >= 7, no dots", which matches both short and full SHAs while
///    letting `v5`, `v5.1`, `v5.1.0`, `2024.01.01`, `1.0-beta` through.
///
/// Shared by the GitHub Actions ref scanner (`@main`, `@v5`,
/// `@8e5e7e5…`) and the container-tag scanner (`:latest`, `:20-alpine`,
/// `:1a2b3c4`). Both ecosystems pin against either a moving name or an
/// immutable version string, and both want the moving names left alone, so
/// the same two rules cover them: `main` and `latest` fail rule 1, build-hash
/// tags fail rule 2, and `20-alpine` passes because `l`/`p`/`i`/`n` are not
/// hex digits.
///
/// ```
/// use dependency_check_updates_core::is_version_ref;
/// assert!(is_version_ref("v5"));
/// assert!(is_version_ref("20-alpine"));
/// assert!(!is_version_ref("main"));
/// assert!(!is_version_ref("latest"));
/// assert!(!is_version_ref("8e5e7e5a3b4c1234abcdef0123456789abcdef01"));
/// ```
#[must_use]
pub fn is_version_ref(git_ref: &str) -> bool {
    let stripped = git_ref.strip_prefix('v').unwrap_or(git_ref);
    let Some(first) = stripped.chars().next() else {
        return false;
    };
    if !first.is_ascii_digit() {
        return false;
    }
    // SHA heuristic: pure hex, length >= 7, no dots. Real version tags
    // contain dots (`1.2.3`) or are very short (`v5` → stripped = `5`).
    if stripped.len() >= 7
        && !stripped.contains('.')
        && stripped.chars().all(|c| c.is_ascii_hexdigit())
    {
        return false;
    }
    true
}

/// Count non-empty dot-separated segments in the numeric head of a version.
#[must_use]
pub fn count_numeric_segments(v: &str) -> usize {
    split_numeric_head(v)
        .0
        .split('.')
        .filter(|s| !s.is_empty())
        .count()
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
    // A non-empty string with NO numeric head has nothing to pad, and must be
    // handed back untouched for the caller's parser to reject. Padding it
    // would fabricate a version out of a branch name.
    #[case("main", "main")]
    #[case("v5", "v5")]
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

    #[rstest]
    // v-prefix versions accepted as version-like.
    #[case::v_major("v5", true)]
    #[case::v_major_minor("v5.1", true)]
    #[case::v_major_minor_patch("v5.1.0", true)]
    #[case::v_prerelease("v1.0.0-beta.1", true)]
    // Bare numeric versions accepted (with or without v prefix).
    #[case::bare_major("5", true)]
    #[case::bare_semver("1.2.3", true)]
    #[case::calendar_version("2024.01.01", true)]
    // Short v-versions: `v12345` strips to `12345` (5 chars, < 7) so it
    // bypasses the SHA heuristic and is treated as a version.
    #[case::v_short_numeric("v12345", true)]
    // Container tag variants: the alphabetic suffix breaks the all-hex test,
    // so `20-alpine` stays version-like despite being 9 chars with no dot.
    #[case::container_variant_tag("20-alpine", true)]
    #[case::container_variant_dotted("3.12-slim-bookworm", true)]
    // Moving pointers are rejected (no leading digit).
    #[case::branch_main("main", false)]
    #[case::branch_master("master", false)]
    #[case::branch_develop("develop", false)]
    #[case::branch_release_with_slash("release/v5", false)]
    #[case::container_latest("latest", false)]
    #[case::container_codename("bookworm", false)]
    // Commit SHAs / build hashes are rejected by the hex+length heuristic.
    #[case::sha_40_char("8e5e7e5a3b4c1234abcdef0123456789abcdef01", false)]
    #[case::sha_7_char_starting_digit("1234567", false)]
    #[case::sha_8_char_mixed_hex("12345abc", false)]
    // Empty / lone `v` produce no leading digit → rejected.
    #[case::empty("", false)]
    #[case::just_v("v", false)]
    fn is_version_ref_cases(#[case] input: &str, #[case] expected: bool) {
        assert_eq!(is_version_ref(input), expected);
    }

    #[rstest]
    #[case::empty("", 0)]
    #[case::simple("5", 1)]
    #[case::dotted("1.2.3", 3)]
    #[case::prefix("v5", 0)]
    #[case::suffix("1.2.3-beta.1", 3)]
    fn count_numeric_segments_cases(#[case] input: &str, #[case] expected: usize) {
        assert_eq!(count_numeric_segments(input), expected);
    }
}
