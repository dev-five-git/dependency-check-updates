//! Variant-aware container tag selection.
//!
//! A container tag is not a version — it is a version **plus a build variant**:
//! `node:20-alpine`, `python:3.12-slim`, `eclipse-temurin:21-jre-jammy`. The
//! variant is a hard constraint: bumping `20-alpine` to `22` silently swaps
//! Alpine for Debian and usually breaks the build, while bumping it to
//! `22.3.0-alpine3.20` swaps the Alpine base version. Neither is an update the
//! user asked for.
//!
//! So this module groups every published tag by its **variant key** — the
//! verbatim, opaque remainder after the leading numeric run — and only ever
//! compares tags within one group:
//!
//! ```text
//! ""            20, 20.11, 20.11.1, 22, 22.3, 22.3.0
//! "-alpine"     20-alpine, 22-alpine
//! "-slim"       20-slim, 22-slim
//! ```
//!
//! Treating the variant as opaque means `1.2.3-rc.1` lands in its own `-rc.1`
//! group rather than being interpreted as a semver pre-release. That is
//! deliberate: `-rc.1` and `-alpine` are indistinguishable at the tag level,
//! and guessing wrong emits a tag that does not exist. Staying inside the
//! group can only ever under-report an update, never break an image.

use std::collections::{HashMap, HashSet};

use dependency_check_updates_core::{
    ResolvedVersion, TargetLevel, count_numeric_segments, is_version_ref, pad_to_three_segments,
    select_version, split_numeric_head,
};

/// A tag split into the parts the updater reasons about.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TagShape<'a> {
    /// The leading numeric run, with any `v` prefix removed (`20`, `3.12`).
    pub numeric: &'a str,
    /// Everything after the numeric run, verbatim — the variant key
    /// (`""`, `"-alpine"`, `"-slim-bookworm"`).
    pub variant: &'a str,
}

impl<'a> TagShape<'a> {
    /// Decompose `tag`, or return `None` when it is not a version-like tag.
    ///
    /// `latest`, codenames (`bookworm`), and build hashes are rejected by
    /// [`is_version_ref`] — the same predicate the GitHub Actions scanner uses
    /// for `@main` and commit SHAs.
    pub(crate) fn parse(tag: &'a str) -> Option<Self> {
        if !is_version_ref(tag) {
            return None;
        }
        // `is_version_ref` guarantees a leading digit after the optional `v`,
        // so the numeric head below is always non-empty.
        let rest = tag.strip_prefix('v').unwrap_or(tag);
        let (numeric, variant) = split_numeric_head(rest);
        Some(Self { numeric, variant })
    }
}

/// Every published tag of one repository, grouped by variant key.
///
/// Built once per unique repository so that a Compose file referencing
/// `postgres:16` and `postgres:16-alpine` pays the grouping cost once.
pub(crate) struct PreparedTags {
    by_variant: HashMap<String, VariantTags>,
}

/// The tags of a single variant group.
#[derive(Default)]
struct VariantTags {
    /// Numeric heads padded to three segments, sorted ascending and
    /// de-duplicated (`20` and `20.0.0` collapse to one entry).
    sorted: Vec<node_semver::Version>,
    /// Numeric heads exactly as published (`20`, `20.11`, `20.11.1`), used to
    /// check whether a given pin precision is actually backed by a tag.
    numerics: HashSet<String>,
}

impl PreparedTags {
    /// Group `tags` by variant key, discarding everything non-version-like.
    pub(crate) fn new(tags: &[String]) -> Self {
        let mut by_variant: HashMap<String, VariantTags> = HashMap::new();

        for tag in tags {
            let Some(shape) = TagShape::parse(tag) else {
                continue;
            };
            let Ok(version) = node_semver::Version::parse(pad_to_three_segments(shape.numeric))
            else {
                // 4+-segment numerics (`1.2.3.4`) have no semver meaning.
                continue;
            };
            let entry = by_variant.entry(shape.variant.to_owned()).or_default();
            entry.sorted.push(version);
            entry.numerics.insert(shape.numeric.to_owned());
        }

        for group in by_variant.values_mut() {
            // Unstable sort matches the convention of the other registries;
            // the following dedup needs the list sorted anyway, and `20` /
            // `20.0.0` both pad to `20.0.0` so duplicates are real.
            group.sorted.sort_unstable();
            group.sorted.dedup();
        }

        Self { by_variant }
    }

    /// Resolve `current_tag` against the published tags for `target`.
    ///
    /// Both returned values are bare tag bodies **without** any `v` prefix:
    /// the CLI pipeline strips the leading non-digit run off the current tag
    /// and re-glues it onto whatever the registry returns, so emitting the `v`
    /// here would double it (`v20` → `vv22`).
    ///
    /// `TargetLevel::Newest` resolves identically to `Greatest`. The OCI
    /// `tags/list` endpoint returns bare names with no timestamps, and
    /// recovering real publish dates would cost one manifest fetch per tag —
    /// the same trade-off the GitHub Tags API forces.
    pub(crate) fn select(&self, current_tag: &str, target: TargetLevel) -> ResolvedVersion {
        let none = ResolvedVersion {
            latest: None,
            selected: None,
        };

        let Some(shape) = TagShape::parse(current_tag) else {
            return none;
        };
        // No group means the registry publishes no tag with this variant —
        // e.g. the image dropped its `-alpine` line. Reporting nothing is the
        // only safe answer.
        let Some(group) = self.by_variant.get(shape.variant) else {
            return none;
        };

        // Every numeric head in a group is a plain `x.y.z`, so the whole group
        // is "stable" as far as `select_version` is concerned and its highest
        // entry doubles as the `latest` fallback.
        let highest = group.sorted.last().map(ToString::to_string);
        let current = node_semver::Version::parse(pad_to_three_segments(shape.numeric)).ok();

        let selected = select_version(
            current.as_ref(),
            &group.sorted,
            target,
            highest.as_deref(),
            None,
        )
        .map(|padded| {
            let numeric = pick_existing_numeric(&padded, shape.numeric, &group.numerics);
            format!("{numeric}{}", shape.variant)
        });

        ResolvedVersion {
            latest: highest.map(|padded| {
                let numeric = pick_existing_numeric(&padded, shape.numeric, &group.numerics);
                format!("{numeric}{}", shape.variant)
            }),
            selected,
        }
    }
}

/// Collapse a padded three-segment version back to the shortest numeric form
/// an actual published tag uses, preferring the user's current pin precision.
///
/// Images publish moving tags at several precisions (`node:22`, `node:22.3`,
/// `node:22.3.0`) but not uniformly — `postgres` publishes `16` and `16.4`,
/// `traefik` only `v3.1.6`. Emitting a precision nobody published produces a
/// tag that fails to pull, so this walks the user's precision upward and, only
/// if nothing at or above it exists, downward, returning the first form backed
/// by a real tag.
///
/// The precision the user already pins is treated as known-to-exist — their
/// build runs on it right now — so a same-prefix match short-circuits without
/// consulting the tag list. Without this, an image whose moving `20` tag is
/// absent from the response would be "upgraded" from `20` to `20.11.1`, which
/// is the same image under a noisier name.
fn pick_existing_numeric(
    padded: &str,
    current_numeric: &str,
    numerics: &HashSet<String>,
) -> String {
    let segments: Vec<&str> = padded.split('.').filter(|s| !s.is_empty()).collect();
    let len = segments.len();
    if len == 0 {
        return padded.to_owned();
    }

    let start = count_numeric_segments(current_numeric).clamp(1, len);

    let current_prefix = segments[..start].join(".");
    if current_numeric == current_prefix {
        return current_prefix;
    }

    // Shortest form at or above the pin precision first, then the longest
    // shorter form. The padded input itself is always one of the candidates at
    // `p == len`, so the fallback only fires for inputs that never came from
    // this repository's tag list.
    (start..=len)
        .chain((1..start).rev())
        .find_map(|p| {
            let candidate = segments[..p].join(".");
            numerics.contains(candidate.as_str()).then_some(candidate)
        })
        .unwrap_or_else(|| padded.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn tags(names: &[&str]) -> PreparedTags {
        let owned: Vec<String> = names.iter().map(|s| (*s).to_owned()).collect();
        PreparedTags::new(&owned)
    }

    #[rstest]
    // Plain numeric tags at every precision.
    #[case::major("20", Some(("20", "")))]
    #[case::major_minor("3.12", Some(("3.12", "")))]
    #[case::full("1.2.3", Some(("1.2.3", "")))]
    // `v` prefix is stripped from the numeric but leaves the variant alone.
    #[case::v_prefixed("v3.1.6", Some(("3.1.6", "")))]
    // Variant suffixes are captured verbatim, however many segments they have.
    #[case::alpine("20-alpine", Some(("20", "-alpine")))]
    #[case::slim("3.12-slim", Some(("3.12", "-slim")))]
    #[case::multi_variant("21-jre-jammy", Some(("21", "-jre-jammy")))]
    #[case::variant_with_digits("20.11-alpine3.19", Some(("20.11", "-alpine3.19")))]
    // A semver pre-release is deliberately treated as just another variant.
    #[case::prerelease_is_a_variant("7.4.0-rc1", Some(("7.4.0", "-rc1")))]
    // Moving pointers and hashes have no version to track.
    #[case::latest("latest", None)]
    #[case::codename("bookworm", None)]
    #[case::build_hash("1a2b3c4", None)]
    #[case::empty("", None)]
    fn tag_shape_parse_cases(#[case] input: &str, #[case] expected: Option<(&str, &str)>) {
        let actual = TagShape::parse(input);
        assert_eq!(
            actual,
            expected.map(|(numeric, variant)| TagShape { numeric, variant })
        );
    }

    #[rstest]
    // ---- The headline guarantee: variants never cross-contaminate. ----
    // `20-alpine` must reach `22-alpine`, never the bare `22` that also exists.
    #[case::alpine_stays_alpine(
        &["20", "20-alpine", "22", "22-alpine"],
        "20-alpine",
        TargetLevel::Latest,
        Some("22-alpine")
    )]
    // …and the bare pin must not wander into a variant.
    #[case::bare_stays_bare(
        &["20", "20-alpine", "22", "22-alpine"],
        "20",
        TargetLevel::Latest,
        Some("22")
    )]
    // A variant the registry no longer publishes yields nothing rather than
    // falling back to some other variant.
    #[case::unknown_variant_yields_nothing(
        &["20", "22"],
        "20-alpine",
        TargetLevel::Latest,
        None
    )]
    // ---- Pin precision is preserved when a tag backs it. ----
    #[case::major_pin_keeps_major(
        &["20", "20.11.1", "22", "22.3.0"],
        "20",
        TargetLevel::Latest,
        Some("22")
    )]
    #[case::full_pin_keeps_full(
        &["20.11.1", "22.3.0"],
        "20.11.1",
        TargetLevel::Latest,
        Some("22.3.0")
    )]
    #[case::minor_pin_keeps_minor(
        &["20.11", "20.11.1", "22.3", "22.3.0"],
        "20.11",
        TargetLevel::Latest,
        Some("22.3")
    )]
    // Major pin, but the registry publishes no moving major tag → escalate to
    // the shortest form that actually exists rather than emitting a 404 tag.
    #[case::escalates_when_major_tag_absent(
        &["20", "22.3.0"],
        "20",
        TargetLevel::Latest,
        Some("22.3.0")
    )]
    // Full pin, but only a moving major tag exists on the new train →
    // de-escalate to it.
    #[case::de_escalates_to_major(
        &["20.11.1", "22"],
        "20.11.1",
        TargetLevel::Latest,
        Some("22")
    )]
    // ---- Target levels behave as they do everywhere else. ----
    #[case::minor_stays_on_major(
        &["20.1.0", "20.5.0", "22.0.0"],
        "20.1.0",
        TargetLevel::Minor,
        Some("20.5.0")
    )]
    #[case::patch_stays_on_minor(
        &["20.1.0", "20.1.4", "20.5.0"],
        "20.1.0",
        TargetLevel::Patch,
        Some("20.1.4")
    )]
    #[case::greatest_takes_the_top(
        &["20", "22", "23"],
        "20",
        TargetLevel::Greatest,
        Some("23")
    )]
    // `newest` has no publish dates to work with and mirrors `greatest`.
    #[case::newest_mirrors_greatest(
        &["20", "22", "23"],
        "20",
        TargetLevel::Newest,
        Some("23")
    )]
    // ---- Inputs that must produce no suggestion at all. ----
    #[case::latest_pin_is_untracked(&["20", "22"], "latest", TargetLevel::Latest, None)]
    #[case::empty_registry(&[], "20", TargetLevel::Latest, None)]
    // Non-version tags in the response are filtered out, not tripped over.
    #[case::ignores_non_version_tags(
        &["latest", "bookworm", "20", "22"],
        "20",
        TargetLevel::Latest,
        Some("22")
    )]
    // Already on the top tag → the selection equals the current pin, and the
    // pipeline's equality check turns that into "no update".
    #[case::already_current(&["20", "22"], "22", TargetLevel::Latest, Some("22"))]
    fn select_cases(
        #[case] published: &[&str],
        #[case] current: &str,
        #[case] target: TargetLevel,
        #[case] expected: Option<&str>,
    ) {
        let selected = tags(published).select(current, target).selected;
        assert_eq!(selected.as_deref(), expected);
    }

    #[test]
    fn select_never_emits_a_v_prefix() {
        // The pipeline re-glues the leading non-digit run from the current
        // spec, so a `v`-prefixed pin must come back bare or it doubles up.
        let selected = tags(&["v3.1.6", "v3.2.0"])
            .select("v3.1.6", TargetLevel::Latest)
            .selected;
        assert_eq!(selected.as_deref(), Some("3.2.0"));
    }

    #[test]
    fn select_reports_latest_within_the_variant_group() {
        // `latest` is informational, but it must still respect the variant
        // boundary — reporting the bare `23` for an `-alpine` pin would be
        // actively misleading.
        let resolved =
            tags(&["20-alpine", "23-alpine", "24"]).select("20-alpine", TargetLevel::Latest);
        assert_eq!(resolved.latest.as_deref(), Some("23-alpine"));
    }

    #[test]
    fn select_keeps_a_major_float_absent_from_the_tag_list() {
        // Mirrors a high-velocity image whose moving `20` tag is not part of
        // the response: the user is provably running on `20`, so keep it
        // instead of "upgrading" them to the equivalent `20.11.1`.
        let selected = tags(&["20.11.1", "20.11.0"])
            .select("20", TargetLevel::Latest)
            .selected;
        assert_eq!(selected.as_deref(), Some("20"));
    }

    #[test]
    fn prepared_tags_dedupes_equivalent_precisions() {
        // `22` and `22.0.0` both pad to `22.0.0`; the sorted list must not
        // carry the duplicate, but BOTH precisions stay available to the
        // existence walk.
        let prepared = tags(&["22", "22.0.0"]);
        let group = prepared.by_variant.get("").expect("bare variant group");
        assert_eq!(group.sorted.len(), 1);
        assert!(group.numerics.contains("22"));
        assert!(group.numerics.contains("22.0.0"));
    }

    #[test]
    fn prepared_tags_skips_four_segment_numerics() {
        // `1.2.3.4` has no semver reading; it must be dropped rather than
        // mis-parsed into the group.
        let prepared = tags(&["1.2.3.4", "1.2.3"]);
        let group = prepared.by_variant.get("").expect("bare variant group");
        assert_eq!(group.sorted.len(), 1);
        assert!(!group.numerics.contains("1.2.3.4"));
    }

    #[rstest]
    // The padded input always wins when the tag list backs that exact form.
    #[case::exact_match("22.3.0", "20.11.1", &["22.3.0"], "22.3.0")]
    // Shorter published forms are preferred at the pin's precision.
    #[case::major_pin_finds_major("22.0.0", "20", &["22", "22.0.0"], "22")]
    // Nothing at or above the pin precision → walk downward.
    #[case::walks_down("22.0.0", "20.11.1", &["22"], "22")]
    // A version that never came from this tag list falls back to itself
    // instead of panicking.
    #[case::unbacked_input_falls_back("99.0.0", "20", &["22"], "99.0.0")]
    fn pick_existing_numeric_cases(
        #[case] padded: &str,
        #[case] current: &str,
        #[case] published: &[&str],
        #[case] expected: &str,
    ) {
        let numerics: HashSet<String> = published.iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(pick_existing_numeric(padded, current, &numerics), expected);
    }
}
