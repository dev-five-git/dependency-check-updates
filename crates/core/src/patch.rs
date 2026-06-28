//! Shared byte-range patch primitive used by every text-based ecosystem patcher.
//!
//! Both `crates/github` (workflow YAML) and `crates/node` (`package.json`)
//! previously redeclared the same [`Patch`] struct and the same overlap-checked
//! descending-`replace_range` loop. Centralising the algorithm here keeps the
//! one place where format-preserving updates touch raw bytes a single source of
//! truth; future text ecosystems (additional YAML flavours, INI, etc.) can lean
//! on the same primitive instead of copy-pasting a third time.
//!
//! The primitive only knows about byte offsets — semantic re-validation (e.g.
//! re-parsing the resulting text as JSON in `crates/node`) stays in the
//! ecosystem-specific wrappers.

/// A patch: replace bytes `[start..end)` with `new_value`.
///
/// `start` and `end` are byte offsets into the original text and must lie on
/// `char` boundaries for `String::replace_range` to accept them.
#[derive(Debug, Clone)]
pub struct Patch {
    /// Inclusive byte offset of the first byte to replace.
    pub start: usize,
    /// Exclusive byte offset just past the last byte to replace.
    pub end: usize,
    /// Replacement text inserted in place of `[start..end)`.
    pub new_value: String,
}

/// Errors returned by [`apply_byte_patches`].
#[derive(Debug, thiserror::Error)]
pub enum PatchError {
    /// Two patches resolved to overlapping byte ranges. Should not happen in
    /// practice — the per-ecosystem scanners emit disjoint spans — but the
    /// check is cheap and prevents silent corruption.
    #[error("overlapping patches detected")]
    OverlappingPatches,
}

/// Apply raw byte-range patches to `original`.
///
/// Patches are applied from highest to lowest byte offset so each replacement
/// leaves the offsets of later (i.e. earlier-in-the-list) patches intact.
///
/// # Errors
///
/// Returns [`PatchError::OverlappingPatches`] if any two patches touch the
/// same byte range — a sentinel for upstream scanner bugs.
pub fn apply_byte_patches(original: &str, patches: &[Patch]) -> Result<String, PatchError> {
    if patches.is_empty() {
        return Ok(original.to_owned());
    }

    let mut sorted: Vec<&Patch> = patches.iter().collect();
    // Unstable sort is safe here: two patches with the same `start` are
    // necessarily overlapping (every patch has `end > start`), so the
    // immediately-following overlap check rejects the only case where
    // stable-vs-unstable ordering would be observable. The stable sort's
    // auxiliary-array allocation and slightly larger constant factor buy
    // nothing in that scenario.
    sorted.sort_unstable_by_key(|p| std::cmp::Reverse(p.start));

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

    #[test]
    fn empty_patch_list_is_identity() {
        let text = "hello world";
        let result = apply_byte_patches(text, &[]).unwrap();
        assert_eq!(result, text);
    }

    #[test]
    fn single_patch_replaces_range() {
        let text = "hello world";
        let patches = vec![Patch {
            start: 6,
            end: 11,
            new_value: "rust!".to_owned(),
        }];
        let result = apply_byte_patches(text, &patches).unwrap();
        assert_eq!(result, "hello rust!");
    }

    #[test]
    fn multiple_disjoint_patches_apply_back_to_front() {
        // Length-changing patches in arbitrary order: the descending
        // application order preserves the lower-offset patch's positions.
        let text = "a-bb-ccc";
        let patches = vec![
            Patch {
                start: 0,
                end: 1,
                new_value: "AAAA".to_owned(),
            },
            Patch {
                start: 5,
                end: 8,
                new_value: "C".to_owned(),
            },
            Patch {
                start: 2,
                end: 4,
                new_value: "BB".to_owned(),
            },
        ];
        let result = apply_byte_patches(text, &patches).unwrap();
        assert_eq!(result, "AAAA-BB-C");
    }

    #[test]
    fn overlapping_patches_error() {
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
        let result = apply_byte_patches("abcdefghijk", &patches);
        assert!(matches!(result, Err(PatchError::OverlappingPatches)));
    }
}
