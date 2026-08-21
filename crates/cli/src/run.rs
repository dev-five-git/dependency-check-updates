use std::path::PathBuf;

use tracing::{debug, info, trace};

use dependency_check_updates_core::manifest::ManifestHandler;
use dependency_check_updates_core::{
    DcuError, DependencySection, DependencySpec, ManifestKind, ResolvedVersion, Scanner,
    TargetLevel,
};
use dependency_check_updates_docker::{ComposeHandler, DockerRegistry, DockerfileHandler};
use dependency_check_updates_github::{GitHubActionsRegistry, GitHubHandler};
use dependency_check_updates_node::{NodeHandler, NpmRegistry};
use dependency_check_updates_python::{PyPiRegistry, PythonHandler};
use dependency_check_updates_rust::{CratesIoRegistry, RustHandler};

use crate::cleanup_progress::{cleanup_with_progress, targets_for_job};
use crate::cli::{Cli, OutputFormat};
use crate::logging::init_tracing;
use crate::output;
use crate::pipeline::{compute_updates, filter_deps};

// Per-kind handlers are stateless zero-sized unit structs, so a single
// `&'static` reference per kind suffices for the whole process. The previous
// `Box::new(XHandler)` per manifest performed a heap allocation per discovered
// manifest (boxing even ZSTs round-trips through the global allocator under
// the current `Box<dyn Trait>` lowering); the static ref keeps the dispatch
// pointer-sized while removing that allocation.
static NODE_HANDLER: NodeHandler = NodeHandler;
static RUST_HANDLER: RustHandler = RustHandler;
static PYTHON_HANDLER: PythonHandler = PythonHandler;
static GITHUB_HANDLER: GitHubHandler = GitHubHandler;
static DOCKERFILE_HANDLER: DockerfileHandler = DockerfileHandler;
static COMPOSE_HANDLER: ComposeHandler = ComposeHandler;

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

/// Shared entry-point used by the `dependency-check-updates` and `dcu`
/// binaries.
///
/// Parses args, runs the pipeline, and translates the outcome into an
/// `ExitCode` so the binary `main` can return it directly. Centralising
/// this here keeps the error-printing and `--error-level` policy in one
/// place instead of duplicating the `match` arms in every entry point.
#[cfg(not(tarpaulin_include))]
pub async fn run_cli() -> std::process::ExitCode {
    use std::process::ExitCode;

    let cli = crate::cli::parse_args();
    let error_level = cli.error_level;

    match run(&cli).await {
        Ok(has_updates) => {
            // error_level 2: exit 1 if any updates were found (CI mode)
            if error_level >= 2 && has_updates {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eprintln!("Error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Construct a registry only when at least one non-empty job of the matching
/// kind exists. Returns `Some(R)` if any job has non-empty deps and matching
/// kind, otherwise `None`.
fn registry_for<R>(
    jobs: &[ManifestJob],
    kind: ManifestKind,
    make: impl FnOnce() -> R,
) -> Option<R> {
    jobs.iter()
        .any(|job| !job.deps.is_empty() && job.manifest_ref.kind == kind)
        .then(make)
}

/// Construct a registry only when at least one collected dependency belongs to
/// `section`.
///
/// Manifest kind is the wrong gate for the GitHub Actions and container
/// registries: a single workflow file can carry `uses:` directives, `image:`
/// containers, or both, so what decides whether a registry is needed is the
/// section of the dependencies actually found — not the file they came from.
fn registry_for_section<R>(
    jobs: &[ManifestJob],
    section: DependencySection,
    make: impl FnOnce() -> R,
) -> Option<R> {
    jobs.iter()
        .any(|job| job.deps.iter().any(|dep| dep.section == section))
        .then(make)
}

/// Resolve a workflow's dependencies, routing each section to the registry
/// that can answer it.
///
/// A workflow mixes two ecosystems: `uses:` refs resolve against the GitHub
/// Tags API, `image:` containers against an OCI registry. Each sub-batch
/// reports indices into its own slice, so they are mapped back onto the
/// caller's indices and re-sorted into document order before returning.
async fn resolve_workflow(
    deps: &[DependencySpec],
    github: Option<&GitHubActionsRegistry>,
    docker: Option<&DockerRegistry>,
    target: TargetLevel,
) -> ResolvedBatch {
    async fn resolve_actions(
        registry: Option<&GitHubActionsRegistry>,
        deps: &[DependencySpec],
        target: TargetLevel,
    ) -> ResolvedBatch {
        match registry {
            Some(registry) if !deps.is_empty() => registry.resolve_batch(deps, target).await,
            // Unreachable for a non-empty batch: the gating above constructs
            // the registry whenever a dep of this section exists.
            _ => Vec::new(),
        }
    }

    async fn resolve_images(
        registry: Option<&DockerRegistry>,
        deps: &[DependencySpec],
        target: TargetLevel,
    ) -> ResolvedBatch {
        match registry {
            Some(registry) if !deps.is_empty() => registry.resolve_batch(deps, target).await,
            _ => Vec::new(),
        }
    }

    // Fast path: a workflow with no container images — by far the common
    // shape — needs no partitioning and therefore no `DependencySpec` clones.
    if deps
        .iter()
        .all(|dep| dep.section == DependencySection::GitHubActions)
    {
        return resolve_actions(github, deps, target).await;
    }

    let (action_indices, image_indices): (Vec<usize>, Vec<usize>) =
        (0..deps.len()).partition(|&i| deps[i].section == DependencySection::GitHubActions);
    let actions: Vec<DependencySpec> = action_indices.iter().map(|&i| deps[i].clone()).collect();
    let images: Vec<DependencySpec> = image_indices.iter().map(|&i| deps[i].clone()).collect();

    let (resolved_actions, resolved_images) = futures::join!(
        resolve_actions(github, &actions, target),
        resolve_images(docker, &images, target),
    );

    let mut results = Vec::with_capacity(resolved_actions.len() + resolved_images.len());
    results.extend(
        resolved_actions
            .into_iter()
            .map(|(i, result)| (action_indices[i], result)),
    );
    results.extend(
        resolved_images
            .into_iter()
            .map(|(i, result)| (image_indices[i], result)),
    );
    // Restore document order so the reported rows follow the file, not the
    // order the two registries happened to be queried in.
    results.sort_unstable_by_key(|(idx, _)| *idx);
    results
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

    let use_color = output::color_enabled(std::env::var_os("NO_COLOR"));
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
    let mut manifest_jobs: Vec<ManifestJob> = Vec::with_capacity(manifests.len());

    for manifest_ref in manifests {
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

        let handler: &'static (dyn ManifestHandler + Send + Sync) = match manifest_ref.kind {
            ManifestKind::PackageJson => &NODE_HANDLER,
            ManifestKind::CargoToml => &RUST_HANDLER,
            ManifestKind::PyProjectToml => &PYTHON_HANDLER,
            ManifestKind::GitHubWorkflow => &GITHUB_HANDLER,
            ManifestKind::Dockerfile => &DOCKERFILE_HANDLER,
            ManifestKind::DockerCompose => &COMPOSE_HANDLER,
        };

        let parsed = handler.parse(&text, &manifest_ref.path)?;
        let total_deps = parsed.dependencies.len();
        debug!(total_deps, "parsed dependencies");
        for dep in &parsed.dependencies {
            trace!(name = %dep.name, version = %dep.current_req, section = %dep.section, "found dependency");
        }

        let deps = filter_deps(parsed.dependencies, &cli.filter, &cli.reject);
        if deps.len() != total_deps {
            debug!(
                before = total_deps,
                after = deps.len(),
                "filtered dependencies"
            );
        }

        manifest_jobs.push(ManifestJob {
            manifest_ref,
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

    // Construct each registry only when at least one non-empty job of the
    // matching kind exists, so a scan touching only (say) Cargo.toml never
    // builds the npm/PyPI/GitHub HTTP clients. Each registry is still created
    // at most once and shared by reference across every manifest of its kind.
    let npm_registry = registry_for(&manifest_jobs, ManifestKind::PackageJson, NpmRegistry::new);
    let crates_registry = registry_for(
        &manifest_jobs,
        ManifestKind::CargoToml,
        CratesIoRegistry::new,
    );
    let pypi_registry = registry_for(
        &manifest_jobs,
        ManifestKind::PyProjectToml,
        PyPiRegistry::new,
    );
    // The last two are gated by dependency section, not manifest kind: a
    // workflow can contribute `uses:` refs, container images, or both, and a
    // Dockerfile / Compose file contributes only images.
    let github_registry = registry_for_section(
        &manifest_jobs,
        DependencySection::GitHubActions,
        GitHubActionsRegistry::new,
    );
    let docker_registry = registry_for_section(
        &manifest_jobs,
        DependencySection::DockerImage,
        DockerRegistry::new,
    );

    let mut resolve_futures = Vec::with_capacity(manifest_jobs.len());
    for (job_idx, job) in manifest_jobs.iter().enumerate() {
        if !job.deps.is_empty() {
            let npm = npm_registry.as_ref();
            let crates_io = crates_registry.as_ref();
            let pypi = pypi_registry.as_ref();
            let github = github_registry.as_ref();
            let docker = docker_registry.as_ref();
            resolve_futures.push(async move {
                // The gating above guarantees the registry matching this job's
                // kind is `Some`; the `None` arms are unreachable for a
                // non-empty job and return an empty batch without panicking.
                let resolved = match job.manifest_ref.kind {
                    ManifestKind::PackageJson => match npm {
                        Some(npm) => npm.resolve_batch(&job.deps, cli.target).await,
                        None => Vec::new(),
                    },
                    ManifestKind::CargoToml => match crates_io {
                        Some(crates_io) => crates_io.resolve_batch(&job.deps, cli.target).await,
                        None => Vec::new(),
                    },
                    ManifestKind::PyProjectToml => match pypi {
                        Some(pypi) => pypi.resolve_batch(&job.deps, cli.target).await,
                        None => Vec::new(),
                    },
                    // A workflow can hold both ecosystems, so it fans out to
                    // both registries and merges the results.
                    ManifestKind::GitHubWorkflow => {
                        resolve_workflow(&job.deps, github, docker, cli.target).await
                    }
                    ManifestKind::Dockerfile | ManifestKind::DockerCompose => match docker {
                        Some(docker) => docker.resolve_batch(&job.deps, cli.target).await,
                        None => Vec::new(),
                    },
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
    let mut cleanup_targets = Vec::new();
    let remove_lockfile = cli.remove_lockfile_requested();
    let remove_installed = cli.remove_installed_requested();

    for (job_idx, job) in manifest_jobs.iter().enumerate() {
        cleanup_targets.extend(targets_for_job(job, remove_lockfile, remove_installed));
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
            std::fs::write(&job.manifest_ref.path, new_text).map_err(|e| DcuError::Io {
                path: job.manifest_ref.path.clone(),
                source: e,
            })?;
            info!(path = %job.display_path, "manifest updated successfully");
        }

        print!(
            "{}",
            output::render_footer(&job.display_path, cli.upgrade, true, use_color)
        );
    }

    print!("{}", cleanup_with_progress(cleanup_targets).await);

    Ok(any_updates)
}

/// Resolved version batch from a registry.
type ResolvedBatch = Vec<(usize, Result<ResolvedVersion, DcuError>)>;

/// Intermediate state for processing a single manifest.
pub(crate) struct ManifestJob {
    pub(crate) manifest_ref: dependency_check_updates_core::ManifestRef,
    pub(crate) display_path: String,
    pub(crate) text: String,
    pub(crate) handler: &'static (dyn ManifestHandler + Send + Sync),
    pub(crate) deps: Vec<DependencySpec>,
}
