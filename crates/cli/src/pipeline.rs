use tracing::{debug, trace, warn};

use dependency_check_updates_core::{
    DcuError, DependencySpec, ManifestKind, PlannedUpdate, ResolvedVersion,
};

/// Filter dependencies by include/exclude patterns.
pub(crate) fn filter_deps(
    deps: &[DependencySpec],
    include: &[String],
    exclude: &[String],
) -> Vec<DependencySpec> {
    deps.iter()
        .filter(|dep| {
            if !include.is_empty() && !include.iter().any(|f| dep.name.contains(f.as_str())) {
                return false;
            }
            if exclude.iter().any(|x| dep.name.contains(x.as_str())) {
                return false;
            }
            true
        })
        .cloned()
        .collect()
}

/// Compute planned updates from resolved versions.
pub(crate) fn compute_updates(
    deps: &[DependencySpec],
    resolved: &[(usize, Result<ResolvedVersion, DcuError>)],
    kind: ManifestKind,
) -> Vec<PlannedUpdate> {
    let mut updates = Vec::new();

    for (idx, result) in resolved {
        let dep = &deps[*idx];

        let resolved = match result {
            Ok(r) => r,
            Err(e) => {
                // Surface the error's full Display — without it, users hit
                // GitHub rate limits and never see the GITHUB_TOKEN hint.
                warn!("{e}");
                continue;
            }
        };

        let Some(selected) = &resolved.selected else {
            debug!(package = %dep.name, "no version selected by registry");
            continue;
        };

        // Strip range prefix for comparison
        let current_bare = dep
            .current_req
            .trim_start_matches(|c: char| !c.is_ascii_digit());

        // Safety net: never suggest a downgrade. When both current and selected
        // can be parsed as semver (after padding short forms like `5` or `5.1`
        // to `5.0.0` / `5.1.0`), skip this dependency if selected <= current.
        //
        // Padding is needed for GitHub Actions refs (`v5`) and short Rust /
        // Python pins (`wiremock = "0.6"`) — without it, the safety net was
        // bypassed exactly where downgrades are most likely.
        if let (Ok(cur_ver), Ok(sel_ver)) = (
            semver::Version::parse(&pad_to_three_segments(current_bare)),
            semver::Version::parse(&pad_to_three_segments(selected)),
        ) && sel_ver <= cur_ver
        {
            trace!(
                package = %dep.name,
                current = %dep.current_req,
                selected = %selected,
                "skipping: selected version is not newer than current"
            );
            continue;
        }

        // Preserve precision: if the user wrote "0.6" (2 segments), truncate the
        // resolved version to 2 segments before comparing. This respects the user's
        // intent to pin only at that granularity.
        //
        // GitHub workflow refs are exempt: the GitHub registry already resolved
        // the exact, tag-validated ref form (`pick_existing_ref`), so re-running
        // the generic truncation here could re-shorten an escalated ref
        // (`v8.1.0` → `v8`) back into a dangling tag.
        let selected_truncated = if kind == ManifestKind::GitHubWorkflow {
            selected.clone()
        } else {
            let precision = count_version_segments(current_bare);

            if precision < 3 && !is_plain_numeric_version(selected) {
                trace!(
                    package = %dep.name,
                    current = %dep.current_req,
                    selected = %selected,
                    "skipping: selected version cannot be safely truncated"
                );
                continue;
            }

            truncate_version(selected, precision)
        };

        if current_bare == selected_truncated {
            trace!(package = %dep.name, version = %dep.current_req, "already up to date");
            continue;
        }

        // Preserve the range prefix from the original spec
        let prefix_len = dep.current_req.len() - current_bare.len();
        let prefix = &dep.current_req[..prefix_len];
        let new_version = format!("{prefix}{selected_truncated}");

        updates.push(PlannedUpdate {
            name: dep.name.clone(),
            section: dep.section,
            from: dep.current_req.clone(),
            to: new_version,
        });
    }

    updates
}

/// Pad a version string to exactly three numeric segments so it can be
/// fed to `semver::Version::parse` for ordering comparisons.
///
/// Preserves any pre-release / build-metadata suffix (`-rc.1`, `+build.7`).
///
/// `pad_to_three_segments("5")`           → `"5.0.0"`
/// `pad_to_three_segments("5.1")`         → `"5.1.0"`
/// `pad_to_three_segments("5.1.0")`       → `"5.1.0"`
/// `pad_to_three_segments("5.1.0-rc.1")`  → `"5.1.0-rc.1"`
/// `pad_to_three_segments("1.2-beta")`    → `"1.2.0-beta"`
fn pad_to_three_segments(v: &str) -> String {
    if v.is_empty() {
        return v.to_owned();
    }
    let (numeric, suffix) = v
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .map_or((v, ""), |i| v.split_at(i));
    let parts: Vec<&str> = numeric.split('.').filter(|s| !s.is_empty()).collect();
    match parts.len() {
        1 => format!("{}.0.0{}", parts[0], suffix),
        2 => format!("{}.{}.0{}", parts[0], parts[1], suffix),
        // 0 (no numeric prefix) or ≥3 (already padded / over-padded): leave
        // as-is. `semver::Version::parse` will reject the 0-parts case below.
        _ => v.to_owned(),
    }
}

/// Count the number of version segments in a bare version string.
///
/// "1"      → 1 (major only)
/// "1.0"    → 2 (major.minor)
/// "1.0.0"  → 3 (major.minor.patch)
/// "1.0.0-beta.1" → 3 (pre-release suffix ignored)
fn count_version_segments(bare: &str) -> usize {
    // Stop at the first non-digit, non-dot character (e.g., '-' for pre-release)
    let numeric_part = bare
        .split(|c: char| !c.is_ascii_digit() && c != '.')
        .next()
        .unwrap_or("");
    if numeric_part.is_empty() {
        return 0;
    }
    numeric_part.split('.').filter(|s| !s.is_empty()).count()
}

/// Whether `version` is a plain numeric version — one or more dot-separated
/// segments that are *all* ASCII digits, with no pre-release (`-…`) or build
/// (`+…`) suffix.
///
/// Such versions are always safe to truncate to fewer segments (`5.1` → `5`,
/// `4.0.0` → `4.0`): there is no pre-release tag that could be silently
/// promoted into a stable-looking pin. This intentionally accepts clean
/// two-segment stables like `5.1` (e.g. Django) — the previous
/// exactly-three-segment check rejected them, which made `--target
/// greatest/newest/minor/patch` silently skip such packages whenever the user
/// pinned at <3-segment precision. Versions carrying a suffix
/// (`4.0.0-beta.0`, `1.2.3+build`) return `false` so the caller refuses to
/// truncate them.
fn is_plain_numeric_version(version: &str) -> bool {
    let mut any = false;
    for segment in version.split('.') {
        if segment.is_empty() || !segment.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        any = true;
    }
    any
}

/// Truncate a version string to the given number of segments.
///
/// Always strips build metadata (the `+...` suffix) since it has no meaning
/// in version requirements and causes warnings in Cargo.toml. Pre-release
/// suffix (`-beta.1`) is preserved when not truncating patch level.
///
/// `truncate_version("1.2.3`", 2)             → "1.2"
/// `truncate_version("1.2.3`", 3)             → "1.2.3"
/// `truncate_version("1.2.3+build.1`", 3)     → "1.2.3"
/// truncate_version("1.2.3-rc.1", 3)        → "1.2.3-rc.1"
/// truncate_version("1.2.3-rc.1", 2)        → "1.2"
fn truncate_version(version: &str, segments: usize) -> String {
    // Strip build metadata unconditionally (`+...`)
    let stripped = version.split('+').next().unwrap_or(version);

    if segments == 0 {
        return stripped.to_owned();
    }

    // Split numeric.dot prefix from any trailing pre-release (`-...`)
    let (numeric, suffix) = stripped
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .map_or((stripped, ""), |i| stripped.split_at(i));

    let parts: Vec<&str> = numeric.split('.').collect();
    if parts.len() <= segments {
        // Already at or below desired precision — keep as-is with any pre-release
        return stripped.to_owned();
    }
    // Truncated: drop any pre-release suffix too
    let _ = suffix;
    parts[..segments].join(".")
}

#[cfg(test)]
mod tests {
    use super::*;
    use dependency_check_updates_core::{
        DcuError, DependencySection, DependencySpec, ResolvedVersion,
    };
    use rstest::rstest;

    /// Owned `(idx, resolved)` batch handed to [`compute_updates`].
    type ResolvedInput = Vec<(usize, Result<ResolvedVersion, DcuError>)>;

    /// Build a `Dependencies`-section spec.
    fn dep(name: &str, current_req: &str) -> DependencySpec {
        DependencySpec {
            name: name.to_owned(),
            current_req: current_req.to_owned(),
            section: DependencySection::Dependencies,
        }
    }

    /// Build the single-dependency input + resolved batch shared by the bulk of
    /// the `compute_updates` cases (`name` is irrelevant to the result).
    fn single(current: &str, latest: &str, selected: &str) -> (Vec<DependencySpec>, ResolvedInput) {
        let deps = vec![dep("pkg", current)];
        let resolved = vec![(
            0,
            Ok(ResolvedVersion {
                latest: Some(latest.to_owned()),
                selected: Some(selected.to_owned()),
            }),
        )];
        (deps, resolved)
    }

    #[rstest]
    // current, registry latest, registry selected, expected `to` (None = skip).
    #[case::basic("^17.0.0", "18.2.0", "18.2.0", Some("^18.2.0"))]
    #[case::already_up_to_date("^18.2.0", "18.2.0", "18.2.0", None)]
    #[case::preserves_tilde("~4.17.0", "4.17.21", "4.17.21", Some("~4.17.21"))]
    #[case::preserves_gte(">=1.0.0", "2.0.0", "2.0.0", Some(">=2.0.0"))]
    #[case::no_prefix("1.0.0", "2.0.0", "2.0.0", Some("2.0.0"))]
    #[case::prerelease_not_truncated_to_stable("3.1", "3.1.0", "4.0.0-beta.0", None)]
    #[case::truncates_plain_three_segment("3.1", "4.0.0", "4.0.0", Some("4.0"))]
    #[case::two_segment_selected_gte(">=4.2", "5.1", "5.1", Some(">=5.1"))]
    #[case::two_segment_selected_no_prefix("4.2", "5.1", "5.1", Some("5.1"))]
    #[case::short_version_upgrade("v5", "6.0.0", "6.0.0", Some("v6"))]
    #[case::blocks_downgrade_short_version("v5", "4.0.0", "4.0.0", None)]
    #[case::respects_major_minor_precision("0.6", "0.6.5", "0.6.5", None)]
    #[case::major_minor_bumps_minor("0.6", "0.7.2", "0.7.2", Some("0.7"))]
    #[case::major_only_bumps_major("1", "2.5.0", "2.5.0", Some("2"))]
    #[case::major_only_stays_same("1", "1.5.0", "1.5.0", None)]
    #[case::full_precision_uses_full_version("1.0.0", "1.0.228", "1.0.228", Some("1.0.228"))]
    #[case::strips_build_metadata(
        "0.25.10",
        "0.25.11+spec-1.1.0",
        "0.25.11+spec-1.1.0",
        Some("0.25.11")
    )]
    #[case::blocks_downgrade_prerelease_to_stable("2.0.0-rc.37", "1.1.20", "1.1.20", None)]
    #[case::blocks_downgrade_same_major("2.5.0", "2.4.0", "2.4.0", None)]
    #[case::allows_prerelease_to_prerelease(
        "2.0.0-rc.37",
        "2.0.0-rc.40",
        "2.0.0-rc.40",
        Some("2.0.0-rc.40")
    )]
    #[case::allows_beta_to_newer_beta(
        "4.0.0-beta.0",
        "4.0.0-beta.2",
        "4.0.0-beta.2",
        Some("4.0.0-beta.2")
    )]
    #[case::allows_prerelease_to_stable("2.0.0-rc.37", "2.0.0", "2.0.0", Some("2.0.0"))]
    #[case::equal_semver_skipped("1.2.3", "1.2.3", "1.2.3", None)]
    fn compute_updates_single(
        #[case] current: &str,
        #[case] latest: &str,
        #[case] selected: &str,
        #[case] expected_to: Option<&str>,
    ) {
        let (deps, resolved) = single(current, latest, selected);
        let updates = compute_updates(&deps, &resolved, ManifestKind::PackageJson);
        match expected_to {
            Some(to) => {
                assert_eq!(
                    updates.len(),
                    1,
                    "expected one update for {current} -> {selected}, got: {updates:?}"
                );
                assert_eq!(updates[0].to, to);
            }
            None => assert!(
                updates.is_empty(),
                "expected no update for {current} -> {selected}, got: {updates:?}"
            ),
        }
    }

    #[test]
    fn compute_updates_sets_package_name() {
        let (deps, resolved) = single("^17.0.0", "18.2.0", "18.2.0");
        let updates = compute_updates(&deps, &resolved, ManifestKind::PackageJson);
        assert_eq!(updates[0].name, "pkg");
    }

    #[test]
    fn compute_updates_skips_failed_resolution() {
        let deps = vec![dep("missing", "^1.0.0")];
        let resolved: ResolvedInput = vec![(
            0,
            Err(DcuError::RegistryLookup {
                package: "missing".to_owned(),
                detail: "not found".to_owned(),
            }),
        )];
        assert!(compute_updates(&deps, &resolved, ManifestKind::PackageJson).is_empty());
    }

    #[test]
    fn compute_updates_skips_no_selected() {
        let deps = vec![dep("pkg", "^1.0.0")];
        let resolved = vec![(
            0,
            Ok(ResolvedVersion {
                latest: None,
                selected: None,
            }),
        )];
        assert!(compute_updates(&deps, &resolved, ManifestKind::PackageJson).is_empty());
    }

    #[test]
    fn compute_updates_multiple_deps() {
        let deps = vec![
            dep("a", "^1.0.0"),
            DependencySpec {
                name: "b".to_owned(),
                current_req: "~2.0.0".to_owned(),
                section: DependencySection::DevDependencies,
            },
            dep("c", "^3.0.0"),
        ];
        let resolved = vec![
            (
                0,
                Ok(ResolvedVersion {
                    latest: Some("1.5.0".to_owned()),
                    selected: Some("1.5.0".to_owned()),
                }),
            ),
            (
                1,
                Ok(ResolvedVersion {
                    latest: Some("2.5.0".to_owned()),
                    selected: Some("2.5.0".to_owned()),
                }),
            ),
            (
                2,
                Ok(ResolvedVersion {
                    latest: Some("3.0.0".to_owned()),
                    selected: Some("3.0.0".to_owned()),
                }),
            ),
        ];
        let updates = compute_updates(&deps, &resolved, ManifestKind::PackageJson);
        // a: ^1.0.0 -> ^1.5.0 (update), b: ~2.0.0 -> ~2.5.0 (update), c: same (no update)
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].name, "a");
        assert_eq!(updates[0].to, "^1.5.0");
        assert_eq!(updates[1].name, "b");
        assert_eq!(updates[1].to, "~2.5.0");
    }

    #[test]
    fn compute_updates_preserves_section() {
        let deps = vec![DependencySpec {
            name: "a".to_owned(),
            current_req: "^1.0.0".to_owned(),
            section: DependencySection::DevDependencies,
        }];
        let resolved = vec![(
            0,
            Ok(ResolvedVersion {
                latest: Some("2.0.0".to_owned()),
                selected: Some("2.0.0".to_owned()),
            }),
        )];
        let updates = compute_updates(&deps, &resolved, ManifestKind::PackageJson);
        assert_eq!(updates[0].section, DependencySection::DevDependencies);
        assert_eq!(updates[0].from, "^1.0.0");
    }

    #[test]
    fn compute_updates_github_skips_precision_truncation() {
        // The GitHub registry already resolved the exact tag form (here an
        // escalated `8.1.0` for a `v7` pin whose `v8` moving tag is missing).
        // compute_updates must emit it verbatim, NOT truncate to the pin's
        // 1-segment precision (which would yield the dangling `v8`).
        let deps = vec![DependencySpec {
            name: "astral-sh/setup-uv".to_owned(),
            current_req: "v7".to_owned(),
            section: DependencySection::GitHubActions,
        }];
        let resolved = vec![(
            0,
            Ok(ResolvedVersion {
                latest: Some("8.1.0".to_owned()),
                selected: Some("8.1.0".to_owned()),
            }),
        )];
        let updates = compute_updates(&deps, &resolved, ManifestKind::GitHubWorkflow);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].to, "v8.1.0");
    }

    #[rstest]
    // dependency names, include filters, exclude filters, expected surviving names.
    #[case::no_filters(&["react", "lodash"], &[], &[], &["react", "lodash"])]
    #[case::include(&["react", "lodash"], &["react"], &[], &["react"])]
    #[case::exclude(&["react", "lodash"], &[], &["lodash"], &["react"])]
    #[case::include_and_exclude(&["react", "react-dom", "lodash"], &["react"], &["react-dom"], &["react"])]
    #[case::partial_match(&["@types/react", "lodash"], &["react"], &[], &["@types/react"])]
    fn filter_deps_cases(
        #[case] names: &[&str],
        #[case] include: &[&str],
        #[case] exclude: &[&str],
        #[case] expected: &[&str],
    ) {
        let deps: Vec<DependencySpec> = names.iter().map(|n| dep(n, "^1.0.0")).collect();
        let include: Vec<String> = include.iter().map(|s| (*s).to_owned()).collect();
        let exclude: Vec<String> = exclude.iter().map(|s| (*s).to_owned()).collect();
        let result = filter_deps(&deps, &include, &exclude);
        let got: Vec<&str> = result.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(got, expected);
    }

    #[rstest]
    #[case("5", "5.0.0")]
    #[case("5.1", "5.1.0")]
    #[case("5.1.0", "5.1.0")]
    #[case("5.1.2.3", "5.1.2.3")] // 4+ segments left as-is
    #[case("5.1.0-rc.1", "5.1.0-rc.1")]
    #[case("1.2-beta", "1.2.0-beta")]
    #[case("5-beta", "5.0.0-beta")]
    #[case("", "")]
    fn pad_to_three_segments_cases(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(pad_to_three_segments(input), expected);
    }

    #[rstest]
    #[case("1", 1)]
    #[case("1.0", 2)]
    #[case("1.0.0", 3)]
    #[case("1.0.0-beta.1", 3)]
    #[case("", 0)]
    fn count_version_segments_cases(#[case] input: &str, #[case] expected: usize) {
        assert_eq!(count_version_segments(input), expected);
    }

    #[rstest]
    #[case("1.2.3+build.7", 0, "1.2.3")] // segments=0 keeps stripped version
    #[case("1.2.3", 2, "1.2")]
    #[case("1.2.3", 3, "1.2.3")]
    #[case("1.2.3", 1, "1")]
    #[case("1.2", 3, "1.2")] // cannot extend
    #[case("0.25.11+spec-1.1.0", 3, "0.25.11")] // strip build metadata
    #[case("1.2.3-rc.1", 3, "1.2.3-rc.1")] // preserve pre-release
    #[case("1.2.3-rc.1", 2, "1.2")] // drop pre-release when truncating
    fn truncate_version_cases(
        #[case] version: &str,
        #[case] segments: usize,
        #[case] expected: &str,
    ) {
        assert_eq!(truncate_version(version, segments), expected);
    }

    #[rstest]
    // Plain numeric versions of any segment count are truncatable.
    #[case("5", true)]
    #[case("5.1", true)]
    #[case("4.2", true)]
    #[case("4.0.0", true)]
    #[case("1.2.3.4", true)]
    // Pre-release / build suffixes are NOT safe to truncate.
    #[case("4.0.0-beta.0", false)]
    #[case("1.2.3+build", false)]
    #[case("5.1-rc.1", false)]
    // Malformed / empty segments.
    #[case("", false)]
    #[case("5.", false)]
    #[case(".5", false)]
    #[case("v5", false)]
    fn is_plain_numeric_version_cases(#[case] input: &str, #[case] expected: bool) {
        assert_eq!(is_plain_numeric_version(input), expected);
    }
}
