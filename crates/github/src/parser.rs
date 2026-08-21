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

use dependency_check_updates_core::{
    DependencySection, DependencySpec, is_version_ref, scalar_value_bounds,
};

/// A located `uses:` directive in the workflow text.
#[derive(Debug)]
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
                path_version: None,
            })
            .collect();

        Self { dependencies }
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
    // `inner_start` is the byte offset (within `line`) of the first content
    // char; `inner_end` is one past the last (so `[inner_start, inner_end)` is
    // the value, quotes and trailing comment excluded).
    let (inner_start, inner_end) = scalar_value_bounds(line, "uses:")?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    // Yaml that produces EXACTLY one `uses:` match — name, ref, and byte
    // offsets must slice back to the recorded ref.
    #[case::basic_multiline_indent(
        "jobs:\n  test:\n    steps:\n      - uses: actions/checkout@v5\n",
        "actions/checkout",
        "v5"
    )]
    #[case::trailing_comment(
        "      - uses: actions/checkout@v5  # pinned\n",
        "actions/checkout",
        "v5"
    )]
    #[case::single_quoted("      - uses: 'actions/checkout@v5'\n", "actions/checkout", "v5")]
    #[case::double_quoted("      - uses: \"actions/checkout@v5\"\n", "actions/checkout", "v5")]
    #[case::no_leading_dash("      uses: actions/checkout@v5\n", "actions/checkout", "v5")]
    #[case::subdir_action(
        "      - uses: actions/checkout/sub/path@v5\n",
        "actions/checkout/sub/path",
        "v5"
    )]
    #[case::crlf_line_ending("      - uses: actions/checkout@v5\r\n", "actions/checkout", "v5")]
    #[case::bare_semver_no_v_prefix(
        "      - uses: actions/checkout@1.2.3\n",
        "actions/checkout",
        "1.2.3"
    )]
    #[case::bare_semver_major_only("      - uses: actions/checkout@5\n", "actions/checkout", "5")]
    #[case::calendar_version_supported(
        "      - uses: cal/ver@2024.01.01\n",
        "cal/ver",
        "2024.01.01"
    )]
    fn scan_yields_single_match(
        #[case] yaml: &str,
        #[case] expected_name: &str,
        #[case] expected_ref: &str,
    ) {
        let locs = scan(yaml);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].name, expected_name);
        assert_eq!(locs[0].current_ref, expected_ref);
        // Offsets must slice the original text back to the recorded ref,
        // with no `\r`/quote/comment bleed.
        assert_eq!(&yaml[locs[0].ref_start..locs[0].ref_end], expected_ref);
    }

    #[rstest]
    // Yaml the scanner intentionally rejects (no `uses:` match emitted).
    #[case::skips_branch_ref("      - uses: foo/bar@main\n")]
    #[case::skips_sha_ref("      - uses: foo/bar@8e5e7e5a3b4c1234abcdef0123456789abcdef01\n")]
    // The literal text "uses:" appears in a scalar value, not as a key.
    #[case::ignores_uses_as_value("      description: This uses: pattern is fine\n")]
    #[case::ignores_missing_slash("      - uses: checkout@v5\n")]
    #[case::empty_value_skipped("      - uses:\n")]
    fn scan_yields_no_matches(#[case] yaml: &str) {
        let locs = scan(yaml);
        assert!(locs.is_empty(), "expected no matches, got {locs:?}");
    }

    #[test]
    fn scan_ignores_uses_inside_comment_but_finds_real_one() {
        let yaml = "      # uses: foo/bar@v1\n      - uses: actions/checkout@v5\n";
        let locs = scan(yaml);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].name, "actions/checkout");
    }

    #[test]
    fn scan_multiple_lines() {
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
    fn scan_offsets_match_original_text() {
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
    fn parse_workflow_manifest_collects_dependency_spec() {
        let yaml = "      - uses: actions/checkout@v5\n";
        let m = WorkflowManifest::parse(yaml);
        assert_eq!(m.dependencies.len(), 1);
        assert_eq!(m.dependencies[0].name, "actions/checkout");
        assert_eq!(m.dependencies[0].current_req, "v5");
    }
}
