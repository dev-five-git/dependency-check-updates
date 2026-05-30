use std::path::PathBuf;

use tracing::{debug, info, trace};

use dependency_check_updates_core::manifest::ManifestHandler;
use dependency_check_updates_core::{
    DcuError, DependencySpec, ManifestKind, ResolvedVersion, Scanner,
};
use dependency_check_updates_github::{GitHubActionsRegistry, GitHubHandler};
use dependency_check_updates_node::{NodeHandler, NpmRegistry};
use dependency_check_updates_python::{PyPiRegistry, PythonHandler};
use dependency_check_updates_rust::{CratesIoRegistry, RustHandler};

use crate::cleanup::cleanup_and_render;
use crate::cli::{Cli, OutputFormat};
use crate::logging::init_tracing;
use crate::output;
use crate::pipeline::{compute_updates, filter_deps};

/// Entry point for bridge crates (napi, maturin).
///
/// Parses CLI args from the given slice and runs the full pipeline.
///
/// # Errors
///
/// Returns an error if the CLI command execution fails.
#[cfg(not(tarpaulin_include))]
pub async fn main(args: &[String]) -> Result<(), DcuError> {
    use clap::Parser;
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
pub async fn run(cli: &Cli) -> Result<bool, DcuError> {
    // Install rustls crypto provider (reqwest is built with rustls-no-provider).
    // Idempotent: subsequent calls are no-ops.
    let _ = rustls::crypto::ring::default_provider().install_default();

    init_tracing(cli.verbose);

    let use_color = std::env::var("NO_COLOR").is_err();
    let root = std::env::current_dir().map_err(|e| DcuError::Io {
        path: PathBuf::from("."),
        source: e,
    })?;

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
        let text = std::fs::read_to_string(&manifest_ref.path).map_err(|e| DcuError::Io {
            path: manifest_ref.path.clone(),
            source: e,
        })?;
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

        let updates = compute_updates(&job.deps, resolved, job.manifest_ref.kind);
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
            std::fs::write(&job.manifest_ref.path, new_text).map_err(|e| DcuError::Io {
                path: job.manifest_ref.path.clone(),
                source: e,
            })?;
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

/// Resolved version batch from a registry.
type ResolvedBatch = Vec<(usize, Result<ResolvedVersion, DcuError>)>;

/// Intermediate state for processing a single manifest.
pub(crate) struct ManifestJob {
    pub(crate) manifest_ref: dependency_check_updates_core::ManifestRef,
    pub(crate) display_path: String,
    pub(crate) text: String,
    pub(crate) handler: Box<dyn ManifestHandler + Send + Sync>,
    pub(crate) deps: Vec<DependencySpec>,
}
