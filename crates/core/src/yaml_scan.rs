//! Line-based YAML scalar location.
//!
//! Several manifests this tool edits are YAML that humans read far more often
//! than machines do: GitHub workflows, composite actions, Compose files. Round
//! -tripping them through a YAML emitter strips comments, blank lines, and
//! anchor formatting, so instead every YAML-backed ecosystem scans line by
//! line and patches the exact bytes of the value it wants to change.
//!
//! The "find `key:` and work out where its scalar value starts and ends" step
//! is identical for every such key — `uses:` in a workflow, `image:` in a
//! Compose service or a workflow job container — so it lives here once.

/// Locate the scalar value of `key` on a single YAML `line`.
///
/// `key` must include its trailing colon (`"uses:"`, `"image:"`). Returns the
/// `(start, end)` byte offsets **within `line`** of the value, with any
/// surrounding quotes and any trailing `# comment` excluded, so
/// `&line[start..end]` is exactly the scalar the user wrote.
///
/// Returns `None` when the line does not carry `key` as an actual mapping key
/// — the text may appear inside a comment (`# uses: foo/bar@v1`), inside
/// another key (`myimage: …`), or inside a scalar value
/// (`description: This uses: pattern`) — or when the key has no value at all.
///
/// ```
/// use dependency_check_updates_core::scalar_value_bounds;
/// let line = "      - uses: actions/checkout@v5  # pinned\n";
/// let (start, end) = scalar_value_bounds(line, "uses:").unwrap();
/// assert_eq!(&line[start..end], "actions/checkout@v5");
/// ```
#[must_use]
pub fn scalar_value_bounds(line: &str, key: &str) -> Option<(usize, usize)> {
    let key_pos = line.find(key)?;

    // Verify everything before the key is YAML key context (whitespace +
    // optional single `-` list-item marker). Anything else — including a
    // leading `#` comment, or the tail of a longer key — disqualifies the line.
    if !is_key_context(&line[..key_pos]) {
        return None;
    }

    let after_colon = key_pos + key.len();
    let rest = line.get(after_colon..)?;
    let leading_ws = rest.find(|c: char| !c.is_whitespace())?;
    let value_start = after_colon + leading_ws;
    let value_str = line.get(value_start..)?;

    let first = value_str.chars().next()?;
    if first == '\'' || first == '"' {
        let close_rel = value_str.get(1..)?.find(first)?;
        return Some((value_start + 1, value_start + 1 + close_rel));
    }
    // Unquoted scalar: terminate at the first whitespace or YAML comment marker.
    let end_rel = value_str
        .find(|c: char| c == '#' || c.is_whitespace())
        .unwrap_or(value_str.len());
    Some((value_start, value_start + end_rel))
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

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    // Plain mapping value, with and without a list-item marker.
    #[case::list_item(
        "      - uses: actions/checkout@v5\n",
        "uses:",
        Some("actions/checkout@v5")
    )]
    #[case::plain_key(
        "      uses: actions/checkout@v5\n",
        "uses:",
        Some("actions/checkout@v5")
    )]
    #[case::top_level("image: nginx:1.27\n", "image:", Some("nginx:1.27"))]
    // Trailing comments and CRLF endings must not bleed into the value.
    #[case::trailing_comment("  image: nginx:1.27  # pinned\n", "image:", Some("nginx:1.27"))]
    #[case::comment_no_space("  image: nginx:1.27# pinned\n", "image:", Some("nginx:1.27"))]
    #[case::crlf("  image: nginx:1.27\r\n", "image:", Some("nginx:1.27"))]
    #[case::no_trailing_newline("  image: nginx:1.27", "image:", Some("nginx:1.27"))]
    // Quotes are stripped from both spellings.
    #[case::single_quoted("  image: 'nginx:1.27'\n", "image:", Some("nginx:1.27"))]
    #[case::double_quoted("  image: \"nginx:1.27\"\n", "image:", Some("nginx:1.27"))]
    // Deeply indented Compose service key.
    #[case::nested("services:\n", "image:", None)]
    // The key appears, but not as a key.
    #[case::inside_comment("      # uses: foo/bar@v1\n", "uses:", None)]
    #[case::inside_longer_key("      myimage: nginx:1.27\n", "image:", None)]
    #[case::inside_scalar_value("      description: This uses: pattern\n", "uses:", None)]
    // Key present but valueless.
    #[case::empty_value("      - uses:\n", "uses:", None)]
    #[case::key_absent("      run: echo hi\n", "image:", None)]
    fn scalar_value_bounds_cases(
        #[case] line: &str,
        #[case] key: &str,
        #[case] expected: Option<&str>,
    ) {
        let bounds = scalar_value_bounds(line, key);
        match expected {
            Some(value) => {
                let (start, end) = bounds.expect("value must be located");
                assert_eq!(&line[start..end], value);
            }
            None => assert!(bounds.is_none(), "expected no match, got {bounds:?}"),
        }
    }

    #[rstest]
    #[case::empty("", true)]
    #[case::spaces("    ", true)]
    #[case::single_dash("  - ", true)]
    #[case::tab_indent("\t\t", true)]
    #[case::two_dashes("  - - ", false)]
    #[case::comment_marker("  # ", false)]
    #[case::trailing_word("  foo", false)]
    fn is_key_context_cases(#[case] input: &str, #[case] expected: bool) {
        assert_eq!(is_key_context(input), expected);
    }
}
