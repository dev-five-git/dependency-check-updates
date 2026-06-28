use std::borrow::Cow;

use tracing::{debug, trace, warn};

use dependency_check_updates_core::{
    DcuError, DependencySpec, ManifestKind, PlannedUpdate, ResolvedVersion, pad_to_three_segments,
    split_numeric_head, strip_range_prefix,
};

/// Filter dependencies by include/exclude patterns.
///
/// Consumes `deps` so kept specs move into the returned vec — no `String`
/// clones on the default code path where neither filter is active and every
/// dependency survives.
pub(crate) fn filter_deps(
    deps: Vec<DependencySpec>,
    include: &[String],
    exclude: &[String],
) -> Vec<DependencySpec> {
    deps.into_iter()
        .filter(|dep| {
            if !include.is_empty() && !include.iter().any(|f| dep.name.contains(f.as_str())) {
                return false;
            }
            if exclude.iter().any(|x| dep.name.contains(x.as_str())) {
                return false;
            }
            true
        })
        .collect()
}

/// Re-attach the range prefix from `current_req` onto a new bare version.
///
/// `current_bare` MUST be the result of `strip_range_prefix(current_req)` — the
/// length difference is the leading non-digit prefix (`^`, `~`, `>=`,
/// `">= "`, …) that needs to be re-glued onto `new_bare`. Centralises the
/// expression previously duplicated in `compute_updates` and `sync_path_dep`,
/// so future range-prefix tightenings land in exactly one place.
fn rewrite_with_range_prefix(current_req: &str, current_bare: &str, new_bare: &str) -> String {
    let prefix = &current_req[..current_req.len() - current_bare.len()];
    format!("{prefix}{new_bare}")
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

        // Local path dependency: exact-sync the `version` field to the crate
        // on disk. The path crate's actual version is the source of truth, so
        // this bypasses the never-downgrade safety net below — Cargo requires
        // the requirement to be satisfiable by the local crate's version.
        if dep.path_version.is_some() {
            if let Some(update) = sync_path_dep(dep, selected) {
                debug!(name = %update.name, from = %update.from, to = %update.to, "path dep sync");
                updates.push(update);
            } else {
                trace!(package = %dep.name, version = %dep.current_req, "path dep already in sync");
            }
            continue;
        }

        // Strip range prefix for comparison
        let current_bare = strip_range_prefix(&dep.current_req);

        // Compound ranges (`^17 || ^18`, `>=1.0, <2.0`, `>=18 <19`) carry
        // multiple clauses; the prefix-reuse rewrite below would keep only
        // the first clause and silently drop the rest, violating the
        // manifest's format-preservation contract. Leave them untouched.
        if is_compound_range(current_bare) {
            trace!(
                package = %dep.name,
                current = %dep.current_req,
                selected = %selected,
                "skipping: compound version range (OR/AND clauses not supported)"
            );
            continue;
        }

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
        ) {
            if sel_ver <= cur_ver {
                trace!(
                    package = %dep.name,
                    current = %dep.current_req,
                    selected = %selected,
                    "skipping: selected version is not newer than current"
                );
                continue;
            }
        }

        // Preserve precision: if the user wrote "0.6" (2 segments), truncate the
        // resolved version to 2 segments before comparing. This respects the user's
        // intent to pin only at that granularity.
        //
        // GitHub workflow refs are exempt: the GitHub registry already resolved
        // the exact, tag-validated ref form (`pick_existing_ref`), so re-running
        // the generic truncation here could re-shorten an escalated ref
        // (`v8.1.0` → `v8`) back into a dangling tag.
        let selected_truncated: Cow<'_, str> = if kind == ManifestKind::GitHubWorkflow {
            Cow::Borrowed(selected)
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

            Cow::Owned(truncate_version(selected, precision))
        };

        if current_bare == selected_truncated.as_ref() {
            trace!(package = %dep.name, version = %dep.current_req, "already up to date");
            continue;
        }

        // Preserve the range prefix from the original spec
        let new_version =
            rewrite_with_range_prefix(&dep.current_req, current_bare, &selected_truncated);

        updates.push(PlannedUpdate {
            name: dep.name.clone(),
            section: dep.section,
            from: dep.current_req.clone(),
            to: new_version,
        });
    }

    updates
}

/// Compute the exact-sync update for a local path dependency.
///
/// The `version` field of a `{ path = "...", version = "..." }` dependency is
/// synced to `local_version` (the version of the crate on disk). Unlike the
/// registry path this allows "downgrades": if the local crate is *older* than
/// the declared requirement the field is lowered to match, because Cargo
/// requires the requirement to be satisfiable by the path crate's version.
///
/// The range prefix (`^`, `~`, `>=`, …) and the user's pin precision are
/// preserved for plain numeric versions (`0.2` → `0.3`); otherwise the full
/// local version is written (build metadata stripped, pre-release preserved).
/// Returns `None` when the field is already in sync.
fn sync_path_dep(dep: &DependencySpec, local_version: &str) -> Option<PlannedUpdate> {
    let current_bare = strip_range_prefix(&dep.current_req);
    if current_bare.is_empty() {
        // e.g. `version = "*"` — already matches any version, nothing to sync.
        return None;
    }

    let precision = count_version_segments(current_bare);
    let new_bare = if precision < 3 && is_plain_numeric_version(local_version) {
        truncate_version(local_version, precision)
    } else {
        // Full version: strip build metadata (`+...`), keep any pre-release.
        local_version
            .split_once('+')
            .map_or(local_version, |(head, _)| head)
            .to_owned()
    };

    if current_bare == new_bare {
        return None;
    }

    Some(PlannedUpdate {
        name: dep.name.clone(),
        section: dep.section,
        from: dep.current_req.clone(),
        to: rewrite_with_range_prefix(&dep.current_req, current_bare, &new_bare),
    })
}

/// True when `current_bare` represents a compound version range — multiple
/// clauses joined by `||` (npm OR), `,` (Cargo / `PyPI` AND), or an internal
/// space (npm AND, e.g. `">=18.0.0 <19.0.0"`, or the npm hyphen-range form
/// `"1.2.3 - 1.5.0"` meaning `>=1.2.3 <=1.5.0`).
///
/// Single clauses with a leading-operator space like `">= 1.0.0"` are NOT
/// compound: `strip_range_prefix` already removed the leading non-digit run
/// (including the space), so this helper sees `"1.0.0"` and returns `false`.
///
/// `compute_updates` cannot rewrite compound ranges without losing user
/// intent — every clause beyond the first would be silently dropped when
/// the registry-resolved version is reprefixed onto the original spec. The
/// safe answer is to leave the manifest byte-identical until a real
/// multi-clause rewriter exists.
fn is_compound_range(current_bare: &str) -> bool {
    if current_bare.contains("||") || current_bare.contains(',') {
        return true;
    }
    // npm AND: a space whose left neighbour is a digit and whose right
    // neighbour is a clause start (digit or one of `<>=~^!`) — or `-`, the
    // npm hyphen-range continuation (`A.B.C - X.Y.Z`). Iterating bytes is
    // safe because every character we test against is ASCII — a non-ASCII
    // byte cannot equal `b' '` or be a digit / operator anyway.
    let bytes = current_bare.as_bytes();
    for i in 1..bytes.len().saturating_sub(1) {
        if bytes[i] == b' '
            && bytes[i - 1].is_ascii_digit()
            && matches!(
                bytes[i + 1],
                b'<' | b'>' | b'=' | b'~' | b'^' | b'!' | b'-' | b'0'..=b'9'
            )
        {
            return true;
        }
    }
    false
}

/// Count the number of version segments in a bare version string.
///
/// "1"      → 1 (major only)
/// "1.0"    → 2 (major.minor)
/// "1.0.0"  → 3 (major.minor.patch)
/// "1.0.0-beta.1" → 3 (pre-release suffix ignored)
fn count_version_segments(bare: &str) -> usize {
    // Stop at the first non-digit, non-dot character (e.g., '-' for pre-release)
    let numeric_part = split_numeric_head(bare).0;
    if numeric_part.is_empty() {
        return 0;
    }
    numeric_part.split('.').filter(|s| !s.is_empty()).count()
}

/// Whether `version` is a plain numeric version — one or more dot-separated
/// segments that are *all* ASCII digits, ignoring any build-metadata tail
/// (`+…`) but still rejecting pre-release tails (`-…`).
///
/// Such versions are always safe to truncate to fewer segments (`5.1` → `5`,
/// `4.0.0` → `4.0`): there is no pre-release tag that could be silently
/// promoted into a stable-looking pin. This intentionally accepts clean
/// two-segment stables like `5.1` (e.g. Django) — the previous
/// exactly-three-segment check rejected them, which made `--target
/// greatest/newest/minor/patch` silently skip such packages whenever the user
/// pinned at <3-segment precision.
///
/// Build metadata is stripped before the digit check so this predicate stays
/// in lock-step with [`truncate_version`], which also drops `+…` before
/// truncating. Without the strip, `0.7.0+build.1` would be rejected here even
/// though the operation this gate guards is provably safe — `0.7.0+build.1`
/// → `0.7`. Pre-release (`-…`) tails are still rejected: silently promoting
/// a prerelease to a stable-looking pin is exactly the surprise this gate
/// guards against.
fn is_plain_numeric_version(version: &str) -> bool {
    let stripped = version.split_once('+').map_or(version, |(head, _)| head);
    let mut any = false;
    for segment in stripped.split('.') {
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
    let stripped = version.split_once('+').map_or(version, |(head, _)| head);

    if segments == 0 {
        return stripped.to_owned();
    }

    // Bare numeric `1.2.3` head — any non-digit, non-dot byte ends the
    // numeric prefix and marks the start of a pre-release tail we drop on
    // truncation (the comparison below decides whether truncation happens).
    let numeric = split_numeric_head(stripped).0;

    if numeric.split('.').count() <= segments {
        // Already at or below desired precision — return `stripped` so any
        // pre-release tail survives unchanged.
        return stripped.to_owned();
    }

    // Truncating: build the result directly from the numeric head without
    // a `Vec<&str>` middleman. Any pre-release suffix is dropped by
    // construction because we only consume `numeric`.
    let mut out = String::with_capacity(numeric.len());
    for (i, part) in numeric.split('.').take(segments).enumerate() {
        if i > 0 {
            out.push('.');
        }
        out.push_str(part);
    }
    out
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
            path_version: None,
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
    // 2-segment pin + selected version with build metadata: the safety gate
    // now strips `+build.1` before checking, matching `truncate_version`, so
    // the dep correctly truncates to `0.7` instead of being silently dropped.
    #[case::truncates_two_segment_with_build_metadata(
        "0.6",
        "0.7.0+build.1",
        "0.7.0+build.1",
        Some("0.7")
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
    // Compound ranges (multiple clauses joined by `||`, `,`, or an internal
    // space) are skipped: rewriting them would drop every clause beyond the
    // first, silently mangling the user's intent. The manifest stays
    // byte-identical until a real multi-clause rewriter exists.
    #[case::npm_or_range_skipped("^17.0.0 || ^18.0.0", "18.3.1", "18.3.1", None)]
    #[case::npm_space_and_range_skipped(">=18.0.0 <19.0.0", "18.3.1", "18.3.1", None)]
    #[case::cargo_comma_and_range_skipped(">=1.0, <2.0", "1.5.0", "1.5.0", None)]
    #[case::pypi_comma_and_range_skipped(">=2.28.0,<3.0", "2.31.0", "2.31.0", None)]
    #[case::npm_hyphen_range_preserved("1.2.3 - 1.5.0", "2.0.0", "2.0.0", None)]
    // Single clause with a leading-operator space (`>= 1.0.0`): the space
    // sits before the digit run and `strip_range_prefix` removes it along
    // with `>=`, so the helper sees a clean `"1.0.0"` and the dep still
    // updates. The prefix on the rewritten value preserves the original
    // operator + space exactly.
    #[case::single_clause_with_leading_space_still_updates(
        ">= 1.0.0",
        "1.5.0",
        "1.5.0",
        Some(">= 1.5.0")
    )]
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
                path_version: None,
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
            path_version: None,
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
            path_version: None,
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

    /// Build a single path-dependency input: `current_req` is what the manifest
    /// declares, `local` is the version of the crate on disk (carried via
    /// `path_version` and echoed by the short-circuiting registry as `selected`).
    fn path_dep_input(current: &str, local: &str) -> (Vec<DependencySpec>, ResolvedInput) {
        let deps = vec![DependencySpec {
            name: "hwp".to_owned(),
            current_req: current.to_owned(),
            section: DependencySection::Dependencies,
            path_version: Some(local.to_owned()),
        }];
        let resolved = vec![(
            0,
            Ok(ResolvedVersion {
                latest: Some(local.to_owned()),
                selected: Some(local.to_owned()),
            }),
        )];
        (deps, resolved)
    }

    #[rstest]
    // current manifest version, local crate version, expected `to` (None = no update).
    #[case::upgrade("0.2.0", "0.3.0", Some("0.3.0"))]
    // Exact sync allows a downgrade — the never-downgrade safety net is bypassed
    // for path deps because the local crate's version is the source of truth.
    #[case::downgrade("0.3.0", "0.2.0", Some("0.2.0"))]
    #[case::already_in_sync("0.3.0", "0.3.0", None)]
    #[case::preserves_caret("^0.2.0", "0.3.0", Some("^0.3.0"))]
    #[case::preserves_tilde("~0.2.0", "0.3.0", Some("~0.3.0"))]
    // Pin precision preserved for plain numeric local versions.
    #[case::preserves_two_segment_precision("0.2", "0.3.1", Some("0.3"))]
    // Same precision-preservation, but the local crate carries build metadata
    // (`+build`). The safety gate now strips it before the digit check, so
    // the manifest's 2-segment precision is preserved (`0.3` instead of the
    // previous fall-through to the full `0.3.0`).
    #[case::path_dep_two_segment_local_with_build_metadata("0.2", "0.3.0+build", Some("0.3"))]
    #[case::full_version_at_three_segments("0.2.0", "0.3.1", Some("0.3.1"))]
    fn compute_updates_path_dep_cases(
        #[case] current: &str,
        #[case] local: &str,
        #[case] expected: Option<&str>,
    ) {
        let (deps, resolved) = path_dep_input(current, local);
        let updates = compute_updates(&deps, &resolved, ManifestKind::CargoToml);
        match expected {
            Some(to) => {
                assert_eq!(updates.len(), 1, "expected one update, got: {updates:?}");
                assert_eq!(updates[0].to, to);
                assert_eq!(updates[0].from, current);
            }
            None => assert!(updates.is_empty(), "expected no update, got: {updates:?}"),
        }
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
        let result = filter_deps(deps, &include, &exclude);
        let got: Vec<&str> = result.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(got, expected);
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
    // Build metadata (`+…`) is stripped before checking — the operation this
    // gate guards (`truncate_version`) drops it too, so these are safe.
    #[case("1.2.3+build", true)]
    #[case("4.0.0+build.7", true)]
    #[case("5.1+meta-7", true)]
    // Pre-release (`-…`) is still rejected; a prerelease must never be
    // silently promoted to a stable-looking pin.
    #[case("4.0.0-beta.0", false)]
    #[case("5.1-rc.1", false)]
    // Pre-release present even after stripping build metadata: still unsafe.
    #[case("4.0.0-beta+build", false)]
    // Malformed / empty segments.
    #[case("", false)]
    #[case("5.", false)]
    #[case(".5", false)]
    #[case("v5", false)]
    fn is_plain_numeric_version_cases(#[case] input: &str, #[case] expected: bool) {
        assert_eq!(is_plain_numeric_version(input), expected);
    }

    #[rstest]
    // Compound — multiple clauses joined by `||`, `,`, or an internal space.
    #[case::or_clauses_with_spaces("17.0.0 || ^18.0.0", true)]
    #[case::or_clauses_tight("17.0.0||18.0.0", true)]
    #[case::cargo_comma_and("1.0, <2.0", true)]
    #[case::pypi_comma_and("2.28.0,<3.0", true)]
    #[case::npm_space_and_lt("18.0.0 <19.0.0", true)]
    #[case::npm_space_and_caret("18.0.0 ^19.0.0", true)]
    #[case::npm_space_and_tilde("18.0.0 ~19.0.0", true)]
    #[case::npm_space_and_eq("18.0.0 =19.0.0", true)]
    #[case::npm_space_and_bang("18.0.0 !=19.0.0", true)]
    #[case::npm_space_and_digit("18.0.0 19.0.0", true)]
    // Single clauses — must NOT be classified as compound.
    #[case::single_full("1.2.3", false)]
    #[case::single_two("1.2", false)]
    #[case::single_major("1", false)]
    #[case::single_prerelease("1.2.3-rc.1", false)]
    #[case::single_with_build("1.2.3+build.7", false)]
    // After `strip_range_prefix` the leading operator (and any space that
    // follows it) is already gone, so a permissive `">= 1.0.0"` arrives
    // here as `"1.0.0"` and stays a single clause.
    #[case::leading_space_stripped("1.0.0", false)]
    #[case::empty("", false)]
    // npm hyphen ranges (`1.2.3 - 1.5.0` meaning `>=1.2.3 <=1.5.0`): the
    // right-of-space byte is `-`, which is treated as a clause-start
    // continuation so the dep is left byte-identical instead of being
    // silently rewritten to a single bare version.
    #[case::npm_hyphen_range_caught("1.2.3 - 1.5.0", true)]
    fn is_compound_range_cases(#[case] input: &str, #[case] expected: bool) {
        assert_eq!(is_compound_range(input), expected);
    }
}
