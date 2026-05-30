//! Byte-range patcher for workflow YAML files.
//!
//! Like the JSON patcher in `crates/node`, this replaces only the bytes that
//! correspond to a version ref. Comments, indentation, anchors, blank lines,
//! and any unrelated `uses:` directives (e.g. ones pinned to `@main` or a
//! commit SHA) survive untouched.

use dependency_check_updates_core::PlannedUpdate;

use crate::parser::scan;

/// Errors returned by the patcher.
#[derive(Debug, thiserror::Error)]
pub enum PatchError {
    /// Two updates resolved to overlapping byte ranges. Should not happen in
    /// practice — each `uses:` ref occupies a distinct byte range — but the
    /// check is cheap and prevents silent corruption.
    #[error("overlapping patches detected")]
    OverlappingPatches,
}

/// A patch: replace bytes `[start..end)` with `new_value`.
#[derive(Debug, Clone)]
pub struct Patch {
    pub start: usize,
    pub end: usize,
    pub new_value: String,
}

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

        apply_patches(text, &patches)
    }
}

/// Apply raw byte-range patches to `original`.
///
/// Patches are applied from highest to lowest byte offset so each replacement
/// leaves the offsets of later (i.e. earlier-in-the-list) patches intact.
fn apply_patches(original: &str, patches: &[Patch]) -> Result<String, PatchError> {
    if patches.is_empty() {
        return Ok(original.to_owned());
    }

    let mut sorted: Vec<&Patch> = patches.iter().collect();
    sorted.sort_by_key(|p| std::cmp::Reverse(p.start));

    for window in sorted.windows(2) {
        // sorted descending: window[0].start >= window[1].start, so window[1]
        // (the lower-start patch) must end at-or-before window[0] starts.
        if window[1].end > window[0].start {
            return Err(PatchError::OverlappingPatches);
        }
    }

    let mut result = original.to_owned();
    for patch in &sorted {
        result.replace_range(patch.start..patch.end, &patch.new_value);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dependency_check_updates_core::DependencySection;

    fn upd(name: &str, from: &str, to: &str) -> PlannedUpdate {
        PlannedUpdate {
            name: name.to_owned(),
            section: DependencySection::GitHubActions,
            from: from.to_owned(),
            to: to.to_owned(),
        }
    }

    #[test]
    fn test_apply_empty_updates_is_identity() {
        let text = "      - uses: actions/checkout@v4\n";
        let result = WorkflowPatcher::apply(text, &[]).unwrap();
        assert_eq!(result, text);
    }

    #[test]
    fn test_apply_single_update() {
        let text = "      - uses: actions/checkout@v4\n";
        let expected = "      - uses: actions/checkout@v5\n";
        let result = WorkflowPatcher::apply(text, &[upd("actions/checkout", "v4", "v5")]).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_apply_preserves_comment() {
        let text = "      - uses: actions/checkout@v4  # pinned\n";
        let expected = "      - uses: actions/checkout@v5  # pinned\n";
        let result = WorkflowPatcher::apply(text, &[upd("actions/checkout", "v4", "v5")]).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_apply_preserves_quotes() {
        let text = "      - uses: 'actions/checkout@v4'\n";
        let expected = "      - uses: 'actions/checkout@v5'\n";
        let result = WorkflowPatcher::apply(text, &[upd("actions/checkout", "v4", "v5")]).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_apply_leaves_branch_refs_alone() {
        let text = concat!(
            "      - uses: actions/checkout@v4\n",
            "      - uses: changepacks/action@main\n",
        );
        let expected = concat!(
            "      - uses: actions/checkout@v5\n",
            "      - uses: changepacks/action@main\n",
        );
        let result = WorkflowPatcher::apply(text, &[upd("actions/checkout", "v4", "v5")]).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_apply_multiple_updates() {
        let text = concat!(
            "      - uses: actions/checkout@v4\n",
            "      - uses: actions/setup-node@v3\n",
        );
        let expected = concat!(
            "      - uses: actions/checkout@v5\n",
            "      - uses: actions/setup-node@v4\n",
        );
        let updates = vec![
            upd("actions/checkout", "v4", "v5"),
            upd("actions/setup-node", "v3", "v4"),
        ];
        let result = WorkflowPatcher::apply(text, &updates).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_apply_handles_ref_length_change() {
        // v4 -> v10.0.0 (length changes from 2 to 7 bytes)
        let text = "      - uses: actions/checkout@v4\n";
        let expected = "      - uses: actions/checkout@v10.0.0\n";
        let result =
            WorkflowPatcher::apply(text, &[upd("actions/checkout", "v4", "v10.0.0")]).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_apply_unmatched_update_silently_skipped() {
        // `from` doesn't match what's in the text → no replacement, no error.
        let text = "      - uses: actions/checkout@v4\n";
        let result = WorkflowPatcher::apply(text, &[upd("actions/checkout", "v3", "v5")]).unwrap();
        assert_eq!(result, text);
    }

    #[test]
    fn test_apply_duplicate_dep_each_gets_own_patch() {
        // Same action appearing twice with different refs — each instance is
        // updated independently using its own `from` as the key.
        let text = concat!(
            "      - uses: actions/checkout@v3\n",
            "      - uses: actions/checkout@v4\n",
        );
        let expected = concat!(
            "      - uses: actions/checkout@v4\n",
            "      - uses: actions/checkout@v5\n",
        );
        let updates = vec![
            upd("actions/checkout", "v3", "v4"),
            upd("actions/checkout", "v4", "v5"),
        ];
        let result = WorkflowPatcher::apply(text, &updates).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_overlapping_patches_error() {
        let patches = vec![
            Patch {
                start: 0,
                end: 5,
                new_value: "a".to_owned(),
            },
            Patch {
                start: 3,
                end: 10,
                new_value: "b".to_owned(),
            },
        ];
        let result = apply_patches("abcdefghijk", &patches);
        assert!(result.is_err());
    }

    #[test]
    fn test_apply_bare_semver_no_v_prefix() {
        // Validate end-to-end patching of v-less refs.
        let text = "      - uses: actions/checkout@1.2.3\n";
        let expected = "      - uses: actions/checkout@4.5.6\n";
        let result =
            WorkflowPatcher::apply(text, &[upd("actions/checkout", "1.2.3", "4.5.6")]).unwrap();
        assert_eq!(result, expected);
    }
}
