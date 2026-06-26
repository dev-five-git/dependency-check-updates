//! Byte-range patcher for workflow YAML files.
//!
//! Like the JSON patcher in `crates/node`, this replaces only the bytes that
//! correspond to a version ref. Comments, indentation, anchors, blank lines,
//! and any unrelated `uses:` directives (e.g. ones pinned to `@main` or a
//! commit SHA) survive untouched.
//!
//! The actual descending-`replace_range` engine lives in
//! [`dependency_check_updates_core::patch`]; this module only handles the
//! workflow-specific scan-and-match step that turns a list of
//! [`PlannedUpdate`]s into byte-range [`Patch`]es.

use dependency_check_updates_core::PlannedUpdate;
use dependency_check_updates_core::patch::{Patch, PatchError, apply_byte_patches};

use crate::parser::scan;

/// Format-preserving workflow patcher.
pub struct WorkflowPatcher;

impl WorkflowPatcher {
    /// Apply `updates` to `text` and return the patched text.
    ///
    /// Updates are matched to locations by `(name, from_ref)` because a single
    /// workflow can call the same action twice with different refs (rare but
    /// legal: e.g. canary vs stable steps), and we must update each
    /// occurrence using its own original ref as the join key.
    ///
    /// # Errors
    ///
    /// Returns [`PatchError::OverlappingPatches`] if two patches would touch
    /// the same byte range. Indicates a parser bug, not user error.
    pub fn apply(text: &str, updates: &[PlannedUpdate]) -> Result<String, PatchError> {
        if updates.is_empty() {
            return Ok(text.to_owned());
        }

        let locations = scan(text);

        // Build patches by matching (name, from) against scanned locations.
        // We consume locations as we go so duplicate (name, from) pairs in the
        // file each get their own patch.
        let mut consumed = vec![false; locations.len()];
        let mut patches: Vec<Patch> = Vec::with_capacity(updates.len());

        for update in updates {
            let Some((idx, loc)) = locations.iter().enumerate().find(|(i, l)| {
                !consumed[*i] && l.name == update.name && l.current_ref == update.from
            }) else {
                // Unmatched updates are silently skipped — the dep may have
                // been pinned to a non-version ref by the user since the scan
                // that produced this update list ran.
                continue;
            };
            consumed[idx] = true;
            patches.push(Patch {
                start: loc.ref_start,
                end: loc.ref_end,
                new_value: update.to.clone(),
            });
        }

        apply_byte_patches(text, &patches)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dependency_check_updates_core::DependencySection;
    use rstest::rstest;

    fn upd(name: &str, from: &str, to: &str) -> PlannedUpdate {
        PlannedUpdate {
            name: name.to_owned(),
            section: DependencySection::GitHubActions,
            from: from.to_owned(),
            to: to.to_owned(),
        }
    }

    fn make_updates(rows: &[(&str, &str, &str)]) -> Vec<PlannedUpdate> {
        rows.iter().map(|(n, f, t)| upd(n, f, t)).collect()
    }

    #[rstest]
    // No updates at all — the patcher must return the original text byte-for-byte.
    #[case::empty_updates_is_identity(
        "      - uses: actions/checkout@v4\n",
        &[],
        "      - uses: actions/checkout@v4\n"
    )]
    // Single happy-path update.
    #[case::single_update(
        "      - uses: actions/checkout@v4\n",
        &[("actions/checkout", "v4", "v5")],
        "      - uses: actions/checkout@v5\n"
    )]
    // Trailing `# pinned` comment must survive untouched.
    #[case::preserves_comment(
        "      - uses: actions/checkout@v4  # pinned\n",
        &[("actions/checkout", "v4", "v5")],
        "      - uses: actions/checkout@v5  # pinned\n"
    )]
    // Surrounding single quotes must survive untouched.
    #[case::preserves_quotes(
        "      - uses: 'actions/checkout@v4'\n",
        &[("actions/checkout", "v4", "v5")],
        "      - uses: 'actions/checkout@v5'\n"
    )]
    // Unrelated `@main` ref next to the updated one must be left alone.
    #[case::leaves_branch_refs_alone(
        concat!(
            "      - uses: actions/checkout@v4\n",
            "      - uses: changepacks/action@main\n",
        ),
        &[("actions/checkout", "v4", "v5")],
        concat!(
            "      - uses: actions/checkout@v5\n",
            "      - uses: changepacks/action@main\n",
        )
    )]
    // Two independent updates on separate lines.
    #[case::multiple_updates(
        concat!(
            "      - uses: actions/checkout@v4\n",
            "      - uses: actions/setup-node@v3\n",
        ),
        &[
            ("actions/checkout", "v4", "v5"),
            ("actions/setup-node", "v3", "v4"),
        ],
        concat!(
            "      - uses: actions/checkout@v5\n",
            "      - uses: actions/setup-node@v4\n",
        )
    )]
    // v4 → v10.0.0 changes ref length from 2 to 7 bytes.
    #[case::handles_ref_length_change(
        "      - uses: actions/checkout@v4\n",
        &[("actions/checkout", "v4", "v10.0.0")],
        "      - uses: actions/checkout@v10.0.0\n"
    )]
    // `from` doesn't match what's in the text → no replacement, no error.
    #[case::unmatched_update_silently_skipped(
        "      - uses: actions/checkout@v4\n",
        &[("actions/checkout", "v3", "v5")],
        "      - uses: actions/checkout@v4\n"
    )]
    // Same action twice with different refs — each instance updates using
    // its own `from` as the join key.
    #[case::duplicate_dep_each_gets_own_patch(
        concat!(
            "      - uses: actions/checkout@v3\n",
            "      - uses: actions/checkout@v4\n",
        ),
        &[
            ("actions/checkout", "v3", "v4"),
            ("actions/checkout", "v4", "v5"),
        ],
        concat!(
            "      - uses: actions/checkout@v4\n",
            "      - uses: actions/checkout@v5\n",
        )
    )]
    // End-to-end patching of v-less refs (some action tags publish bare semver).
    #[case::bare_semver_no_v_prefix(
        "      - uses: actions/checkout@1.2.3\n",
        &[("actions/checkout", "1.2.3", "4.5.6")],
        "      - uses: actions/checkout@4.5.6\n"
    )]
    fn workflow_patcher_apply_cases(
        #[case] text: &str,
        #[case] updates: &[(&str, &str, &str)],
        #[case] expected: &str,
    ) {
        let result = WorkflowPatcher::apply(text, &make_updates(updates)).unwrap();
        assert_eq!(result, expected);
    }
}
