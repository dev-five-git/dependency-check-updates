use tracing::{debug, trace, warn};

use dependency_check_updates_core::{DcuError, DependencySpec, PlannedUpdate, ResolvedVersion};

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

        let selected_truncated = truncate_version(selected, precision);

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

    #[test]
    fn test_compute_updates_basic() {
        let deps = vec![DependencySpec {
            name: "react".to_owned(),
            current_req: "^17.0.0".to_owned(),
            section: DependencySection::Dependencies,
        }];
        let resolved = vec![(
            0,
            Ok(ResolvedVersion {
                latest: Some("18.2.0".to_owned()),
                selected: Some("18.2.0".to_owned()),
            }),
        )];
        let updates = compute_updates(&deps, &resolved);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].name, "react");
        assert_eq!(updates[0].to, "^18.2.0");
    }

    #[test]
    fn test_compute_updates_already_up_to_date() {
        let deps = vec![DependencySpec {
            name: "react".to_owned(),
            current_req: "^18.2.0".to_owned(),
            section: DependencySection::Dependencies,
        }];
        let resolved = vec![(
            0,
            Ok(ResolvedVersion {
                latest: Some("18.2.0".to_owned()),
                selected: Some("18.2.0".to_owned()),
            }),
        )];
        let updates = compute_updates(&deps, &resolved);
        assert!(updates.is_empty());
    }

    #[test]
    fn test_compute_updates_preserves_tilde_prefix() {
        let deps = vec![DependencySpec {
            name: "lodash".to_owned(),
            current_req: "~4.17.0".to_owned(),
            section: DependencySection::Dependencies,
        }];
        let resolved = vec![(
            0,
            Ok(ResolvedVersion {
                latest: Some("4.17.21".to_owned()),
                selected: Some("4.17.21".to_owned()),
            }),
        )];
        let updates = compute_updates(&deps, &resolved);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].to, "~4.17.21");
    }

    #[test]
    fn test_compute_updates_preserves_gte_prefix() {
        let deps = vec![DependencySpec {
            name: "pkg".to_owned(),
            current_req: ">=1.0.0".to_owned(),
            section: DependencySection::Dependencies,
        }];
        let resolved = vec![(
            0,
            Ok(ResolvedVersion {
                latest: Some("2.0.0".to_owned()),
                selected: Some("2.0.0".to_owned()),
            }),
        )];
        let updates = compute_updates(&deps, &resolved);
        assert_eq!(updates[0].to, ">=2.0.0");
    }

    #[test]
    fn test_compute_updates_no_prefix() {
        let deps = vec![DependencySpec {
            name: "pkg".to_owned(),
            current_req: "1.0.0".to_owned(),
            section: DependencySection::Dependencies,
        }];
        let resolved = vec![(
            0,
            Ok(ResolvedVersion {
                latest: Some("2.0.0".to_owned()),
                selected: Some("2.0.0".to_owned()),
            }),
        )];
        let updates = compute_updates(&deps, &resolved);
        assert_eq!(updates[0].to, "2.0.0");
    }

    #[test]
    fn test_compute_updates_does_not_truncate_prerelease_to_stable() {
        let deps = vec![DependencySpec {
            name: "pkg".to_owned(),
            current_req: "3.1".to_owned(),
            section: DependencySection::Dependencies,
        }];
        let resolved = vec![(
            0,
            Ok(ResolvedVersion {
                latest: Some("3.1.0".to_owned()),
                selected: Some("4.0.0-beta.0".to_owned()),
            }),
        )];
        let updates = compute_updates(&deps, &resolved);
        assert!(updates.is_empty());
    }

    #[test]
    fn test_is_plain_numeric_version() {
        // Plain numeric versions of any segment count are truncatable.
        assert!(is_plain_numeric_version("5"));
        assert!(is_plain_numeric_version("5.1"));
        assert!(is_plain_numeric_version("4.2"));
        assert!(is_plain_numeric_version("4.0.0"));
        assert!(is_plain_numeric_version("1.2.3.4"));
        // Pre-release / build suffixes are NOT safe to truncate.
        assert!(!is_plain_numeric_version("4.0.0-beta.0"));
        assert!(!is_plain_numeric_version("1.2.3+build"));
        assert!(!is_plain_numeric_version("5.1-rc.1"));
        // Malformed / empty segments.
        assert!(!is_plain_numeric_version(""));
        assert!(!is_plain_numeric_version("5."));
        assert!(!is_plain_numeric_version(".5"));
        assert!(!is_plain_numeric_version("v5"));
    }

    #[test]
    fn test_compute_updates_two_segment_selected_truncatable() {
        // Django-style 2-segment versioning: current pinned at 2-segment
        // precision, registry resolves a higher 2-segment version (e.g. via
        // `-t greatest`/`newest`). The clean 2-segment stable MUST be applied,
        // not silently skipped as "cannot be safely truncated".
        let deps = vec![DependencySpec {
            name: "django".to_owned(),
            current_req: ">=4.2".to_owned(),
            section: DependencySection::Dependencies,
        }];
        let resolved = vec![(
            0,
            Ok(ResolvedVersion {
                latest: Some("5.1".to_owned()),
                selected: Some("5.1".to_owned()),
            }),
        )];
        let updates = compute_updates(&deps, &resolved);
        assert_eq!(updates.len(), 1, "2-segment stable must update, got: {updates:?}");
        assert_eq!(updates[0].to, ">=5.1");
    }

    #[test]
    fn test_compute_updates_two_segment_selected_no_prefix() {
        // Same as above but a bare 2-segment pin with no range operator.
        let deps = vec![DependencySpec {
            name: "django".to_owned(),
            current_req: "4.2".to_owned(),
            section: DependencySection::Dependencies,
        }];
        let resolved = vec![(
            0,
            Ok(ResolvedVersion {
                latest: Some("5.1".to_owned()),
                selected: Some("5.1".to_owned()),
            }),
        )];
        let updates = compute_updates(&deps, &resolved);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].to, "5.1");
    }

    #[test]
    fn test_compute_updates_truncates_plain_three_segment_version() {
        let deps = vec![DependencySpec {
            name: "pkg".to_owned(),
            current_req: "3.1".to_owned(),
            section: DependencySection::Dependencies,
        }];
        let resolved = vec![(
            0,
            Ok(ResolvedVersion {
                latest: Some("4.0.0".to_owned()),
                selected: Some("4.0.0".to_owned()),
            }),
        )];
        let updates = compute_updates(&deps, &resolved);
        assert_eq!(updates[0].to, "4.0");
    }

    #[test]
    fn test_compute_updates_skips_failed_resolution() {
        let deps = vec![DependencySpec {
            name: "missing".to_owned(),
            current_req: "^1.0.0".to_owned(),
            section: DependencySection::Dependencies,
        }];
        let resolved: Vec<(usize, Result<ResolvedVersion, DcuError>)> = vec![(
            0,
            Err(DcuError::RegistryLookup {
                package: "missing".to_owned(),
                detail: "not found".to_owned(),
            }),
        )];
        let updates = compute_updates(&deps, &resolved);
        assert!(updates.is_empty());
    }

    #[test]
    fn test_compute_updates_skips_no_selected() {
        let deps = vec![DependencySpec {
            name: "pkg".to_owned(),
            current_req: "^1.0.0".to_owned(),
            section: DependencySection::Dependencies,
        }];
        let resolved = vec![(
            0,
            Ok(ResolvedVersion {
                latest: None,
                selected: None,
            }),
        )];
        let updates = compute_updates(&deps, &resolved);
        assert!(updates.is_empty());
    }

    #[test]
    fn test_filter_deps_no_filters() {
        let deps = vec![
            DependencySpec {
                name: "react".to_owned(),
                current_req: "^18.0.0".to_owned(),
                section: DependencySection::Dependencies,
            },
            DependencySpec {
                name: "lodash".to_owned(),
                current_req: "^4.0.0".to_owned(),
                section: DependencySection::Dependencies,
            },
        ];
        let result = filter_deps(&deps, &[], &[]);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_filter_deps_include() {
        let deps = vec![
            DependencySpec {
                name: "react".to_owned(),
                current_req: "^18.0.0".to_owned(),
                section: DependencySection::Dependencies,
            },
            DependencySpec {
                name: "lodash".to_owned(),
                current_req: "^4.0.0".to_owned(),
                section: DependencySection::Dependencies,
            },
        ];
        let include = vec!["react".to_owned()];
        let result = filter_deps(&deps, &include, &[]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "react");
    }

    #[test]
    fn test_filter_deps_exclude() {
        let deps = vec![
            DependencySpec {
                name: "react".to_owned(),
                current_req: "^18.0.0".to_owned(),
                section: DependencySection::Dependencies,
            },
            DependencySpec {
                name: "lodash".to_owned(),
                current_req: "^4.0.0".to_owned(),
                section: DependencySection::Dependencies,
            },
        ];
        let exclude = vec!["lodash".to_owned()];
        let result = filter_deps(&deps, &[], &exclude);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "react");
    }

    #[test]
    fn test_filter_deps_include_and_exclude() {
        let deps = vec![
            DependencySpec {
                name: "react".to_owned(),
                current_req: "^18.0.0".to_owned(),
                section: DependencySection::Dependencies,
            },
            DependencySpec {
                name: "react-dom".to_owned(),
                current_req: "^18.0.0".to_owned(),
                section: DependencySection::Dependencies,
            },
            DependencySpec {
                name: "lodash".to_owned(),
                current_req: "^4.0.0".to_owned(),
                section: DependencySection::Dependencies,
            },
        ];
        let include = vec!["react".to_owned()];
        let exclude = vec!["react-dom".to_owned()];
        let result = filter_deps(&deps, &include, &exclude);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "react");
    }

    #[test]
    fn test_filter_deps_partial_match() {
        let deps = vec![
            DependencySpec {
                name: "@types/react".to_owned(),
                current_req: "^18.0.0".to_owned(),
                section: DependencySection::Dependencies,
            },
            DependencySpec {
                name: "lodash".to_owned(),
                current_req: "^4.0.0".to_owned(),
                section: DependencySection::Dependencies,
            },
        ];
        let include = vec!["react".to_owned()];
        let result = filter_deps(&deps, &include, &[]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "@types/react");
    }

    #[test]
    fn test_compute_updates_multiple_deps() {
        let deps = vec![
            DependencySpec {
                name: "a".to_owned(),
                current_req: "^1.0.0".to_owned(),
                section: DependencySection::Dependencies,
            },
            DependencySpec {
                name: "b".to_owned(),
                current_req: "~2.0.0".to_owned(),
                section: DependencySection::DevDependencies,
            },
            DependencySpec {
                name: "c".to_owned(),
                current_req: "^3.0.0".to_owned(),
                section: DependencySection::Dependencies,
            },
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
        let updates = compute_updates(&deps, &resolved);
        // a: ^1.0.0 -> ^1.5.0 (update), b: ~2.0.0 -> ~2.5.0 (update), c: same (no update)
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].name, "a");
        assert_eq!(updates[0].to, "^1.5.0");
        assert_eq!(updates[1].name, "b");
        assert_eq!(updates[1].to, "~2.5.0");
    }

    #[test]
    fn test_pad_to_three_segments() {
        assert_eq!(pad_to_three_segments("5"), "5.0.0");
        assert_eq!(pad_to_three_segments("5.1"), "5.1.0");
        assert_eq!(pad_to_three_segments("5.1.0"), "5.1.0");
        assert_eq!(pad_to_three_segments("5.1.2.3"), "5.1.2.3"); // 4+ left as-is
        assert_eq!(pad_to_three_segments("5.1.0-rc.1"), "5.1.0-rc.1");
        assert_eq!(pad_to_three_segments("1.2-beta"), "1.2.0-beta");
        assert_eq!(pad_to_three_segments("5-beta"), "5.0.0-beta");
        assert_eq!(pad_to_three_segments(""), "");
    }

    #[test]
    fn test_compute_updates_blocks_downgrade_short_version() {
        // Regression: `v5` should not be downgraded to `v4` even though semver
        // parse of bare "5" fails — the padded path now catches this.
        let deps = vec![DependencySpec {
            name: "actions/checkout".to_owned(),
            current_req: "v5".to_owned(),
            section: DependencySection::Dependencies,
        }];
        let resolved = vec![(
            0,
            Ok(ResolvedVersion {
                latest: Some("4.0.0".to_owned()),
                selected: Some("4.0.0".to_owned()),
            }),
        )];
        let updates = compute_updates(&deps, &resolved);
        assert!(
            updates.is_empty(),
            "must not downgrade v5 → v4, got: {updates:?}"
        );
    }

    #[test]
    fn test_compute_updates_short_version_upgrade() {
        // v5 → registry returns 6.0.0 → output v6 (precision-truncated).
        let deps = vec![DependencySpec {
            name: "actions/checkout".to_owned(),
            current_req: "v5".to_owned(),
            section: DependencySection::Dependencies,
        }];
        let resolved = vec![(
            0,
            Ok(ResolvedVersion {
                latest: Some("6.0.0".to_owned()),
                selected: Some("6.0.0".to_owned()),
            }),
        )];
        let updates = compute_updates(&deps, &resolved);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].to, "v6");
    }

    #[test]
    fn test_count_version_segments() {
        assert_eq!(count_version_segments("1"), 1);
        assert_eq!(count_version_segments("1.0"), 2);
        assert_eq!(count_version_segments("1.0.0"), 3);
        assert_eq!(count_version_segments("1.0.0-beta.1"), 3);
        assert_eq!(count_version_segments(""), 0);
    }

    #[test]
    fn test_truncate_version() {
        assert_eq!(truncate_version("1.2.3+build.7", 0), "1.2.3"); // segments=0 keeps stripped version
        assert_eq!(truncate_version("1.2.3", 2), "1.2");
        assert_eq!(truncate_version("1.2.3", 3), "1.2.3");
        assert_eq!(truncate_version("1.2.3", 1), "1");
        assert_eq!(truncate_version("1.2", 3), "1.2"); // cannot extend
        assert_eq!(truncate_version("0.25.11+spec-1.1.0", 3), "0.25.11"); // strip build metadata
        assert_eq!(truncate_version("1.2.3-rc.1", 3), "1.2.3-rc.1"); // preserve pre-release
        assert_eq!(truncate_version("1.2.3-rc.1", 2), "1.2"); // drop pre-release when truncating
    }

    #[test]
    fn test_compute_updates_respects_major_minor_precision() {
        // current = "0.6" (2 segments), latest = "0.6.5" → no update needed
        let deps = vec![DependencySpec {
            name: "wiremock".to_owned(),
            current_req: "0.6".to_owned(),
            section: DependencySection::Dependencies,
        }];
        let resolved = vec![(
            0,
            Ok(ResolvedVersion {
                latest: Some("0.6.5".to_owned()),
                selected: Some("0.6.5".to_owned()),
            }),
        )];
        let updates = compute_updates(&deps, &resolved);
        assert!(updates.is_empty(), "0.6 should not be rewritten to 0.6.5");
    }

    #[test]
    fn test_compute_updates_major_minor_bumps_minor() {
        // current = "0.6" (2 segments), latest = "0.7.2" → update to "0.7"
        let deps = vec![DependencySpec {
            name: "wiremock".to_owned(),
            current_req: "0.6".to_owned(),
            section: DependencySection::Dependencies,
        }];
        let resolved = vec![(
            0,
            Ok(ResolvedVersion {
                latest: Some("0.7.2".to_owned()),
                selected: Some("0.7.2".to_owned()),
            }),
        )];
        let updates = compute_updates(&deps, &resolved);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].to, "0.7");
    }

    #[test]
    fn test_compute_updates_major_only_bumps_major() {
        // current = "1" (1 segment), latest = "2.5.0" → update to "2"
        let deps = vec![DependencySpec {
            name: "pkg".to_owned(),
            current_req: "1".to_owned(),
            section: DependencySection::Dependencies,
        }];
        let resolved = vec![(
            0,
            Ok(ResolvedVersion {
                latest: Some("2.5.0".to_owned()),
                selected: Some("2.5.0".to_owned()),
            }),
        )];
        let updates = compute_updates(&deps, &resolved);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].to, "2");
    }

    #[test]
    fn test_compute_updates_major_only_stays_same() {
        // current = "1" (1 segment), latest = "1.5.0" → no update
        let deps = vec![DependencySpec {
            name: "pkg".to_owned(),
            current_req: "1".to_owned(),
            section: DependencySection::Dependencies,
        }];
        let resolved = vec![(
            0,
            Ok(ResolvedVersion {
                latest: Some("1.5.0".to_owned()),
                selected: Some("1.5.0".to_owned()),
            }),
        )];
        let updates = compute_updates(&deps, &resolved);
        assert!(updates.is_empty());
    }

    #[test]
    fn test_compute_updates_full_precision_uses_full_version() {
        // current = "1.0.0" (3 segments), latest = "1.0.228" → update to "1.0.228"
        let deps = vec![DependencySpec {
            name: "serde".to_owned(),
            current_req: "1.0.0".to_owned(),
            section: DependencySection::Dependencies,
        }];
        let resolved = vec![(
            0,
            Ok(ResolvedVersion {
                latest: Some("1.0.228".to_owned()),
                selected: Some("1.0.228".to_owned()),
            }),
        )];
        let updates = compute_updates(&deps, &resolved);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].to, "1.0.228");
    }

    #[test]
    fn test_compute_updates_strips_build_metadata() {
        // current = "0.25.10" (3 segments), latest = "0.25.11+spec-1.1.0" → "0.25.11" (no +metadata)
        let deps = vec![DependencySpec {
            name: "toml_edit".to_owned(),
            current_req: "0.25.10".to_owned(),
            section: DependencySection::Dependencies,
        }];
        let resolved = vec![(
            0,
            Ok(ResolvedVersion {
                latest: Some("0.25.11+spec-1.1.0".to_owned()),
                selected: Some("0.25.11+spec-1.1.0".to_owned()),
            }),
        )];
        let updates = compute_updates(&deps, &resolved);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].to, "0.25.11");
    }

    #[test]
    fn test_compute_updates_blocks_downgrade_from_prerelease_to_stable() {
        // Regression test for the sea-orm 2.0.0-rc.37 -> 1.1.20 bug.
        // When the current version is a higher prerelease (2.0.0-rc.37) and
        // the registry filtering returns an older stable (1.1.20), the
        // safety net MUST skip this update instead of suggesting a downgrade.
        let deps = vec![DependencySpec {
            name: "sea-orm".to_owned(),
            current_req: "2.0.0-rc.37".to_owned(),
            section: DependencySection::Dependencies,
        }];
        let resolved = vec![(
            0,
            Ok(ResolvedVersion {
                latest: Some("1.1.20".to_owned()),
                selected: Some("1.1.20".to_owned()),
            }),
        )];
        let updates = compute_updates(&deps, &resolved);
        assert!(
            updates.is_empty(),
            "must not suggest downgrade from 2.0.0-rc.37 to 1.1.20, got: {updates:?}"
        );
    }

    #[test]
    fn test_compute_updates_blocks_downgrade_same_major() {
        // Current is newer stable; registry returned something older. Skip.
        let deps = vec![DependencySpec {
            name: "pkg".to_owned(),
            current_req: "2.5.0".to_owned(),
            section: DependencySection::Dependencies,
        }];
        let resolved = vec![(
            0,
            Ok(ResolvedVersion {
                latest: Some("2.4.0".to_owned()),
                selected: Some("2.4.0".to_owned()),
            }),
        )];
        let updates = compute_updates(&deps, &resolved);
        assert!(updates.is_empty(), "must not downgrade 2.5.0 -> 2.4.0");
    }

    #[test]
    fn test_compute_updates_allows_prerelease_to_prerelease_upgrade() {
        // Current: 2.0.0-rc.37, Selected: 2.0.0-rc.40 → valid upgrade.
        let deps = vec![DependencySpec {
            name: "sea-orm".to_owned(),
            current_req: "2.0.0-rc.37".to_owned(),
            section: DependencySection::Dependencies,
        }];
        let resolved = vec![(
            0,
            Ok(ResolvedVersion {
                latest: Some("2.0.0-rc.40".to_owned()),
                selected: Some("2.0.0-rc.40".to_owned()),
            }),
        )];
        let updates = compute_updates(&deps, &resolved);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].to, "2.0.0-rc.40");
    }

    #[test]
    fn test_compute_updates_allows_beta_to_newer_beta_upgrade() {
        let deps = vec![DependencySpec {
            name: "pkg".to_owned(),
            current_req: "4.0.0-beta.0".to_owned(),
            section: DependencySection::Dependencies,
        }];
        let resolved = vec![(
            0,
            Ok(ResolvedVersion {
                latest: Some("4.0.0-beta.2".to_owned()),
                selected: Some("4.0.0-beta.2".to_owned()),
            }),
        )];
        let updates = compute_updates(&deps, &resolved);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].to, "4.0.0-beta.2");
    }

    #[test]
    fn test_compute_updates_allows_prerelease_to_stable_upgrade() {
        // Current: 2.0.0-rc.37 (prerelease), Selected: 2.0.0 (stable) → semver: stable > prerelease of same version.
        let deps = vec![DependencySpec {
            name: "sea-orm".to_owned(),
            current_req: "2.0.0-rc.37".to_owned(),
            section: DependencySection::Dependencies,
        }];
        let resolved = vec![(
            0,
            Ok(ResolvedVersion {
                latest: Some("2.0.0".to_owned()),
                selected: Some("2.0.0".to_owned()),
            }),
        )];
        let updates = compute_updates(&deps, &resolved);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].to, "2.0.0");
    }

    #[test]
    fn test_compute_updates_equal_semver_skipped() {
        // Exact same version: must skip (not a "downgrade", but not an upgrade either).
        let deps = vec![DependencySpec {
            name: "pkg".to_owned(),
            current_req: "1.2.3".to_owned(),
            section: DependencySection::Dependencies,
        }];
        let resolved = vec![(
            0,
            Ok(ResolvedVersion {
                latest: Some("1.2.3".to_owned()),
                selected: Some("1.2.3".to_owned()),
            }),
        )];
        let updates = compute_updates(&deps, &resolved);
        assert!(updates.is_empty());
    }

    #[test]
    fn test_compute_updates_preserves_section() {
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
        let updates = compute_updates(&deps, &resolved);
        assert_eq!(updates[0].section, DependencySection::DevDependencies);
        assert_eq!(updates[0].from, "^1.0.0");
    }
}
