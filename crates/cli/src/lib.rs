//! dependency-check-updates CLI — check and update package dependencies.

mod output;

use std::path::PathBuf;

use clap::Parser;
use tracing::{debug, info, trace, warn};

use dependency_check_updates_core::manifest::ManifestHandler;
use dependency_check_updates_core::{
    DcuError, DependencySpec, ManifestKind, PlannedUpdate, ResolvedVersion, Scanner, TargetLevel,
};
use dependency_check_updates_node::{NodeHandler, NpmRegistry};
use dependency_check_updates_python::{PyPiRegistry, PythonHandler};
use dependency_check_updates_rust::{CratesIoRegistry, RustHandler};

/// dependency-check-updates — check and update package dependencies
#[derive(Parser, Debug)]
#[command(
    name = "dependency-check-updates",
    version,
    about = "Check and update package dependencies",
    long_about = "dependency-check-updates scans package manifests for outdated dependencies and optionally updates them.\n\nRun without flags to check for updates. Use -u to apply updates."
)]
pub struct Cli {
    /// Package names to check (acts as filter)
    pub filter: Vec<String>,

    /// Update package file with new versions
    #[arg(short, long)]
    pub upgrade: bool,

    /// Recursively scan subdirectories for manifests
    #[arg(short, long)]
    pub deep: bool,

    /// Target version level
    #[arg(short, long, default_value = "latest", value_parser = parse_target_level)]
    pub target: TargetLevel,

    /// Exclude packages matching pattern
    #[arg(short = 'x', long = "reject")]
    pub reject: Vec<String>,

    /// Path to specific manifest file
    #[arg(long)]
    pub manifest: Option<PathBuf>,

    /// Output format
    #[arg(long, default_value = "table")]
    pub format: OutputFormat,

    /// Exit code behavior: 1 = exit 0 always, 2 = exit 1 if upgrades exist
    #[arg(short, long, default_value = "1")]
    pub error_level: u8,

    /// Increase verbosity (-v, -vv, -vvv)
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

#[derive(Debug, Clone, Copy, Default, clap::ValueEnum)]
pub enum OutputFormat {
    #[default]
    Table,
    Json,
}

/// Parse a `TargetLevel` from a string.
#[cfg(not(tarpaulin_include))]
fn parse_target_level(s: &str) -> Result<TargetLevel, String> {
    s.parse::<TargetLevel>()
}

/// Parse command-line arguments from `std::env::args`.
#[must_use]
#[cfg(not(tarpaulin_include))]
pub fn parse_args() -> Cli {
    Cli::parse()
}

/// Entry point for bridge crates (napi, maturin).
///
/// Parses CLI args from the given slice and runs the full pipeline.
///
/// # Errors
///
/// Returns an error if the CLI command execution fails.
#[cfg(not(tarpaulin_include))]
pub async fn main(args: &[String]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cli = Cli::parse_from(args);
    let error_level = cli.error_level;
    let has_updates = run(&cli).await?;

    if error_level >= 2 && has_updates {
        std::process::exit(1);
    }

    Ok(())
}

/// Run the dependency-check-updates CLI with the given configuration.
///
/// # Errors
///
/// Returns an error if scanning, resolving, or patching fails.
#[allow(clippy::too_many_lines)]
#[cfg(not(tarpaulin_include))]
pub async fn run(cli: &Cli) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    // Install rustls crypto provider (reqwest is built with rustls-no-provider).
    // Idempotent: subsequent calls are no-ops.
    let _ = rustls::crypto::ring::default_provider().install_default();

    init_tracing(cli.verbose);

    let use_color = std::env::var("NO_COLOR").is_err();
    let root = std::env::current_dir()?;

    debug!(root = %root.display(), "working directory");
    debug!(target = %cli.target, upgrade = cli.upgrade, deep = cli.deep, "options");

    if !cli.filter.is_empty() {
        debug!(filter = ?cli.filter, "include filter");
    }
    if !cli.reject.is_empty() {
        debug!(reject = ?cli.reject, "exclude filter");
    }

    // 1. Discover manifests
    let manifests = Scanner::discover(&root, cli.manifest.as_deref(), cli.deep)?;
    info!(count = manifests.len(), "discovered manifests");
    for m in &manifests {
        debug!(path = %m.path.display(), kind = %m.kind, "found manifest");
    }

    // 2. Parse all manifests and collect deps (sync — fast, no I/O wait)
    let mut manifest_jobs: Vec<ManifestJob> = Vec::new();

    for manifest_ref in &manifests {
        let text = std::fs::read_to_string(&manifest_ref.path)?;
        let display_path = manifest_ref
            .path
            .strip_prefix(&root)
            .unwrap_or(&manifest_ref.path)
            .display()
            .to_string();

        info!(path = %display_path, kind = %manifest_ref.kind, "processing manifest");

        let handler: Box<dyn ManifestHandler + Send + Sync> = match manifest_ref.kind {
            ManifestKind::PackageJson => Box::new(NodeHandler),
            ManifestKind::CargoToml => Box::new(RustHandler),
            ManifestKind::PyProjectToml => Box::new(PythonHandler),
        };

        let parsed = handler.parse(&text, &manifest_ref.path)?;
        debug!(
            total_deps = parsed.dependencies.len(),
            "parsed dependencies"
        );
        for dep in &parsed.dependencies {
            trace!(name = %dep.name, version = %dep.current_req, section = %dep.section, "found dependency");
        }

        let deps = filter_deps(&parsed.dependencies, &cli.filter, &cli.reject);
        if deps.len() != parsed.dependencies.len() {
            debug!(
                before = parsed.dependencies.len(),
                after = deps.len(),
                "filtered dependencies"
            );
        }

        manifest_jobs.push(ManifestJob {
            manifest_ref: manifest_ref.clone(),
            display_path,
            text,
            handler,
            deps,
        });
    }

    // 3. Resolve ALL versions concurrently across all manifests (Promise.all pattern)
    //    Create registries once and share across all manifests of the same kind.
    let total_deps: usize = manifest_jobs.iter().map(|j| j.deps.len()).sum();
    info!(
        manifests = manifest_jobs.len(),
        total_deps, "resolving all versions concurrently"
    );

    let npm_registry = NpmRegistry::new();
    let crates_registry = CratesIoRegistry::new();
    let pypi_registry = PyPiRegistry::new();

    let mut resolve_futures = Vec::new();
    for (job_idx, job) in manifest_jobs.iter().enumerate() {
        if !job.deps.is_empty() {
            let npm = &npm_registry;
            let crates_io = &crates_registry;
            let pypi = &pypi_registry;
            resolve_futures.push(async move {
                let resolved = match job.manifest_ref.kind {
                    ManifestKind::PackageJson => npm.resolve_batch(&job.deps, cli.target).await,
                    ManifestKind::CargoToml => crates_io.resolve_batch(&job.deps, cli.target).await,
                    ManifestKind::PyProjectToml => pypi.resolve_batch(&job.deps, cli.target).await,
                };
                (job_idx, resolved)
            });
        }
    }

    let resolved_results: Vec<_> = futures::future::join_all(resolve_futures).await;

    // Build a vec: job_idx -> resolved versions (dense indices, no HashMap needed)
    let mut resolved_map: Vec<Option<ResolvedBatch>> =
        (0..manifest_jobs.len()).map(|_| None).collect();
    for (job_idx, resolved) in resolved_results {
        resolved_map[job_idx] = Some(resolved);
    }

    // 4. Print results and apply updates (sequential — needs ordered output)
    let mut any_updates = false;

    for (job_idx, job) in manifest_jobs.iter().enumerate() {
        print!("{}", output::render_header(&job.display_path, cli.upgrade));

        if job.deps.is_empty() {
            print!(
                "{}",
                output::render_footer(&job.display_path, cli.upgrade, false, use_color)
            );
            continue;
        }

        let resolved = resolved_map[job_idx].as_deref().unwrap_or(&[]);

        let success_count = resolved.iter().filter(|(_, r)| r.is_ok()).count();
        let fail_count = resolved.len() - success_count;
        debug!(
            resolved = success_count,
            failed = fail_count,
            "registry resolution complete"
        );

        let updates = compute_updates(&job.deps, resolved);
        debug!(updates = updates.len(), "computed planned updates");

        for update in &updates {
            debug!(name = %update.name, from = %update.from, to = %update.to, "update available");
        }

        if updates.is_empty() {
            info!(path = %job.display_path, "all dependencies up to date");
            print!(
                "{}",
                output::render_footer(&job.display_path, cli.upgrade, false, use_color)
            );
            continue;
        }

        any_updates = true;

        match cli.format {
            OutputFormat::Table => print!("{}", output::render_table(&updates, use_color)),
            OutputFormat::Json => println!("{}", output::render_json(&updates)),
        }

        if cli.upgrade {
            info!(path = %job.display_path, count = updates.len(), "applying updates");
            let new_text = job.handler.apply_updates(&job.text, &updates)?;
            std::fs::write(&job.manifest_ref.path, new_text)?;
            info!(path = %job.display_path, "manifest updated successfully");
        }

        print!(
            "{}",
            output::render_footer(&job.display_path, cli.upgrade, true, use_color)
        );
    }

    Ok(any_updates)
}

/// Filter dependencies by include/exclude patterns.
fn filter_deps(
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

/// Resolved version batch from a registry.
type ResolvedBatch = Vec<(usize, Result<ResolvedVersion, DcuError>)>;

/// Intermediate state for processing a single manifest.
struct ManifestJob {
    manifest_ref: dependency_check_updates_core::ManifestRef,
    display_path: String,
    text: String,
    handler: Box<dyn ManifestHandler + Send + Sync>,
    deps: Vec<DependencySpec>,
}

/// Compute planned updates from resolved versions.
fn compute_updates(
    deps: &[DependencySpec],
    resolved: &[(usize, Result<ResolvedVersion, DcuError>)],
) -> Vec<PlannedUpdate> {
    let mut updates = Vec::new();

    for (idx, result) in resolved {
        let dep = &deps[*idx];

        let Ok(resolved) = result else {
            warn!(package = %dep.name, "failed to resolve version");
            continue;
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
        // can be parsed as semver, skip this dependency if selected <= current.
        // This guards against the registry-filtering path returning an older
        // stable when the user is already on a higher prerelease (e.g.
        // `sea-orm 2.0.0-rc.37` -> `1.1.20` should NOT be reported).
        //
        // For ranges like `^1` / `0.6` where we cannot fully parse as semver,
        // we fall through to the string-based precision comparison below.
        if let (Ok(cur_ver), Ok(sel_ver)) = (
            semver::Version::parse(current_bare),
            semver::Version::parse(selected),
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

/// Initialize tracing/logging based on verbosity level.
#[cfg(not(tarpaulin_include))]
fn init_tracing(verbose: u8) {
    use tracing_subscriber::{EnvFilter, fmt};

    let level = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("dependency_check_updates={level}")));

    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
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
    fn test_parse_target_level_valid() {
        assert_eq!(parse_target_level("latest").unwrap(), TargetLevel::Latest);
        assert_eq!(parse_target_level("minor").unwrap(), TargetLevel::Minor);
        assert_eq!(parse_target_level("patch").unwrap(), TargetLevel::Patch);
        assert_eq!(parse_target_level("newest").unwrap(), TargetLevel::Newest);
        assert_eq!(
            parse_target_level("greatest").unwrap(),
            TargetLevel::Greatest
        );
    }

    #[test]
    fn test_parse_target_level_invalid() {
        assert!(parse_target_level("invalid").is_err());
    }

    #[test]
    fn test_output_format_default() {
        // Table is the default format
        let fmt = OutputFormat::default();
        assert!(matches!(fmt, OutputFormat::Table));
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
