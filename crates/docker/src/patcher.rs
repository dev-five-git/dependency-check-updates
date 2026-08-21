//! Byte-range patcher for container image tags.
//!
//! Only the bytes of a tag are replaced. The image name, surrounding quotes,
//! trailing comments, `AS <stage>` tails, build flags, and every untracked
//! reference (`:latest`, digest pins, `${VAR}` interpolations) survive
//! byte-for-byte.
//!
//! The descending-`replace_range` engine itself lives in
//! [`dependency_check_updates_core::patch`]; this module only turns a list of
//! [`PlannedUpdate`]s into the byte ranges it consumes.

use dependency_check_updates_core::PlannedUpdate;
use dependency_check_updates_core::patch::{Patch, PatchError, apply_byte_patches};

use crate::image::ImageLocation;

/// Build byte patches by matching `updates` against `locations`.
///
/// Updates are joined on `(name, from-tag)` because one document can reference
/// the same image at two different tags — a Compose file pinning
/// `postgres:16` for the primary and `postgres:15` for a migration sidecar is
/// ordinary — so each occurrence must be rewritten using its own original tag
/// as the key. Locations are consumed as they match, which lets repeated
/// `(name, tag)` pairs each receive their own patch.
///
/// Updates that match no location are silently skipped: the reference may have
/// been edited since the scan that produced the plan.
pub(crate) fn build_patches(locations: &[ImageLocation], updates: &[PlannedUpdate]) -> Vec<Patch> {
    let mut consumed = vec![false; locations.len()];
    let mut patches = Vec::with_capacity(updates.len());

    for update in updates {
        let Some((idx, location)) = locations
            .iter()
            .enumerate()
            .find(|(i, l)| !consumed[*i] && l.name == update.name && l.tag == update.from)
        else {
            continue;
        };
        consumed[idx] = true;
        patches.push(Patch {
            start: location.tag_start,
            end: location.tag_end,
            new_value: update.to.clone(),
        });
    }

    patches
}

/// Apply `updates` to `text`, replacing only the tag bytes of each matched
/// image reference.
///
/// # Errors
///
/// Returns [`PatchError::OverlappingPatches`] if two patches would touch the
/// same byte range. Each scanned tag occupies a distinct, byte-disjoint span
/// and every location is consumed at most once, so this indicates a scanner
/// bug rather than user error.
pub(crate) fn apply(
    text: &str,
    locations: &[ImageLocation],
    updates: &[PlannedUpdate],
) -> Result<String, PatchError> {
    if updates.is_empty() {
        return Ok(text.to_owned());
    }
    apply_byte_patches(text, &build_patches(locations, updates))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dependency_check_updates_core::DependencySection;
    use rstest::rstest;

    fn updates(rows: &[(&str, &str, &str)]) -> Vec<PlannedUpdate> {
        rows.iter()
            .map(|(name, from, to)| PlannedUpdate {
                name: (*name).to_owned(),
                section: DependencySection::DockerImage,
                from: (*from).to_owned(),
                to: (*to).to_owned(),
            })
            .collect()
    }

    #[rstest]
    // No updates → byte-identical output.
    #[case::empty_is_identity("FROM node:20-alpine\n", &[], "FROM node:20-alpine\n")]
    // The headline case: only the tag changes, the variant travels with it.
    #[case::single_update(
        "FROM node:20-alpine\n",
        &[("node", "20-alpine", "22-alpine")],
        "FROM node:22-alpine\n"
    )]
    // Everything around the tag survives: stage alias, build flag, comment.
    #[case::preserves_stage_alias(
        "FROM node:20-alpine AS builder\n",
        &[("node", "20-alpine", "22-alpine")],
        "FROM node:22-alpine AS builder\n"
    )]
    #[case::preserves_platform_flag(
        "FROM --platform=linux/amd64 node:20 AS build\n",
        &[("node", "20", "22")],
        "FROM --platform=linux/amd64 node:22 AS build\n"
    )]
    #[case::preserves_yaml_comment(
        "    image: redis:7.2  # pinned\n",
        &[("redis", "7.2", "7.4")],
        "    image: redis:7.4  # pinned\n"
    )]
    #[case::preserves_yaml_quotes(
        "    image: 'redis:7.2'\n",
        &[("redis", "7.2", "7.4")],
        "    image: 'redis:7.4'\n"
    )]
    // A tag that grows or shrinks in length must not disturb its neighbours.
    #[case::handles_length_change(
        "FROM node:20 AS build\n",
        &[("node", "20", "22.3.0-alpine")],
        "FROM node:22.3.0-alpine AS build\n"
    )]
    // Untracked references next to a tracked one are left alone.
    #[case::leaves_untracked_alone(
        concat!(
            "FROM node:20-alpine AS build\n",
            "FROM node:latest AS scratchpad\n",
            "FROM gcr.io/distroless/base@sha256:abc123\n",
        ),
        &[("node", "20-alpine", "22-alpine")],
        concat!(
            "FROM node:22-alpine AS build\n",
            "FROM node:latest AS scratchpad\n",
            "FROM gcr.io/distroless/base@sha256:abc123\n",
        )
    )]
    // The same image pinned at two different tags: each occurrence is keyed on
    // its own `from`, so neither steals the other's patch.
    #[case::same_image_distinct_tags(
        concat!(
            "    image: postgres:15\n",
            "    image: postgres:16\n",
        ),
        &[("postgres", "15", "15.8"), ("postgres", "16", "16.4")],
        concat!(
            "    image: postgres:15.8\n",
            "    image: postgres:16.4\n",
        )
    )]
    // The same image at the same tag twice: both get patched, not just the first.
    #[case::duplicate_occurrences_both_patched(
        concat!(
            "FROM node:20-alpine AS deps\n",
            "FROM node:20-alpine AS build\n",
        ),
        &[("node", "20-alpine", "22-alpine"), ("node", "20-alpine", "22-alpine")],
        concat!(
            "FROM node:22-alpine AS deps\n",
            "FROM node:22-alpine AS build\n",
        )
    )]
    // A `from` that no longer matches the text is skipped, not misapplied.
    #[case::unmatched_update_skipped(
        "FROM node:20-alpine\n",
        &[("node", "19-alpine", "22-alpine")],
        "FROM node:20-alpine\n"
    )]
    fn apply_dockerfile_cases(
        #[case] text: &str,
        #[case] rows: &[(&str, &str, &str)],
        #[case] expected: &str,
    ) {
        // Both scanners feed the same patcher, so the case table drives
        // whichever one recognises the fixture.
        let mut locations = crate::parser::scan(text);
        locations.extend(crate::yaml::scan(text));
        let result = apply(text, &locations, &updates(rows)).expect("patches never overlap");
        assert_eq!(result, expected);
    }

    #[test]
    fn build_patches_consumes_each_location_once() {
        // Two updates carrying an identical `(name, from)` must map onto the
        // two distinct occurrences rather than both landing on the first.
        let text = "FROM node:20 AS a\nFROM node:20 AS b\n";
        let locations = crate::parser::scan(text);
        let patches = build_patches(
            &locations,
            &updates(&[("node", "20", "22"), ("node", "20", "22")]),
        );
        assert_eq!(patches.len(), 2);
        assert_ne!(patches[0].start, patches[1].start);
    }
}
