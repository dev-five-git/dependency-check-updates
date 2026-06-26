//! Cross-ecosystem helpers for format-preserving edits of `toml_edit` docs.
//!
//! [`replace_string_preserving_decor`] was previously copy-pasted byte-for-byte
//! into both `crates/rust/src/parser.rs` (`replace_version_string_preserving_decor`)
//! and `crates/python/src/parser.rs` (`replace_string_preserving_decor`) — see
//! the comment block at the top of the python copy that explicitly mirrored
//! the rust copy. Centralising it here keeps the "preserve `Formatted<String>`
//! decor across a value rewrite" invariant in one place so a future `toml_edit`
//! tightening lands in a single function.

use toml_edit::Formatted;

/// Replace the inner string of a [`Formatted<String>`] while preserving its
/// surrounding decor (leading/trailing whitespace, attached comments).
///
/// Both Cargo.toml and pyproject.toml patchers call this when rewriting a
/// dependency `version` value, so the format-preservation guarantees of every
/// TOML-backed manifest in the workspace go through this single helper. A
/// naïve `*s = Formatted::new(new)` would drop the decor and collapse e.g.
/// `version = "1.0"` into `version ="1.0"`, breaking the byte-for-byte
/// preservation tests (`apply_updates_inline_table_preserves_decor_byte_for_byte`,
/// `apply_updates_full_table_preserves_decor_byte_for_byte`,
/// `apply_updates_pep621_preserves_multiline_format`,
/// `apply_updates_patches_pep621_optional_dependencies`, …).
pub fn replace_string_preserving_decor(s: &mut Formatted<String>, new: String) {
    let decor = s.decor().clone();
    let mut next = Formatted::new(new);
    *next.decor_mut() = decor;
    *s = next;
}
