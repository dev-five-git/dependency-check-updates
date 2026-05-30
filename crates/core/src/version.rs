//! Generic, ecosystem-agnostic version selection.
//!
//! npm, crates.io, and the GitHub Tags API all pick a target version from a
//! sorted candidate list using the same algorithm — only the concrete version
//! type and the fallback values differ. This module captures that algorithm
//! once behind the [`SelectableVersion`] trait so the three registry clients
//! no longer carry near-identical copies of it.

use crate::types::TargetLevel;

/// A version type the [`select_version`] algorithm can operate on.
///
/// Implemented in this crate for both `node_semver::Version` and
/// `semver::Version`. The impls live here (rather than in the ecosystem
/// crates) because both the Node and GitHub registries resolve
/// `node_semver::Version`; a per-crate impl would violate the orphan rule.
pub trait SelectableVersion: std::fmt::Display {
    /// Whether this version carries a pre-release tag (e.g. `-rc.1`).
    fn is_prerelease(&self) -> bool;
    /// Major version component.
    fn major(&self) -> u64;
    /// Minor version component.
    fn minor(&self) -> u64;
    /// Patch version component.
    fn patch(&self) -> u64;
}

impl SelectableVersion for node_semver::Version {
    fn is_prerelease(&self) -> bool {
        !self.pre_release.is_empty()
    }
    fn major(&self) -> u64 {
        self.major
    }
    fn minor(&self) -> u64 {
        self.minor
    }
    fn patch(&self) -> u64 {
        self.patch
    }
}

impl SelectableVersion for semver::Version {
    fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }
    fn major(&self) -> u64 {
        self.major
    }
    fn minor(&self) -> u64 {
        self.minor
    }
    fn patch(&self) -> u64 {
        self.patch
    }
}

impl SelectableVersion for pep440_rs::Version {
    /// `any_prerelease` covers alpha/beta/rc *and* dev releases — all of which
    /// are "not a final stable release" for selection purposes.
    fn is_prerelease(&self) -> bool {
        self.any_prerelease()
    }
    fn major(&self) -> u64 {
        self.release().first().copied().unwrap_or(0)
    }
    fn minor(&self) -> u64 {
        self.release().get(1).copied().unwrap_or(0)
    }
    fn patch(&self) -> u64 {
        self.release().get(2).copied().unwrap_or(0)
    }
}

/// Select the best candidate for `target` from a pre-sorted (ascending)
/// `all_versions` list.
///
/// `current` is the user's currently-pinned version (already parsed), used for
/// the "prerelease tail" policy and to constrain `Minor`/`Patch` to the same
/// major(.minor).
///
/// Two fallbacks are split out so the registries can share one implementation
/// despite differing edge-case behaviour:
/// - `latest_for_stable` is returned for `Latest` when `current` is stable,
///   and for an empty candidate list. npm/crates.io pass the registry's latest
///   stable; GitHub passes its highest stable tag.
/// - `unparseable_minor_patch` is returned for `Minor`/`Patch` when `current`
///   could not be parsed. npm/crates.io fall back to the latest stable here;
///   GitHub passes `None` (an unparseable ref gives no major to stay on).
#[must_use]
pub fn select_version<V: SelectableVersion>(
    current: Option<&V>,
    all_versions: &[V],
    target: TargetLevel,
    latest_for_stable: Option<String>,
    unparseable_minor_patch: Option<String>,
) -> Option<String> {
    if all_versions.is_empty() {
        return latest_for_stable;
    }

    let current_is_prerelease = current.is_some_and(SelectableVersion::is_prerelease);

    // Accept any stable version; accept a pre-release only when the user is
    // already on a pre-release of the *same* major.minor.patch train. Written
    // without `unwrap`/`expect` so the function carries no panic path.
    let accept = |v: &&V| -> bool {
        if !v.is_prerelease() {
            return true;
        }
        current.is_some_and(|cur| {
            cur.is_prerelease()
                && v.major() == cur.major()
                && v.minor() == cur.minor()
                && v.patch() == cur.patch()
        })
    };

    // `match` is kept (for variant-exhaustiveness) but every arm body is an
    // expression that begins on the arm line, so each branch is a single
    // covered region.
    match target {
        TargetLevel::Latest if current_is_prerelease => {
            all_versions.iter().rev().find(accept).map(ToString::to_string)
        }
        TargetLevel::Latest => latest_for_stable,
        TargetLevel::Greatest | TargetLevel::Newest => all_versions.last().map(ToString::to_string),
        TargetLevel::Minor => match current {
            None => unparseable_minor_patch,
            Some(cur) => all_versions
                .iter()
                .rev()
                .find(|v| v.major() == cur.major() && accept(v))
                .map(ToString::to_string),
        },
        TargetLevel::Patch => match current {
            None => unparseable_minor_patch,
            Some(cur) => all_versions
                .iter()
                .rev()
                .find(|v| v.major() == cur.major() && v.minor() == cur.minor() && accept(v))
                .map(ToString::to_string),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn vers(specs: &[&str]) -> Vec<semver::Version> {
        let mut v: Vec<_> = specs
            .iter()
            .filter_map(|s| semver::Version::parse(s).ok())
            .collect();
        v.sort();
        v
    }

    fn parse(s: &str) -> Option<semver::Version> {
        semver::Version::parse(s).ok()
    }

    /// Whole-algorithm coverage for [`select_version`] against
    /// `semver::Version`. Every case captures the unique combination of
    /// `(current, candidates, target, latest_for_stable, unparseable_minor_patch)`
    /// → expected output that the three registry clients depend on.
    ///
    /// `current_str = None` simulates an unparseable current pin; the
    /// `unparseable_*` case proves the dedicated fallback wins over
    /// `latest_for_stable` for `Minor`/`Patch`.
    #[rstest]
    #[case::empty_returns_latest_for_stable(
        None,
        &[],
        TargetLevel::Greatest,
        Some("1.0.0"),
        None,
        Some("1.0.0"),
    )]
    #[case::latest_stable_returns_fallback(
        Some("1.0.0"),
        &["1.0.0", "1.5.0", "2.0.0"],
        TargetLevel::Latest,
        Some("2.0.0"),
        None,
        Some("2.0.0"),
    )]
    #[case::minor_stays_on_major(
        Some("1.0.0"),
        &["1.0.0", "1.5.0", "2.0.0"],
        TargetLevel::Minor,
        None,
        None,
        Some("1.5.0"),
    )]
    #[case::patch_stays_on_minor(
        Some("1.0.0"),
        &["1.0.0", "1.0.5", "1.1.0", "2.0.0"],
        TargetLevel::Patch,
        None,
        None,
        Some("1.0.5"),
    )]
    #[case::greatest_includes_prerelease(
        Some("1.0.0"),
        &["1.0.0", "2.0.0-rc.1"],
        TargetLevel::Greatest,
        None,
        None,
        Some("2.0.0-rc.1"),
    )]
    // Stable current + Latest → returns latest_for_stable, never a prerelease.
    #[case::latest_stable_excludes_prerelease_via_fallback(
        Some("1.0.0"),
        &["1.0.0", "2.0.0-rc.1"],
        TargetLevel::Latest,
        Some("1.0.0"),
        None,
        Some("1.0.0"),
    )]
    // Current on 2.0.0-rc.1 → Latest may climb the same train.
    #[case::prerelease_tail_same_train(
        Some("2.0.0-rc.1"),
        &["1.1.0", "2.0.0-rc.1", "2.0.0-rc.2"],
        TargetLevel::Latest,
        Some("1.1.0"),
        None,
        Some("2.0.0-rc.2"),
    )]
    // current None (unparseable) → returns the unparseable fallback, not latest_for_stable.
    #[case::unparseable_minor_uses_dedicated_fallback(
        None,
        &["1.0.0", "2.0.0"],
        TargetLevel::Minor,
        Some("2.0.0"),
        None,
        None,
    )]
    // Patch with current None (unparseable) → returns the unparseable fallback.
    // Exercises the early-return arm in `TargetLevel::Patch` (line 142).
    #[case::unparseable_patch_uses_dedicated_fallback(
        None,
        &["1.0.0", "1.0.1", "2.0.0"],
        TargetLevel::Patch,
        Some("2.0.0"),
        None,
        None,
    )]
    // `Newest` is the second arm under `Greatest | Newest` (line 127); proves
    // it picks the last entry just like `Greatest`.
    #[case::newest_returns_last_candidate(
        Some("1.0.0"),
        &["1.0.0", "1.5.0", "2.0.0"],
        TargetLevel::Newest,
        None,
        None,
        Some("2.0.0"),
    )]
    fn select_version_cases(
        #[case] current_str: Option<&str>,
        #[case] version_strs: &[&str],
        #[case] target: TargetLevel,
        #[case] latest_for_stable: Option<&str>,
        #[case] unparseable_minor_patch: Option<&str>,
        #[case] expected: Option<&str>,
    ) {
        let cur = current_str.and_then(parse);
        let candidates = vers(version_strs);
        let selected = select_version(
            cur.as_ref(),
            &candidates,
            target,
            latest_for_stable.map(ToOwned::to_owned),
            unparseable_minor_patch.map(ToOwned::to_owned),
        );
        assert_eq!(selected, expected.map(ToOwned::to_owned));
    }

    #[test]
    fn test_selectable_version_trait_accessors() {
        let v = semver::Version::parse("3.4.5-beta.1").unwrap();
        assert!(v.is_prerelease());
        assert_eq!(v.major(), 3);
        assert_eq!(v.minor(), 4);
        assert_eq!(v.patch(), 5);

        let nv = node_semver::Version::parse("6.7.8").unwrap();
        assert!(!nv.is_prerelease());
        assert_eq!(nv.major(), 6);
        assert_eq!(nv.minor(), 7);
        assert_eq!(nv.patch(), 8);

        let pv: pep440_rs::Version = "9.10.11".parse().unwrap();
        assert!(!pv.is_prerelease());
        assert_eq!(pv.major(), 9);
        assert_eq!(pv.minor(), 10);
        assert_eq!(pv.patch(), 11);

        // PEP 440 pre/dev releases and short release tuples.
        let pre: pep440_rs::Version = "2.0a1".parse().unwrap();
        assert!(pre.is_prerelease());
        assert_eq!(pre.major(), 2);
        assert_eq!(pre.minor(), 0);
        let dev: pep440_rs::Version = "1.0.dev0".parse().unwrap();
        assert!(dev.is_prerelease());
        let short: pep440_rs::Version = "5".parse().unwrap();
        assert_eq!(short.major(), 5);
        assert_eq!(short.minor(), 0);
        assert_eq!(short.patch(), 0);
    }
}
