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

    match target {
        TargetLevel::Latest => {
            if current_is_prerelease {
                all_versions.iter().rev().find(accept).map(ToString::to_string)
            } else {
                latest_for_stable
            }
        }
        TargetLevel::Greatest | TargetLevel::Newest => {
            all_versions.last().map(ToString::to_string)
        }
        TargetLevel::Minor => {
            let Some(cur) = current else {
                return unparseable_minor_patch;
            };
            all_versions
                .iter()
                .rev()
                .find(|v| v.major() == cur.major() && accept(v))
                .map(ToString::to_string)
        }
        TargetLevel::Patch => {
            let Some(cur) = current else {
                return unparseable_minor_patch;
            };
            all_versions
                .iter()
                .rev()
                .find(|v| v.major() == cur.major() && v.minor() == cur.minor() && accept(v))
                .map(ToString::to_string)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn test_empty_returns_latest_for_stable() {
        let selected = select_version::<semver::Version>(
            None,
            &[],
            TargetLevel::Greatest,
            Some("1.0.0".to_owned()),
            None,
        );
        assert_eq!(selected, Some("1.0.0".to_owned()));
    }

    #[test]
    fn test_latest_stable_returns_fallback() {
        let v = vers(&["1.0.0", "1.5.0", "2.0.0"]);
        let cur = parse("1.0.0");
        let selected = select_version(
            cur.as_ref(),
            &v,
            TargetLevel::Latest,
            Some("2.0.0".to_owned()),
            None,
        );
        assert_eq!(selected, Some("2.0.0".to_owned()));
    }

    #[test]
    fn test_minor_stays_on_major() {
        let v = vers(&["1.0.0", "1.5.0", "2.0.0"]);
        let cur = parse("1.0.0");
        let selected =
            select_version(cur.as_ref(), &v, TargetLevel::Minor, None, None);
        assert_eq!(selected, Some("1.5.0".to_owned()));
    }

    #[test]
    fn test_patch_stays_on_minor() {
        let v = vers(&["1.0.0", "1.0.5", "1.1.0", "2.0.0"]);
        let cur = parse("1.0.0");
        let selected =
            select_version(cur.as_ref(), &v, TargetLevel::Patch, None, None);
        assert_eq!(selected, Some("1.0.5".to_owned()));
    }

    #[test]
    fn test_greatest_includes_prerelease() {
        let v = vers(&["1.0.0", "2.0.0-rc.1"]);
        let cur = parse("1.0.0");
        let selected =
            select_version(cur.as_ref(), &v, TargetLevel::Greatest, None, None);
        assert_eq!(selected, Some("2.0.0-rc.1".to_owned()));
    }

    #[test]
    fn test_latest_stable_excludes_prerelease_via_fallback() {
        // Stable current + Latest → returns latest_for_stable, never a prerelease.
        let v = vers(&["1.0.0", "2.0.0-rc.1"]);
        let cur = parse("1.0.0");
        let selected = select_version(
            cur.as_ref(),
            &v,
            TargetLevel::Latest,
            Some("1.0.0".to_owned()),
            None,
        );
        assert_eq!(selected, Some("1.0.0".to_owned()));
    }

    #[test]
    fn test_prerelease_tail_same_train() {
        // Current on 2.0.0-rc.1 → Latest may climb the same train.
        let v = vers(&["1.1.0", "2.0.0-rc.1", "2.0.0-rc.2"]);
        let cur = parse("2.0.0-rc.1");
        let selected = select_version(
            cur.as_ref(),
            &v,
            TargetLevel::Latest,
            Some("1.1.0".to_owned()),
            None,
        );
        assert_eq!(selected, Some("2.0.0-rc.2".to_owned()));
    }

    #[test]
    fn test_unparseable_minor_uses_dedicated_fallback() {
        let v = vers(&["1.0.0", "2.0.0"]);
        // current None (unparseable) → returns the unparseable fallback, not latest_for_stable.
        let selected = select_version::<semver::Version>(
            None,
            &v,
            TargetLevel::Minor,
            Some("2.0.0".to_owned()),
            None,
        );
        assert_eq!(selected, None);
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
    }
}
