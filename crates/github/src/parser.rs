//! Line-based parser for GitHub Actions `uses:` directives.
//!
//! A full YAML parser is intentionally avoided: round-tripping YAML through any
//! emitter strips comments, blank lines, and anchor formatting, and that loss
//! is unacceptable for workflow files which are read by humans far more often
//! than by machines.
//!
//! The parser walks the text line by line, identifies every `uses:` key, splits
//! its scalar value on `@`, and records absolute byte offsets of the version
//! ref so the patcher can perform surgical substring replacement.

use dependency_check_updates_core::{DependencySection, DependencySpec};

/// A located `uses:` directive in the workflow text.
#[derive(Debug, Clone)]
pub struct UsesLocation {
    /// `owner/repo` or `owner/repo/sub/path` — preserved verbatim from the
    /// source so output matches what the user wrote.
    pub name: String,
    /// The version ref as it appears after `@` (without surrounding quotes).
    pub current_ref: String,
    /// Absolute byte offset (inclusive) of the first character of the ref.
    pub ref_start: usize,
    /// Absolute byte offset (exclusive) of one past the last character of
    /// the ref. The range `[ref_start, ref_end)` covers exactly the ref bytes,
    /// excluding any surrounding quotes or trailing whitespace/comments.
    pub ref_end: usize,
}

/// Parsed workflow manifest.
#[derive(Debug)]
pub struct WorkflowManifest {
    /// The original raw text (preserved for surgical patching).
    pub original_text: String,
    /// Version-like `uses:` refs collected as dependency specs.
    pub dependencies: Vec<DependencySpec>,
}

impl WorkflowManifest {
    /// Parse a workflow file from raw text.
    ///
    /// Infallible: malformed YAML never aborts the scan — the line-based
    /// approach simply skips lines that do not match the `uses:` pattern.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let dependencies = scan(text)
            .into_iter()
            .map(|loc| DependencySpec {
                name: loc.name,
                current_req: loc.current_ref,
                section: DependencySection::GitHubActions,
            })
            .collect();

        Self {
            original_text: text.to_owned(),
            dependencies,
        }
    }
}

/// Scan workflow text and return every `uses: owner/repo@ref` directive
/// whose ref looks like a version (per [`is_version_ref`]).
#[must_use]
pub fn scan(text: &str) -> Vec<UsesLocation> {
    let mut locations = Vec::new();
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        if let Some(loc) = scan_line(line, offset) {
            locations.push(loc);
        }
        offset += line.len();
    }
    locations
}

/// Scan a single line; returns the directive iff it parses cleanly and its
/// ref is version-like.
fn scan_line(line: &str, line_offset: usize) -> Option<UsesLocation> {
    let uses_pos = line.find("uses:")?;

    // Verify everything before `uses:` is YAML key context (whitespace +
    // optional single `-` list-item marker). Anything else — including
    // a leading `#` comment — disqualifies the line.
    if !is_key_context(&line[..uses_pos]) {
        return None;
    }

    let after_colon = uses_pos + "uses:".len();
    let rest = line.get(after_colon..)?;
    let leading_ws = rest.find(|c: char| !c.is_whitespace())?;
    let value_start_in_line = after_colon + leading_ws;
    let value_str = line.get(value_start_in_line..)?;

    // Strip optional surrounding quotes. `inner_start` is the byte offset
    // (within `line`) of the first content char; `inner_end` is one past
    // the last content char (so [inner_start, inner_end) is the value).
    let (inner_start, inner_end) = parse_scalar_bounds(value_str, value_start_in_line)?;
    let inner = line.get(inner_start..inner_end)?;

    let at_pos = inner.find('@')?;
    let name = &inner[..at_pos];
    let git_ref = &inner[at_pos + 1..];

    // Must be `owner/repo` or `owner/repo/...` — single-segment names are
    // not valid GitHub action references. (Empty `name` fails this check
    // because `"".contains('/')` is false; empty `git_ref` is rejected by
    // `is_version_ref("")` below — no separate is_empty check needed.)
    if !name.contains('/') {
        return None;
    }
    if !is_version_ref(git_ref) {
        return None;
    }

    let ref_start_byte = line_offset + inner_start + at_pos + 1;
    let ref_end_byte = line_offset + inner_end;

    Some(UsesLocation {
        name: name.to_owned(),
        current_ref: git_ref.to_owned(),
        ref_start: ref_start_byte,
        ref_end: ref_end_byte,
    })
}

/// Returns true iff `s` contains only whitespace and at most one `-` token,
/// i.e. it looks like the indent of a YAML key (possibly inside a list).
fn is_key_context(s: &str) -> bool {
    let mut seen_dash = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            continue;
        }
        if ch == '-' && !seen_dash {
            seen_dash = true;
            continue;
        }
        return false;
    }
    true
}

/// Compute the (start, end) byte offsets within `line` of the scalar value,
/// stripping optional surrounding quotes and any trailing `# comment` /
/// whitespace.
///
/// `value_start_in_line` is the byte offset within `line` where `value_str`
/// begins; this is needed because the returned offsets are absolute within
/// `line`.
fn parse_scalar_bounds(value_str: &str, value_start_in_line: usize) -> Option<(usize, usize)> {
    let first = value_str.chars().next()?;
    if first == '\'' || first == '"' {
        let close_rel = value_str.get(1..)?.find(first)?;
        let inner_start = value_start_in_line + 1;
        let inner_end = value_start_in_line + 1 + close_rel;
        return Some((inner_start, inner_end));
    }
    // Unquoted scalar: terminate at first whitespace or YAML comment marker.
    let end_rel = value_str
        .find(|c: char| c == '#' || c.is_whitespace())
        .unwrap_or(value_str.len());
    Some((value_start_in_line, value_start_in_line + end_rel))
}

/// Return true if `git_ref` looks like a version tag we want to track.
///
/// Rules (all must hold):
/// 1. After stripping an optional leading `v`, the first char is a digit.
/// 2. The ref is NOT a commit SHA — heuristically defined as "all
///    hex digits, length ≥ 7, no dots", which matches both short and full
///    SHAs while letting `v5`, `v5.1`, `v5.1.0`, `2024.01.01`, `1.0-beta`
///    through.
///
/// Refs that fail either rule (`@main`, `@master`, `@my-branch`,
/// `@8e5e7e5a3b4c1234abcdef0123456789abcdef01`) are intentionally skipped:
/// the user is opting out of automatic version pinning by referencing a
/// moving target or a content-addressed SHA.
#[must_use]
pub fn is_version_ref(git_ref: &str) -> bool {
    let stripped = git_ref.strip_prefix('v').unwrap_or(git_ref);
    let Some(first) = stripped.chars().next() else {
        return false;
    };
    if !first.is_ascii_digit() {
        return false;
    }
    // SHA heuristic: pure hex, length ≥ 7, no dots. Real version tags
    // contain dots (`1.2.3`) or are very short (`v5` → stripped = `5`).
    if stripped.len() >= 7
        && !stripped.contains('.')
        && stripped.chars().all(|c| c.is_ascii_hexdigit())
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_version_ref_v_prefix() {
        assert!(is_version_ref("v5"));
        assert!(is_version_ref("v5.1"));
        assert!(is_version_ref("v5.1.0"));
        assert!(is_version_ref("v1.0.0-beta.1"));
    }

    #[test]
    fn test_is_version_ref_bare_digits() {
        assert!(is_version_ref("5"));
        assert!(is_version_ref("1.2.3"));
        assert!(is_version_ref("2024.01.01"));
    }

    #[test]
    fn test_is_version_ref_rejects_branches() {
        assert!(!is_version_ref("main"));
        assert!(!is_version_ref("master"));
        assert!(!is_version_ref("develop"));
        assert!(!is_version_ref("release/v5"));
    }

    #[test]
    fn test_is_version_ref_rejects_shas() {
        // 40-char SHA
        assert!(!is_version_ref("8e5e7e5a3b4c1234abcdef0123456789abcdef01"));
        // 7-char short SHA starting with digit
        assert!(!is_version_ref("1234567"));
        // 8-char hex starting with digit
        assert!(!is_version_ref("12345abc"));
    }

    #[test]
    fn test_is_version_ref_short_versions_passthrough() {
        // `v5` strips to `5` (1 char, < 7) → passes SHA check.
        assert!(is_version_ref("v5"));
        // `v12345` strips to `12345` (5 chars, < 7) → version-like.
        assert!(is_version_ref("v12345"));
    }

    #[test]
    fn test_is_version_ref_rejects_empty() {
        assert!(!is_version_ref(""));
        assert!(!is_version_ref("v"));
    }

    #[test]
    fn test_scan_basic() {
        let yaml = "jobs:\n  test:\n    steps:\n      - uses: actions/checkout@v5\n";
        let locs = scan(yaml);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].name, "actions/checkout");
        assert_eq!(locs[0].current_ref, "v5");
        // Verify offsets actually point at the ref bytes.
        assert_eq!(&yaml[locs[0].ref_start..locs[0].ref_end], "v5");
    }

    #[test]
    fn test_scan_with_trailing_comment() {
        let yaml = "      - uses: actions/checkout@v5  # pinned\n";
        let locs = scan(yaml);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].current_ref, "v5");
        assert_eq!(&yaml[locs[0].ref_start..locs[0].ref_end], "v5");
    }

    #[test]
    fn test_scan_single_quoted() {
        let yaml = "      - uses: 'actions/checkout@v5'\n";
        let locs = scan(yaml);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].current_ref, "v5");
        assert_eq!(&yaml[locs[0].ref_start..locs[0].ref_end], "v5");
    }

    #[test]
    fn test_scan_double_quoted() {
        let yaml = "      - uses: \"actions/checkout@v5\"\n";
        let locs = scan(yaml);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].current_ref, "v5");
    }

    #[test]
    fn test_scan_no_leading_dash() {
        let yaml = "      uses: actions/checkout@v5\n";
        let locs = scan(yaml);
        assert_eq!(locs.len(), 1);
    }

    #[test]
    fn test_scan_subdir_action() {
        let yaml = "      - uses: actions/checkout/sub/path@v5\n";
        let locs = scan(yaml);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].name, "actions/checkout/sub/path");
        assert_eq!(locs[0].current_ref, "v5");
    }

    #[test]
    fn test_scan_skips_branch_ref() {
        let yaml = "      - uses: foo/bar@main\n";
        let locs = scan(yaml);
        assert!(locs.is_empty());
    }

    #[test]
    fn test_scan_skips_sha_ref() {
        let yaml = "      - uses: foo/bar@8e5e7e5a3b4c1234abcdef0123456789abcdef01\n";
        let locs = scan(yaml);
        assert!(locs.is_empty());
    }

    #[test]
    fn test_scan_multiple_lines() {
        let yaml = concat!(
            "jobs:\n  test:\n    steps:\n",
            "      - uses: actions/checkout@v4\n",
            "      - uses: actions/setup-node@v3\n",
            "      - uses: changepacks/action@main\n",
        );
        let locs = scan(yaml);
        assert_eq!(locs.len(), 2);
        assert_eq!(locs[0].name, "actions/checkout");
        assert_eq!(locs[1].name, "actions/setup-node");
    }

    #[test]
    fn test_scan_ignores_uses_inside_comment() {
        let yaml = "      # uses: foo/bar@v1\n      - uses: actions/checkout@v5\n";
        let locs = scan(yaml);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].name, "actions/checkout");
    }

    #[test]
    fn test_scan_ignores_uses_as_value() {
        // The literal text "uses:" appears in a scalar value, not as a key.
        let yaml = "      description: This uses: pattern is fine\n";
        let locs = scan(yaml);
        assert!(locs.is_empty());
    }

    #[test]
    fn test_scan_ignores_missing_slash() {
        let yaml = "      - uses: checkout@v5\n";
        let locs = scan(yaml);
        assert!(locs.is_empty());
    }

    #[test]
    fn test_scan_handles_crlf() {
        let yaml = "      - uses: actions/checkout@v5\r\n";
        let locs = scan(yaml);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].current_ref, "v5");
        // Slicing the original text by the recorded offsets must yield
        // exactly the ref bytes, with no `\r` bleed.
        assert_eq!(&yaml[locs[0].ref_start..locs[0].ref_end], "v5");
    }

    #[test]
    fn test_scan_offsets_match_original_text() {
        // Smoke test: every recorded location must slice the original text
        // back into the recorded `current_ref`.
        let yaml = concat!(
            "jobs:\n  a:\n    steps:\n",
            "      - uses: actions/checkout@v4\n",
            "      - uses: 'actions/setup-node@v3'\n",
            "      - uses: \"oven-sh/setup-bun@v2\"  # comment\n",
        );
        let locs = scan(yaml);
        assert_eq!(locs.len(), 3);
        for loc in &locs {
            assert_eq!(&yaml[loc.ref_start..loc.ref_end], loc.current_ref);
        }
    }

    #[test]
    fn test_parse_workflow_manifest() {
        let yaml = "      - uses: actions/checkout@v5\n";
        let m = WorkflowManifest::parse(yaml);
        assert_eq!(m.dependencies.len(), 1);
        assert_eq!(m.dependencies[0].name, "actions/checkout");
        assert_eq!(m.dependencies[0].current_req, "v5");
    }

    #[test]
    fn test_scan_empty_value_skipped() {
        let yaml = "      - uses:\n";
        let locs = scan(yaml);
        assert!(locs.is_empty());
    }

    #[test]
    fn test_scan_bare_semver_no_v_prefix() {
        // Some users / actions publish tags as plain semver without the
        // `v` prefix. Validate the full pipeline: parser recognises them as
        // version refs and exposes them for resolution.
        let yaml = "      - uses: actions/checkout@1.2.3\n";
        let locs = scan(yaml);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].name, "actions/checkout");
        assert_eq!(locs[0].current_ref, "1.2.3");
        assert_eq!(&yaml[locs[0].ref_start..locs[0].ref_end], "1.2.3");
    }

    #[test]
    fn test_scan_bare_semver_major_only() {
        let yaml = "      - uses: actions/checkout@5\n";
        let locs = scan(yaml);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].current_ref, "5");
    }

    #[test]
    fn test_scan_calendar_version_supported() {
        // CalVer tags (`2024.01.01`) are version-like and should be tracked.
        let yaml = "      - uses: cal/ver@2024.01.01\n";
        let locs = scan(yaml);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].current_ref, "2024.01.01");
    }
}
