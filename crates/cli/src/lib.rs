//! dependency-check-updates CLI — check and update package dependencies.

mod output;

use std::path::{Path, PathBuf};

use clap::Parser;
use tracing::{debug, info, trace, warn};

use dependency_check_updates_core::manifest::ManifestHandler;
use dependency_check_updates_core::{
    DcuError, DependencySpec, ManifestKind, PlannedUpdate, ResolvedVersion, Scanner, TargetLevel,
};
use dependency_check_updates_github::{GitHubActionsRegistry, GitHubHandler};
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
// A clap-derived CLI struct naturally accumulates one bool per boolean flag.
// `upgrade`, `deep`, `remove_lockfile`, `remove_installed` are all independent
// command-line switches with no shared state — collapsing them into an enum
// would obscure the 1:1 mapping to `--flag` arguments without buying anything.
#[allow(clippy::struct_excessive_bools)]
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

    /// Remove lockfiles next to each discovered manifest so the package
    /// manager regenerates them on the next install — bumping transitive
    /// dependencies too.
    ///
    /// Files removed (siblings of each manifest):
    /// - `package.json`   → `bun.lock`, `bun.lockb`, `package-lock.json`, `pnpm-lock.yaml`, `yarn.lock`
    /// - `Cargo.toml`     → `Cargo.lock`
    /// - `pyproject.toml` → `uv.lock`, `poetry.lock`, `Pipfile.lock`
    #[arg(long = "remove-lockfile")]
    pub remove_lockfile: bool,

    /// Remove installed dependency directories next to each discovered
    /// manifest. Useful together with `--remove-lockfile` to force a clean
    /// install when the previously installed copies pin transitive
    /// versions even after the lockfile is gone.
    ///
    /// Directories removed (siblings of each manifest):
    /// - `package.json`   → `node_modules/`
    /// - `Cargo.toml`     → `target/`
    /// - `pyproject.toml` → `.venv/`, `venv/`
    #[arg(long = "remove-installed")]
    pub remove_installed: bool,

    /// Shortcut for `--remove-lockfile --remove-installed`. Wipes both
    /// lockfiles and installed-dep directories next to every discovered
    /// manifest — the one-liner for "give me a fully fresh dep tree on the
    /// next install".
    ///
    /// OR'd with the granular flags: combining `--rm` with
    /// `--remove-lockfile` is harmless and behaves the same as `--rm`.
    #[arg(long = "rm")]
    pub rm: bool,

    /// Exit code behavior: 1 = exit 0 always, 2 = exit 1 if upgrades exist
    #[arg(short, long, default_value = "1")]
    pub error_level: u8,

    /// Increase verbosity (-v, -vv, -vvv)
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

impl Cli {
    /// Effective `--remove-lockfile` value: true when either the granular
    /// flag is set or the `--rm` shortcut is set.
    ///
    /// Encoded once here so every consumer (cleanup loop, tests, future
    /// callers) reads the same OR semantics instead of re-implementing it.
    #[must_use]
    pub fn remove_lockfile_requested(&self) -> bool {
        self.remove_lockfile || self.rm
    }

    /// Effective `--remove-installed` value: true when either the granular
    /// flag is set or the `--rm` shortcut is set.
    #[must_use]
    pub fn remove_installed_requested(&self) -> bool {
        self.remove_installed || self.rm
    }
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
            ManifestKind::GitHubWorkflow => Box::new(GitHubHandler),
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
    let github_registry = GitHubActionsRegistry::new();

    let mut resolve_futures = Vec::new();
    for (job_idx, job) in manifest_jobs.iter().enumerate() {
        if !job.deps.is_empty() {
            let npm = &npm_registry;
            let crates_io = &crates_registry;
            let pypi = &pypi_registry;
            let github = &github_registry;
            resolve_futures.push(async move {
                let resolved = match job.manifest_ref.kind {
                    ManifestKind::PackageJson => npm.resolve_batch(&job.deps, cli.target).await,
                    ManifestKind::CargoToml => crates_io.resolve_batch(&job.deps, cli.target).await,
                    ManifestKind::PyProjectToml => pypi.resolve_batch(&job.deps, cli.target).await,
                    ManifestKind::GitHubWorkflow => {
                        github.resolve_batch(&job.deps, cli.target).await
                    }
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
            print!("{}", cleanup_and_render(job, cli));
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
            print!("{}", cleanup_and_render(job, cli));
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

        print!("{}", cleanup_and_render(job, cli));
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

/// Lockfiles that sit next to a manifest of the given kind.
///
/// These are the files `--remove-lockfile` clears. The intent is to force the
/// downstream package manager to re-resolve every transitive dependency on
/// the next install, so that `dcu -u --remove-lockfile` is a true "update
/// everything, including dep-of-dep" operation rather than just the
/// top-level entries written into the manifest.
#[must_use]
fn lockfiles_for(kind: ManifestKind) -> &'static [&'static str] {
    match kind {
        ManifestKind::PackageJson => &[
            "bun.lock",
            "bun.lockb",
            "package-lock.json",
            "pnpm-lock.yaml",
            "yarn.lock",
        ],
        ManifestKind::CargoToml => &["Cargo.lock"],
        ManifestKind::PyProjectToml => &["uv.lock", "poetry.lock", "Pipfile.lock"],
        // Workflow files have no companion lockfile.
        ManifestKind::GitHubWorkflow => &[],
    }
}

/// Installed-dependency directories that sit next to a manifest of the given
/// kind.
///
/// `--remove-installed` wipes these so the package manager performs a clean
/// install. Without this step, an already-installed copy of a transitive
/// dependency can pin the resolver back to its old version even after the
/// lockfile is gone (bun/pnpm/uv all exhibit this).
#[must_use]
fn installed_dirs_for(kind: ManifestKind) -> &'static [&'static str] {
    match kind {
        ManifestKind::PackageJson => &["node_modules"],
        ManifestKind::CargoToml => &["target"],
        ManifestKind::PyProjectToml => &[".venv", "venv"],
        ManifestKind::GitHubWorkflow => &[],
    }
}

/// Delete sibling lockfiles and/or installed-dep directories next to a
/// manifest. Missing entries are silently skipped — the goal is idempotency,
/// not strictness.
///
/// Returns the display names (lockfiles as-is, directories with a trailing
/// `/`) of every entry actually removed, in the order they were processed.
/// The caller uses this list to print a per-manifest summary.
fn cleanup_manifest_siblings(
    manifest_path: &Path,
    kind: ManifestKind,
    remove_lockfile: bool,
    remove_installed: bool,
) -> Vec<String> {
    let mut removed = Vec::new();

    if !remove_lockfile && !remove_installed {
        return removed;
    }

    let Some(dir) = manifest_path.parent() else {
        return removed;
    };

    if remove_lockfile {
        for name in lockfiles_for(kind) {
            let path = dir.join(name);
            match std::fs::remove_file(&path) {
                Ok(()) => removed.push((*name).to_owned()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "failed to remove lockfile");
                }
            }
        }
    }

    if remove_installed {
        for name in installed_dirs_for(kind) {
            let path = dir.join(name);
            match std::fs::remove_dir_all(&path) {
                Ok(()) => removed.push(format!("{name}/")),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "failed to remove installed directory");
                }
            }
        }
    }

    removed
}

/// Render the " Removed <name>\n" lines for a list of deleted siblings.
/// Returns an empty string when nothing was removed so the caller can print
/// it unconditionally without producing a stray blank line.
#[must_use]
fn render_removed(removed: &[String]) -> String {
    let mut out = String::new();
    for name in removed {
        let _ = std::fmt::Write::write_fmt(&mut out, format_args!(" Removed {name}\n"));
    }
    out
}

/// Convenience wrapper: run the sibling cleanup for a job and render the
/// resulting summary string in one call.
///
/// Reads the effective removal flags via [`Cli::remove_lockfile_requested`]
/// and [`Cli::remove_installed_requested`] so the `--rm` shortcut and the
/// granular flags share one OR-semantics implementation.
#[cfg(not(tarpaulin_include))]
fn cleanup_and_render(job: &ManifestJob, cli: &Cli) -> String {
    let removed = cleanup_manifest_siblings(
        &job.manifest_ref.path,
        job.manifest_ref.kind,
        cli.remove_lockfile_requested(),
        cli.remove_installed_requested(),
    );
    render_removed(&removed)
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

        if precision < 3 && !is_plain_three_segment_version(selected) {
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

fn is_plain_three_segment_version(version: &str) -> bool {
    let mut parts = version.split('.');
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some(major), Some(minor), Some(patch), None)
            if !major.is_empty()
                && !minor.is_empty()
                && !patch.is_empty()
                && major.chars().all(|c| c.is_ascii_digit())
                && minor.chars().all(|c| c.is_ascii_digit())
                && patch.chars().all(|c| c.is_ascii_digit())
    )
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

    // -------- Cleanup helpers --------

    #[test]
    fn test_lockfiles_for_each_kind() {
        // PackageJson covers every major Node lockfile variant.
        let node = lockfiles_for(ManifestKind::PackageJson);
        assert!(node.contains(&"bun.lock"));
        assert!(node.contains(&"bun.lockb"));
        assert!(node.contains(&"package-lock.json"));
        assert!(node.contains(&"pnpm-lock.yaml"));
        assert!(node.contains(&"yarn.lock"));

        assert_eq!(lockfiles_for(ManifestKind::CargoToml), &["Cargo.lock"]);

        let py = lockfiles_for(ManifestKind::PyProjectToml);
        assert!(py.contains(&"uv.lock"));
        assert!(py.contains(&"poetry.lock"));
        assert!(py.contains(&"Pipfile.lock"));

        assert!(lockfiles_for(ManifestKind::GitHubWorkflow).is_empty());
    }

    #[test]
    fn test_installed_dirs_for_each_kind() {
        assert_eq!(
            installed_dirs_for(ManifestKind::PackageJson),
            &["node_modules"]
        );
        assert_eq!(installed_dirs_for(ManifestKind::CargoToml), &["target"]);

        let py = installed_dirs_for(ManifestKind::PyProjectToml);
        assert!(py.contains(&".venv"));
        assert!(py.contains(&"venv"));

        assert!(installed_dirs_for(ManifestKind::GitHubWorkflow).is_empty());
    }

    #[test]
    fn test_render_removed_empty_returns_empty_string() {
        // No removals → no output (caller prints unconditionally).
        assert_eq!(render_removed(&[]), "");
    }

    #[test]
    fn test_render_removed_formats_each_entry_on_its_own_line() {
        let lines = render_removed(&["Cargo.lock".to_owned(), "target/".to_owned()]);
        assert_eq!(lines, " Removed Cargo.lock\n Removed target/\n");
    }

    #[test]
    fn test_cleanup_manifest_siblings_both_flags_off_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        std::fs::write(&manifest, "").unwrap();
        let lock = dir.path().join("Cargo.lock");
        std::fs::write(&lock, "").unwrap();

        let removed =
            cleanup_manifest_siblings(&manifest, ManifestKind::CargoToml, false, false);

        assert!(removed.is_empty());
        assert!(lock.exists(), "Cargo.lock must be untouched when both flags are off");
    }

    #[test]
    fn test_cleanup_manifest_siblings_removes_existing_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        std::fs::write(&manifest, "").unwrap();
        let lock = dir.path().join("Cargo.lock");
        std::fs::write(&lock, "# whatever").unwrap();

        let removed = cleanup_manifest_siblings(&manifest, ManifestKind::CargoToml, true, false);

        assert_eq!(removed, vec!["Cargo.lock".to_owned()]);
        assert!(!lock.exists());
    }

    #[test]
    fn test_cleanup_manifest_siblings_silently_skips_missing_lockfile() {
        // No Cargo.lock present — must succeed and report nothing removed.
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        std::fs::write(&manifest, "").unwrap();

        let removed = cleanup_manifest_siblings(&manifest, ManifestKind::CargoToml, true, true);

        assert!(removed.is_empty());
    }

    #[test]
    fn test_cleanup_manifest_siblings_removes_node_modules() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("package.json");
        std::fs::write(&manifest, "{}").unwrap();
        let nm = dir.path().join("node_modules");
        std::fs::create_dir_all(nm.join("react")).unwrap();
        std::fs::write(nm.join("react").join("index.js"), "").unwrap();

        let removed =
            cleanup_manifest_siblings(&manifest, ManifestKind::PackageJson, false, true);

        assert_eq!(removed, vec!["node_modules/".to_owned()]);
        assert!(!nm.exists(), "node_modules must be removed recursively");
    }

    #[test]
    fn test_cleanup_manifest_siblings_removes_lockfile_and_installed_together() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("package.json");
        std::fs::write(&manifest, "{}").unwrap();
        std::fs::write(dir.path().join("bun.lock"), "").unwrap();
        std::fs::write(dir.path().join("yarn.lock"), "").unwrap();
        std::fs::create_dir_all(dir.path().join("node_modules")).unwrap();

        let removed =
            cleanup_manifest_siblings(&manifest, ManifestKind::PackageJson, true, true);

        // Lockfiles come first (in their declared order), then installed dirs.
        assert!(removed.contains(&"bun.lock".to_owned()));
        assert!(removed.contains(&"yarn.lock".to_owned()));
        assert!(removed.contains(&"node_modules/".to_owned()));
        // Lockfiles must appear before installed dirs.
        let lock_idx = removed.iter().position(|n| n == "bun.lock").unwrap();
        let nm_idx = removed.iter().position(|n| n == "node_modules/").unwrap();
        assert!(lock_idx < nm_idx, "lockfiles should be listed before installed dirs");

        assert!(!dir.path().join("bun.lock").exists());
        assert!(!dir.path().join("yarn.lock").exists());
        assert!(!dir.path().join("node_modules").exists());
    }

    #[test]
    fn test_cleanup_manifest_siblings_does_not_touch_unrelated_lockfiles() {
        // A Cargo manifest must NEVER delete bun.lock or pnpm-lock.yaml, even
        // when both --remove-lockfile and --remove-installed are set.
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        std::fs::write(&manifest, "").unwrap();
        std::fs::write(dir.path().join("Cargo.lock"), "").unwrap();
        let foreign_lock = dir.path().join("bun.lock");
        std::fs::write(&foreign_lock, "").unwrap();
        let foreign_dir = dir.path().join("node_modules");
        std::fs::create_dir_all(&foreign_dir).unwrap();

        let removed = cleanup_manifest_siblings(&manifest, ManifestKind::CargoToml, true, true);

        assert!(removed.contains(&"Cargo.lock".to_owned()));
        assert!(
            !removed.iter().any(|n| n == "bun.lock"),
            "Cargo cleanup must not touch bun.lock"
        );
        assert!(foreign_lock.exists(), "bun.lock must survive Cargo cleanup");
        assert!(foreign_dir.exists(), "node_modules must survive Cargo cleanup");
    }

    // -------- --rm shortcut + effective-flag accessors --------

    #[test]
    fn test_cli_no_removal_flags_default_to_false() {
        let cli = Cli::parse_from(["dcu"]);
        assert!(!cli.rm);
        assert!(!cli.remove_lockfile);
        assert!(!cli.remove_installed);
        assert!(!cli.remove_lockfile_requested());
        assert!(!cli.remove_installed_requested());
    }

    #[test]
    fn test_cli_rm_alone_implies_both_removals() {
        let cli = Cli::parse_from(["dcu", "--rm"]);
        assert!(cli.rm);
        // Granular flags remain false — `--rm` does not back-fill them.
        assert!(!cli.remove_lockfile);
        assert!(!cli.remove_installed);
        // …but the effective accessors must report true.
        assert!(cli.remove_lockfile_requested());
        assert!(cli.remove_installed_requested());
    }

    #[test]
    fn test_cli_granular_lockfile_alone_does_not_imply_installed() {
        let cli = Cli::parse_from(["dcu", "--remove-lockfile"]);
        assert!(!cli.rm);
        assert!(cli.remove_lockfile_requested());
        assert!(
            !cli.remove_installed_requested(),
            "granular --remove-lockfile must NOT enable installed-dir removal"
        );
    }

    #[test]
    fn test_cli_granular_installed_alone_does_not_imply_lockfile() {
        let cli = Cli::parse_from(["dcu", "--remove-installed"]);
        assert!(!cli.rm);
        assert!(cli.remove_installed_requested());
        assert!(
            !cli.remove_lockfile_requested(),
            "granular --remove-installed must NOT enable lockfile removal"
        );
    }

    #[test]
    fn test_cli_rm_combined_with_granular_is_idempotent() {
        // --rm alongside --remove-lockfile is harmless; both effective flags
        // stay true, matching --rm-only semantics.
        let cli = Cli::parse_from(["dcu", "--rm", "--remove-lockfile"]);
        assert!(cli.rm);
        assert!(cli.remove_lockfile);
        assert!(cli.remove_lockfile_requested());
        assert!(cli.remove_installed_requested());
    }

    #[test]
    fn test_cleanup_manifest_siblings_github_workflow_is_noop() {
        // Workflows have no companion lockfile or installed dir — both flags
        // must produce zero removals and zero side effects.
        let dir = tempfile::tempdir().unwrap();
        let workflows = dir.path().join(".github").join("workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        let manifest = workflows.join("CI.yml");
        std::fs::write(&manifest, "name: CI\n").unwrap();
        // A stray bun.lock next to the workflow must not be touched.
        let stray = workflows.join("bun.lock");
        std::fs::write(&stray, "").unwrap();

        let removed =
            cleanup_manifest_siblings(&manifest, ManifestKind::GitHubWorkflow, true, true);

        assert!(removed.is_empty());
        assert!(stray.exists());
    }
}
