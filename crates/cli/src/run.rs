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
use crate::compatibility;
use crate::logging::init_tracing;
use crate::output;
use crate::pipeline::{compute_updates, filter_deps};
use crate::project::{self, Document, ProjectHandler};
use crate::report::{ApplyOutcome, Diagnostic, Item, ProjectRow, RunReport, Status};
use crate::tool_registry::ToolRegistry;
use crate::transaction;

/// Resolved version batch from a registry, indexed into the dependency slice
/// the registry was handed.
pub(crate) type ResolvedBatch = Vec<(usize, Result<ResolvedVersion, DcuError>)>;

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
    let report = execute(&cli).await?;
    if report.execution_failed {
        return Err(report_error(&report));
    }
    let code = report.exit_code(&cli);
    if code != 0 {
        std::process::exit(i32::from(code));
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
    match execute(&cli).await {
        Ok(report) => ExitCode::from(report.exit_code(&cli)),
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
#[cfg(not(tarpaulin_include))]
async fn resolve_workflow(
    deps: &[DependencySpec],
    github: Option<&GitHubActionsRegistry>,
    docker: Option<&DockerRegistry>,
    target: TargetLevel,
) -> ResolvedBatch {
    // Fast path: a workflow with no container images — by far the common
    // shape — needs no partitioning and therefore no `DependencySpec` clones.
    if deps
        .iter()
        .all(|dep| dep.section == DependencySection::GitHubActions)
    {
        return resolve_with(github, deps, |r, d| r.resolve_batch(d, target)).await;
    }

    let (action_indices, image_indices) = partition_by_section(deps);
    let actions: Vec<DependencySpec> = action_indices.iter().map(|&i| deps[i].clone()).collect();
    let images: Vec<DependencySpec> = image_indices.iter().map(|&i| deps[i].clone()).collect();

    let (resolved_actions, resolved_images) = futures::join!(
        resolve_with(github, &actions, |r, d| r.resolve_batch(d, target)),
        resolve_with(docker, &images, |r, d| r.resolve_batch(d, target)),
    );

    merge_resolved(
        &action_indices,
        resolved_actions,
        &image_indices,
        resolved_images,
    )
}

/// Split a workflow's dependency indices into (`uses:` refs, container images).
///
/// Returns index lists rather than sub-slices because the two groups are
/// interleaved in the source file, and the caller must map each sub-batch's
/// results back onto the original positions.
fn partition_by_section(deps: &[DependencySpec]) -> (Vec<usize>, Vec<usize>) {
    (0..deps.len()).partition(|&i| deps[i].section == DependencySection::GitHubActions)
}

/// Map two sub-batches back onto the caller's indices and restore document
/// order.
///
/// Each registry reports indices into the slice it was handed, so
/// `resolved_actions[i].0` indexes `action_indices`, not `deps`. Sorting at the
/// end means reported rows follow the file rather than the order the two
/// registries happened to be queried in.
fn merge_resolved(
    action_indices: &[usize],
    resolved_actions: ResolvedBatch,
    image_indices: &[usize],
    resolved_images: ResolvedBatch,
) -> ResolvedBatch {
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
    results.sort_unstable_by_key(|(idx, _)| *idx);
    results
}

/// Resolve a batch against `registry`, yielding an empty batch when there is
/// nothing to ask or nobody to ask.
///
/// The `None` arm is unreachable for a non-empty batch: the gating in [`run`]
/// constructs a registry whenever a dependency of its section exists.
async fn resolve_with<'a, R, F, Fut>(
    registry: Option<&'a R>,
    deps: &'a [DependencySpec],
    call: F,
) -> ResolvedBatch
where
    // The lifetimes are named so the future `call` returns may borrow both
    // arguments; an elided closure signature would force that future to
    // outlive the very references it holds.
    F: FnOnce(&'a R, &'a [DependencySpec]) -> Fut,
    Fut: std::future::Future<Output = ResolvedBatch>,
{
    match registry {
        Some(registry) if !deps.is_empty() => call(registry, deps).await,
        _ => Vec::new(),
    }
}

fn docker_registry(endpoint: Option<&str>) -> DockerRegistry {
    endpoint.map_or_else(DockerRegistry::new, DockerRegistry::with_base_url)
}

async fn resolve_individual_job(
    job: &ManifestJob,
    npm: Option<&NpmRegistry>,
    crates_io: Option<&CratesIoRegistry>,
    pypi: Option<&PyPiRegistry>,
    registry: &ToolRegistry,
    target: TargetLevel,
) -> ResolvedBatch {
    match job.manifest_ref.kind {
        ManifestKind::PackageJson => {
            let ordinary: Vec<_> = job
                .deps
                .iter()
                .enumerate()
                .filter(|(_, d)| d.section != DependencySection::Toolchain)
                .collect();
            let specs: Vec<_> = ordinary.iter().map(|(_, d)| (*d).clone()).collect();
            let mut batch = resolve_with(npm, &specs, |r, deps| r.resolve_batch(deps, target))
                .await
                .into_iter()
                .map(|(i, result)| (ordinary[i].0, result))
                .collect::<Vec<_>>();
            batch.extend(resolve_project(job, registry, target).await);
            batch.sort_by_key(|(i, _)| *i);
            batch
        }
        ManifestKind::CargoToml => {
            resolve_with(crates_io, &job.deps, |r, deps| {
                r.resolve_batch(deps, target)
            })
            .await
        }
        ManifestKind::PyProjectToml => {
            resolve_with(pypi, &job.deps, |r, deps| r.resolve_batch(deps, target)).await
        }
        // These jobs are aggregated by remote_specs, never queried twice.
        ManifestKind::GitHubWorkflow | ManifestKind::Dockerfile | ManifestKind::DockerCompose => {
            Vec::new()
        }
        ManifestKind::Gradle
        | ManifestKind::GradleCatalog
        | ManifestKind::GradleProperties
        | ManifestKind::GradleWrapper
        | ManifestKind::ToolVersions => resolve_project(job, registry, target).await,
    }
}

/// Run the dependency-check-updates CLI with the given configuration.
///
/// # Errors
///
/// Returns an error if scanning, resolving, or patching fails.
#[allow(clippy::too_many_lines)]
#[cfg(not(tarpaulin_include))]
pub async fn run(cli: &Cli) -> Result<bool, DcuError> {
    let report = execute(cli).await?;
    if report.execution_failed {
        return Err(report_error(&report));
    }
    Ok(report.has_updates())
}

fn report_error(report: &RunReport) -> DcuError {
    DcuError::PatchFailed {
        path: PathBuf::from("."),
        detail: report
            .diagnostics
            .iter()
            .map(|d| d.message.as_str())
            .collect::<Vec<_>>()
            .join("; "),
    }
}

#[cfg(not(tarpaulin_include))]
async fn execute(cli: &Cli) -> Result<RunReport, DcuError> {
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
    let tool_registry = ToolRegistry::new();
    let result = if cli.local_tools {
        crate::local_tools::run(cli, &tool_registry).await
    } else {
        execute_at(cli, &root, &tool_registry, use_color).await
    };
    match result {
        Err(e) if cli.format == OutputFormat::JsonReport => {
            let report = RunReport {
                execution_failed: true,
                outcome: if cli.recover.is_some() {
                    ApplyOutcome::RecoveryRequired
                } else if cli.upgrade {
                    ApplyOutcome::Aborted
                } else {
                    ApplyOutcome::NotRequested
                },
                diagnostics: vec![Diagnostic {
                    code: "execution-failed".into(),
                    message: diagnostic_message(&e),
                    path: None,
                }],
                ..RunReport::default()
            };
            report.print_json(cli.format)?;
            Ok(report)
        }
        result => result,
    }
}

fn diagnostic_message(error: &DcuError) -> String {
    match error {
        DcuError::Io { source, .. } => format!("{error}: {source}"),
        DcuError::ManifestParse { detail, .. } | DcuError::PatchFailed { detail, .. } => {
            format!("{error}: {detail}")
        }
        _ => error.to_string(),
    }
}

#[cfg(test)]
pub(crate) async fn run_at(
    cli: &Cli,
    root: &std::path::Path,
    registry: &ToolRegistry,
    color: bool,
) -> Result<bool, DcuError> {
    let report = execute_at(cli, root, registry, color).await?;
    if report.execution_failed {
        return Err(report_error(&report));
    }
    Ok(report.has_updates())
}

pub(crate) async fn execute_at(
    cli: &Cli,
    root: &std::path::Path,
    tool_registry: &ToolRegistry,
    use_color: bool,
) -> Result<RunReport, DcuError> {
    execute_at_with_commit(cli, root, tool_registry, use_color, transaction::commit).await
}

#[allow(clippy::too_many_lines)]
async fn execute_at_with_commit<F>(
    cli: &Cli,
    root: &std::path::Path,
    tool_registry: &ToolRegistry,
    use_color: bool,
    commit: F,
) -> Result<RunReport, DcuError>
where
    F: FnOnce(&std::path::Path, Vec<transaction::Change>) -> Result<(), transaction::Failure>,
{
    if let Some(mode) = cli.recover {
        let changed = transaction::recover(root, mode == crate::cli::RecoveryMode::Finish)
            .map_err(|e| project::error("recovery", e))?;
        let report = RunReport {
            outcome: if !changed {
                ApplyOutcome::NoChanges
            } else if mode == crate::cli::RecoveryMode::Finish {
                ApplyOutcome::Committed
            } else {
                ApplyOutcome::RolledBack
            },
            ..RunReport::default()
        };
        if cli.format.is_json() {
            report.print_json(cli.format)?;
        } else {
            println!("Recovery outcome: {:?}", report.outcome);
        }
        return Ok(report);
    }
    let mut registry = tool_registry.fresh_execution();
    if let Some(path) = &cli.maven_config {
        registry.private_repositories = crate::maven_access::load(&root.join(path))?;
        registry.private_cache =
            Some(dependency_check_updates_core::MetadataCache::without_redirects());
    }
    let tool_registry = &registry;
    let rule_path = cli.compatibility_file.as_ref().map(|p| root.join(p));
    let rules = crate::compatibility_rules::Rules::load(rule_path.as_deref())?;
    if cli.upgrade {
        let receipts = transaction::pending(root).map_err(|source| DcuError::Io {
            path: root.to_owned(),
            source,
        })?;
        if !receipts.is_empty() {
            return pending_report(cli, receipts);
        }
    }
    debug!(target = %cli.target, upgrade = cli.upgrade, deep = cli.deep, "options");

    if !cli.filter.is_empty() {
        debug!(filter = ?cli.filter, "include filter");
    }
    if !cli.reject.is_empty() {
        debug!(reject = ?cli.reject, "exclude filter");
    }

    // 1. Discover manifests
    let mut manifests = Scanner::discover(root, cli.manifest.as_deref(), cli.deep)?;
    let documents =
        project::load_with_discovery(&manifests, root, cli.deep && cli.manifest.is_none())?;
    for document in documents.values() {
        if document.entries.iter().any(|e| e.requested)
            && !manifests.iter().any(|m| m.path == document.path)
        {
            manifests.push(dependency_check_updates_core::ManifestRef {
                path: document.path.clone(),
                kind: ManifestKind::from_path(&document.path).expect("recognized context"),
            });
        }
    }
    manifests.sort_by(|a, b| a.path.cmp(&b.path));
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
            .strip_prefix(root)
            .unwrap_or(&manifest_ref.path)
            .display()
            .to_string();

        info!(path = %display_path, kind = %manifest_ref.kind, "processing manifest");

        let document = documents.get(&manifest_ref.path).cloned();
        let handler: Box<dyn ManifestHandler + Send + Sync> = match manifest_ref.kind {
            ManifestKind::PackageJson => Box::new(NodeHandler),
            ManifestKind::CargoToml => Box::new(RustHandler),
            ManifestKind::PyProjectToml => Box::new(PythonHandler),
            ManifestKind::GitHubWorkflow => Box::new(GitHubHandler),
            ManifestKind::Dockerfile => Box::new(DockerfileHandler),
            ManifestKind::DockerCompose => Box::new(ComposeHandler),
            ManifestKind::Gradle
            | ManifestKind::GradleCatalog
            | ManifestKind::GradleProperties
            | ManifestKind::GradleWrapper
            | ManifestKind::ToolVersions => Box::new(ProjectHandler(
                document.as_ref().expect("project document").clone(),
            )),
        };

        let mut parsed = handler.parse(&text, &manifest_ref.path)?;
        if manifest_ref.kind == ManifestKind::PackageJson
            && let Some(document) = &document
        {
            parsed.dependencies.extend(document.dependencies());
        }
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
            document,
        });
    }

    let target_receipts =
        transaction::pending_for(manifest_jobs.iter().map(|j| j.manifest_ref.path.as_path()))
            .map_err(|e| project::error("recovery marker", e))?;
    if cli.upgrade && !target_receipts.is_empty() {
        return pending_report(cli, target_receipts);
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
    let npm_registry = registry_for(&manifest_jobs, ManifestKind::PackageJson, || {
        NpmRegistry::with_cache(&tool_registry.endpoints.npm, tool_registry.cache.clone())
    });
    let crates_registry = registry_for(&manifest_jobs, ManifestKind::CargoToml, || {
        CratesIoRegistry::with_cache(
            &tool_registry.endpoints.crates_io,
            tool_registry.cache.clone(),
        )
    });
    let pypi_registry = registry_for(&manifest_jobs, ManifestKind::PyProjectToml, || {
        PyPiRegistry::with_cache(&tool_registry.endpoints.pypi, tool_registry.cache.clone())
    });
    // The last two are gated by dependency section, not manifest kind: a
    // workflow can contribute `uses:` refs, container images, or both, and a
    // Dockerfile / Compose file contributes only images.
    let github_registry =
        registry_for_section(&manifest_jobs, DependencySection::GitHubActions, || {
            GitHubActionsRegistry::with_base_url(&tool_registry.endpoints.github)
        });
    let docker_registry =
        registry_for_section(&manifest_jobs, DependencySection::DockerImage, || {
            docker_registry(tool_registry.endpoints.docker.as_deref())
        });

    let mut resolve_futures = Vec::with_capacity(manifest_jobs.len());
    // Keep the existing GitHub/OCI repository/auth-aware batch deduplication,
    // but batch across files rather than caching authenticated HTTP by URL.
    let (remote_indices, remote_specs) = remote_specs(&manifest_jobs);
    for (job_idx, job) in manifest_jobs.iter().enumerate() {
        if !job.deps.is_empty()
            && !matches!(
                job.manifest_ref.kind,
                ManifestKind::GitHubWorkflow
                    | ManifestKind::Dockerfile
                    | ManifestKind::DockerCompose
            )
        {
            let npm = npm_registry.as_ref();
            let crates_io = crates_registry.as_ref();
            let pypi = pypi_registry.as_ref();
            resolve_futures.push(async move {
                let resolved =
                    resolve_individual_job(job, npm, crates_io, pypi, tool_registry, cli.target)
                        .await;
                (job_idx, resolved)
            });
        }
    }

    let (resolved_results, remote_results) = futures::join!(
        futures::future::join_all(resolve_futures),
        resolve_workflow(
            &remote_specs,
            github_registry.as_ref(),
            docker_registry.as_ref(),
            cli.target
        )
    );

    // Build a vec: job_idx -> resolved versions (dense indices, no HashMap needed)
    let mut resolved_map: Vec<Option<ResolvedBatch>> =
        (0..manifest_jobs.len()).map(|_| None).collect();
    for (job_idx, resolved) in resolved_results {
        resolved_map[job_idx] = Some(resolved);
    }
    for (i, result) in remote_results {
        let (job, dep) = remote_indices[i];
        resolved_map[job]
            .get_or_insert_with(Vec::new)
            .push((dep, result));
    }
    for batch in resolved_map.iter_mut().flatten() {
        batch.sort_by_key(|(i, _)| *i);
    }

    // 4. Print results and apply updates (sequential — needs ordered output)
    let mut plans: compatibility::Plans = manifest_jobs
        .iter()
        .enumerate()
        .map(|(i, j)| {
            (
                j.manifest_ref.path.clone(),
                compute_updates(&j.deps, resolved_map[i].as_deref().unwrap_or(&[])),
            )
        })
        .collect();
    let suggestions = crate::compatible::suggest(
        &manifest_jobs,
        &resolved_map,
        &documents,
        tool_registry,
        cli.target,
        &rules,
    )
    .await;
    if cli.compatible {
        for (job_idx, job) in manifest_jobs.iter().enumerate() {
            plans
                .get_mut(&job.manifest_ref.path)
                .expect("job plan")
                .retain(|u| {
                    !job.deps.iter().enumerate().any(|(i, d)| {
                        suggestions.coupled.contains(&(job_idx, i))
                            && u.name == d.name
                            && u.from == d.current_req
                            && u.section == d.section
                    })
                });
        }
        for (path, updates) in &suggestions.updates {
            plans
                .entry(path.clone())
                .or_default()
                .extend(updates.iter().cloned());
        }
    }
    let mut compatibility = std::collections::HashMap::new();
    let mut preparation_errors = std::collections::HashMap::new();
    block_failed_shared_queries(
        &manifest_jobs,
        &resolved_map,
        &documents,
        &mut plans,
        &mut preparation_errors,
    );
    validate_plans(
        &documents,
        &mut plans,
        &mut compatibility,
        &mut preparation_errors,
        cli.strict_compatibility,
        &rules,
    );
    let mut sidecars = std::collections::HashMap::new();
    let mut sidecar_errors = std::collections::HashMap::new();
    for job in &manifest_jobs {
        let updates = plans.get_mut(&job.manifest_ref.path).expect("job plan");
        if let Some(document) = &job.document {
            let mut retained = Vec::new();
            let mut extra = Vec::new();
            let mut errors = Vec::new();
            for update in updates.drain(..) {
                match tool_registry
                    .sidecars(document, std::slice::from_ref(&update))
                    .await
                {
                    Ok(patches) => {
                        extra.extend(patches);
                        retained.push(update);
                    }
                    Err(e) => errors.push((update, e.to_string())),
                }
            }
            *updates = retained;
            sidecars.insert(job.manifest_ref.path.clone(), extra);
            sidecar_errors.insert(job.manifest_ref.path.clone(), errors);
        }
    }
    // Check the actual retained combination after failed checksum/hash lookups.
    validate_plans(
        &documents,
        &mut plans,
        &mut compatibility,
        &mut preparation_errors,
        cli.strict_compatibility,
        &rules,
    );
    let mut prepared = std::collections::HashMap::new();
    for job in &manifest_jobs {
        let updates = &plans[&job.manifest_ref.path];
        if updates.is_empty() {
            continue;
        }
        let text = if let Some(document) = &job.document {
            let extra = sidecars
                .remove(&job.manifest_ref.path)
                .unwrap_or_default()
                .into_iter()
                .filter(|p| {
                    (document
                        .checksum
                        .as_ref()
                        .is_some_and(|span| span.start == p.start && span.end == p.end)
                        && updates.iter().any(|u| u.name == "gradle"))
                        || document.entries.iter().any(|e| {
                            e.integrity
                                .as_ref()
                                .is_some_and(|(span, _)| span.start == p.start && span.end == p.end)
                                && updates
                                    .iter()
                                    .any(|u| u.name == e.dep.name && u.from == e.dep.current_req)
                        })
                })
                .collect();
            let project_updates: Vec<_> = updates
                .iter()
                .filter(|u| project_section(u.section))
                .cloned()
                .collect();
            let text = document.apply(&job.text, &project_updates, extra)?;
            let ordinary: Vec<_> = updates
                .iter()
                .filter(|u| !project_section(u.section))
                .cloned()
                .collect();
            if ordinary.is_empty() {
                text
            } else {
                job.handler.apply_updates(&text, &ordinary)?
            }
        } else {
            job.handler.apply_updates(&job.text, updates)?
        };
        prepared.insert(job.manifest_ref.path.clone(), text);
    }
    let mut report = RunReport {
        manifest_count: manifest_jobs.len(),
        compatibility_rules: if compatibility.is_empty() {
            Vec::new()
        } else {
            rules.provenance.clone()
        },
        ..RunReport::default()
    };
    report.diagnostics.extend(suggestions.diagnostics);
    if !compatibility.is_empty() {
        for provenance in &rules.provenance {
            if provenance.stale || provenance.future {
                report.diagnostics.push(Diagnostic { code: "compatibility-rules-date".into(), message: format!("compatibility rules verified {} are stale (>180 days) or future-dated; compatibility remains unverified", provenance.verified_at), path: None });
            }
        }
    }
    for (i, job) in manifest_jobs.iter().enumerate() {
        let mut rows = report_rows(
            job,
            resolved_map[i].as_deref().unwrap_or(&[]),
            &plans[&job.manifest_ref.path],
            compatibility
                .get(&job.manifest_ref.path)
                .map(String::as_str),
            preparation_errors
                .get(&job.manifest_ref.path)
                .map(String::as_str),
            false,
        );
        if let Some(errors) = sidecar_errors.get(&job.manifest_ref.path) {
            for row in &mut rows {
                if let Some((_, error)) = errors.iter().find(|(u, _)| {
                    u.name == row.name && u.from == row.from && u.section.label() == row.section
                }) {
                    row.status = Status::Blocked;
                    row.reason = Some(format!("integrity preparation failed: {error}"));
                }
            }
        }
        for (dep_idx, row) in rows.iter_mut().enumerate() {
            row.compatible = suggestions.choices.get(&(i, dep_idx)).cloned();
            if cli.compatible && suggestions.coupled.contains(&(i, dep_idx)) {
                row.selection_policy = Some("bounded verified combination: AGP, Kotlin, Gradle, JDK, SDK descending; no downgrades or channel conversion".into());
                if row.compatible.is_none() && row.status == Status::Current {
                    row.status = Status::Unverified;
                    row.reason = Some("no verified combination found within published candidates and documented rules".into());
                }
            }
        }
        report.items.extend(rows.into_iter().map(Item::Project));
    }
    let mut pending_receipts = transaction::pending(root).map_err(|source| DcuError::Io {
        path: root.to_owned(),
        source,
    })?;
    pending_receipts.extend(target_receipts);
    pending_receipts.sort();
    pending_receipts.dedup();
    for path in pending_receipts {
        report.diagnostics.push(Diagnostic {code:"pending-recovery".into(),message:format!("unfinished update receipt {}; inspect backups; read-only queries never recover files automatically",path.display()),path:Some(path.display().to_string())});
    }
    // Validate output representability before any writes, including conflicting
    // versions of a repeated name in legacy's flat update-only object.
    if cli.format == OutputFormat::JsonLegacy {
        report.json(cli.format)?;
    }
    if cli.upgrade {
        if report
            .diagnostics
            .iter()
            .any(|d| d.code == "pending-recovery")
        {
            report.outcome = ApplyOutcome::Aborted;
            report.execution_failed = true;
        } else if cli.fail_on_incomplete && report.incomplete() {
            report.outcome = ApplyOutcome::Aborted;
        } else if prepared.is_empty() {
            report.outcome = ApplyOutcome::NoChanges;
        } else {
            let changes = manifest_jobs
                .iter()
                .filter_map(|job| {
                    prepared
                        .get(&job.manifest_ref.path)
                        .map(|text| transaction::Change {
                            path: job.manifest_ref.path.clone(),
                            original: job.text.as_bytes().to_vec(),
                            replacement: text.as_bytes().to_vec(),
                        })
                })
                .collect();
            match commit(root, changes) {
                Ok(()) => {
                    report.outcome = ApplyOutcome::Committed;
                    for item in &mut report.items {
                        if let Item::Project(r) = item {
                            r.updated = r.to.is_some();
                        }
                    }
                }
                Err(failure) => {
                    report.outcome = if failure.recovery_required {
                        ApplyOutcome::RecoveryRequired
                    } else if failure.committed {
                        ApplyOutcome::Committed
                    } else if failure.rolled_back {
                        ApplyOutcome::RolledBack
                    } else {
                        ApplyOutcome::Aborted
                    };
                    if failure.committed {
                        for item in &mut report.items {
                            if let Item::Project(r) = item {
                                r.updated = r.to.is_some();
                            }
                        }
                    }
                    report.execution_failed = true;
                    report.diagnostics.push(Diagnostic {
                        code: "apply-failed".into(),
                        message: failure.detail,
                        path: None,
                    });
                }
            }
        }
    }
    let mut cleanup_targets = Vec::new();
    let remove_lockfile = cli.remove_lockfile_requested();
    let remove_installed = cli.remove_installed_requested();
    let aborted = report.execution_failed || (cli.fail_on_incomplete && report.incomplete());
    for job in &manifest_jobs {
        if !aborted {
            cleanup_targets.extend(targets_for_job(job, remove_lockfile, remove_installed));
        }
        if cli.format == OutputFormat::Table {
            print!("{}", output::render_header(&job.display_path, cli.upgrade));
        }

        if job.deps.is_empty() {
            if cli.format == OutputFormat::Table {
                println!("No matching dependency declarations.\n");
            }
            continue;
        }

        let updates = &plans[&job.manifest_ref.path];
        let rows: Vec<_> = report
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Project(r) if r.manifest == job.display_path => Some(r),
                _ => None,
            })
            .collect();
        let incomplete = rows.iter().any(|r| r.status.incomplete());
        if cli.format == OutputFormat::Table {
            for row in &rows {
                if !matches!(row.status, Status::Update | Status::Current) {
                    println!(
                        " {} [{:?}] current={} latest={} {}",
                        row.name,
                        row.status,
                        row.from,
                        row.latest.as_deref().unwrap_or("unknown"),
                        row.reason.as_deref().unwrap_or("")
                    );
                }
                if let Some(c) = &row.compatibility {
                    println!("   {}: {c}", row.name);
                }
                if let Some(candidate) = &row.compatible {
                    println!(
                        "   {}: verified combination candidate={candidate} (apply with --compatible -u)",
                        row.name
                    );
                }
            }
        }
        debug!(updates = updates.len(), "computed planned updates");

        for update in updates {
            debug!(name = %update.name, from = %update.from, to = %update.to, "update available");
        }

        if updates.is_empty() {
            if cli.format == OutputFormat::Table && incomplete {
                println!("Some entries could not be checked or safely updated.\n");
            } else if cli.format == OutputFormat::Table {
                print!(
                    "{}",
                    output::render_footer(&job.display_path, cli.upgrade, false, use_color)
                );
            }
            continue;
        }

        if cli.format == OutputFormat::Table {
            print!("{}", output::render_table(updates, use_color));
            if cli.upgrade && !matches!(report.outcome, ApplyOutcome::Committed) {
                println!("Project changes were not committed.\n");
            } else {
                print!(
                    "{}",
                    output::render_footer(&job.display_path, cli.upgrade, true, use_color)
                );
            }
        }
    }

    let cleanup = cleanup_with_progress(cleanup_targets).await;
    if !cleanup.diagnostics.is_empty() {
        report.execution_failed = true;
        report.diagnostics.extend(cleanup.diagnostics);
    }
    if cli.format.is_json() {
        report.print_json(cli.format)?;
        if !cleanup.summary.is_empty() {
            eprint!("{}", cleanup.summary);
        }
    } else {
        print!("{}", cleanup.summary);
        for d in &report.diagnostics {
            eprintln!("{}: {}", d.code, d.message);
        }
    }

    Ok(report)
}

fn pending_report(cli: &Cli, receipts: Vec<std::path::PathBuf>) -> Result<RunReport, DcuError> {
    let report = RunReport { execution_failed: true, outcome: ApplyOutcome::Aborted,
        diagnostics: receipts.into_iter().map(|path| Diagnostic { code: "pending-recovery".into(), message: format!("unfinished update receipt {}; run --recover from its directory before another update", path.display()), path: Some(path.display().to_string()) }).collect(), ..RunReport::default() };
    if cli.format.is_json() {
        report.print_json(cli.format)?;
    } else {
        for d in &report.diagnostics {
            eprintln!("{}: {}", d.code, d.message);
        }
    }
    Ok(report)
}

fn project_section(section: DependencySection) -> bool {
    matches!(
        section,
        DependencySection::Maven
            | DependencySection::GradlePlugin
            | DependencySection::AndroidSdk
            | DependencySection::Toolchain
    )
}

fn remote_specs(jobs: &[ManifestJob]) -> (Vec<(usize, usize)>, Vec<DependencySpec>) {
    let indices: Vec<_> = jobs
        .iter()
        .enumerate()
        .flat_map(|(job_idx, job)| {
            job.deps
                .iter()
                .enumerate()
                .filter(|(_, dep)| {
                    matches!(
                        dep.section,
                        DependencySection::GitHubActions | DependencySection::DockerImage
                    )
                })
                .map(move |(dep_idx, _)| (job_idx, dep_idx))
        })
        .collect();
    let specs = indices
        .iter()
        .map(|&(job, dep)| jobs[job].deps[dep].clone())
        .collect();
    (indices, specs)
}

fn block_failed_shared_queries(
    jobs: &[ManifestJob],
    results: &[Option<ResolvedBatch>],
    documents: &std::collections::HashMap<PathBuf, Document>,
    plans: &mut compatibility::Plans,
    errors: &mut std::collections::HashMap<PathBuf, String>,
) {
    for (job_idx, job) in jobs.iter().enumerate() {
        for (dep_idx, result) in results[job_idx].as_deref().unwrap_or(&[]) {
            let Err(error) = result else {
                continue;
            };
            let Some(entry) = source_entry(job, *dep_idx) else {
                continue;
            };
            let Some(span) = &entry.span else {
                continue;
            };
            let doc = &documents[&job.manifest_ref.path];
            let consumers: Vec<_> = doc
                .entries
                .iter()
                .filter(|e| e.span.as_ref() == Some(span))
                .collect();
            let updates = plans.get_mut(&job.manifest_ref.path).unwrap();
            let before = updates.len();
            updates.retain(|u| {
                !consumers.iter().any(|e| {
                    e.dep.name == u.name
                        && e.dep.current_req == u.from
                        && e.dep.section == u.section
                })
            });
            if updates.len() != before {
                errors.insert(
                    job.manifest_ref.path.clone(),
                    format!("shared-source consumer could not be checked; preserved: {error}"),
                );
            }
        }
    }
}

fn validate_plans(
    documents: &std::collections::HashMap<PathBuf, Document>,
    plans: &mut compatibility::Plans,
    statuses: &mut std::collections::HashMap<PathBuf, String>,
    errors: &mut std::collections::HashMap<PathBuf, String>,
    strict: bool,
    rules: &crate::compatibility_rules::Rules,
) {
    loop {
        let before: usize = plans.values().map(Vec::len).sum();
        for (path, status) in compatibility::guard(documents, plans, strict, rules) {
            if !statuses.get(&path).is_some_and(|s| {
                s.starts_with("conflict:") || s.contains("strict compatibility blocks")
            }) {
                statuses.insert(path, status);
            }
        }
        errors.extend(project::guard_shared_versions(documents, plans));
        if plans.values().map(Vec::len).sum::<usize>() == before {
            break;
        }
    }
}

async fn resolve_project(
    job: &ManifestJob,
    registry: &ToolRegistry,
    target: TargetLevel,
) -> ResolvedBatch {
    let futures = job
        .deps
        .iter()
        .enumerate()
        .filter(|(_, d)| project_section(d.section))
        .map(|(i, dep)| async move {
            let result = match source_entry(job, i) {
                Some(entry) => registry.resolve(entry, target).await,
                None => Err(project::error(&dep.name, "source declaration not found")),
            };
            (i, result)
        });
    futures::future::join_all(futures).await
}

pub(crate) fn source_entry(job: &ManifestJob, index: usize) -> Option<&project::Entry> {
    let dep = &job.deps[index];
    let same = |d: &DependencySpec| {
        d.name == dep.name && d.current_req == dep.current_req && d.section == dep.section
    };
    let ordinal = job.deps[..index].iter().filter(|d| same(d)).count();
    job.document
        .as_ref()?
        .entries
        .iter()
        .filter(|e| e.requested && same(&e.dep))
        .nth(ordinal)
}

pub(crate) fn report_rows(
    job: &ManifestJob,
    resolved: &[(usize, Result<ResolvedVersion, DcuError>)],
    updates: &[dependency_check_updates_core::PlannedUpdate],
    compatibility: Option<&str>,
    preparation_error: Option<&str>,
    upgrade: bool,
) -> Vec<ProjectRow> {
    job.deps
        .iter()
        .enumerate()
        .map(|(i, dep)| {
            let result = resolved.iter().find(|(idx, _)| *idx == i).map(|(_, r)| r);
            let update = updates.iter().find(|u| {
                u.name == dep.name && u.from == dep.current_req && u.section == dep.section
            });
            let mut status = if update.is_some() {
                Status::Update
            } else {
                Status::Current
            };
            let mut reason = None;
            let (latest, selected) = match result {
                Some(Ok(r)) => (r.latest.clone(), r.selected.clone()),
                _ => (None, None),
            };
            if let Some(e) = source_entry(job, i).and_then(|e| e.reason.clone()) {
                status = if e.starts_with("channel preserved") {
                    Status::Channel
                } else {
                    Status::Unsupported
                };
                reason = Some(e);
            }
            if let Some(Err(e)) = result {
                if status != Status::Unsupported {
                    status = Status::Failed;
                }
                reason = Some(e.to_string());
            }
            if result.is_none() {
                status = Status::Failed;
                reason = Some("no registry result".into());
            }
            if result.is_some_and(|r| r.as_ref().is_ok_and(|r| r.selected.is_none()))
                && status == Status::Current
            {
                status = Status::Unverified;
                reason = Some("no candidate for requested target".into());
            }
            let compatibility = compatibility.filter(|_| {
                project_section(dep.section) && crate::compatibility::related(&dep.name)
            });
            if let Some(c) = compatibility {
                if c.starts_with("conflict:") {
                    status = Status::Blocked;
                    reason = Some(c.to_owned());
                } else if c.starts_with("unverified:") && status == Status::Current {
                    status = Status::Unverified;
                    reason = Some(c.to_owned());
                }
            }
            if let Some(e) = preparation_error.filter(|_| update.is_none()) {
                status = Status::Blocked;
                reason = Some(e.to_owned());
            }
            let selection_policy = (matches!(
                dep.section,
                DependencySection::Maven
                    | DependencySection::GradlePlugin
                    | DependencySection::AndroidSdk
            ) || (dep.section == DependencySection::Toolchain
                && dep.name == "jdk"))
                .then(|| {
                    "newest falls back to greatest: metadata has no per-version publication date"
                        .to_owned()
                });
            ProjectRow {
                manifest: job.display_path.clone(),
                name: dep.name.clone(),
                section: dep.section.label().into(),
                from: dep.current_req.clone(),
                to: update.map(|u| u.to.clone()),
                latest,
                selected,
                compatible: None,
                status,
                reason,
                compatibility: compatibility.map(str::to_owned),
                selection_policy,
                updated: upgrade && update.is_some(),
            }
        })
        .collect()
}

/// Intermediate state for processing a single manifest.
pub(crate) struct ManifestJob {
    pub(crate) manifest_ref: dependency_check_updates_core::ManifestRef,
    pub(crate) display_path: String,
    pub(crate) text: String,
    pub(crate) handler: Box<dyn ManifestHandler + Send + Sync>,
    pub(crate) deps: Vec<DependencySpec>,
    pub(crate) document: Option<Document>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use dependency_check_updates_core::ManifestRef;
    use rstest::rstest;
    use std::path::PathBuf;

    #[tokio::test]
    async fn update_failure_reports_match_actual_transaction_outcomes() {
        use crate::tool_registry::Endpoints;
        use clap::Parser;
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};
        let _ = rustls::crypto::ring::default_provider().install_default();
        let server = MockServer::start().await;
        Mock::given(path("/node"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!([{ "version":"v22.0.0", "lts":"Fixed" }])),
            )
            .mount(&server)
            .await;
        let registry = ToolRegistry::with_endpoints(Endpoints {
            node: format!("{}/node", server.uri()),
            ..Endpoints::default()
        });
        for (step, expected, committed) in [
            (transaction::Step::Stage, ApplyOutcome::Aborted, false),
            (transaction::Step::Commit, ApplyOutcome::RolledBack, false),
            (
                transaction::Step::Finalize,
                ApplyOutcome::RecoveryRequired,
                true,
            ),
            (
                transaction::Step::AfterReplace,
                ApplyOutcome::Committed,
                true,
            ),
        ] {
            let dir = tempfile::TempDir::new().unwrap();
            for name in [".nvmrc", ".node-version"] {
                std::fs::write(dir.path().join(name), "20.0.0\n").unwrap();
            }
            let cli = Cli::parse_from(["dcu", "-d", "-u", "--format", "json-report"]);
            let report =
                execute_at_with_commit(&cli, dir.path(), &registry, false, |root, changes| {
                    if step == transaction::Step::AfterReplace {
                        transaction::commit(root, changes)?;
                        return Err(transaction::Failure {
                            detail: "post-commit cleanup failed".into(),
                            committed: true,
                            recovery_required: false,
                            rolled_back: false,
                        });
                    }
                    transaction::commit_with(root, changes, |at, i, _| {
                        if at == step && (step != transaction::Step::Commit || i == 1) {
                            Err("injected failure".into())
                        } else {
                            Ok(())
                        }
                    })
                })
                .await
                .unwrap();
            assert_eq!(report.outcome, expected);
            assert!(report.execution_failed);
            assert_eq!(report.exit_code(&cli), 1);
            assert!(
                report
                    .items
                    .iter()
                    .all(|item| matches!(item, Item::Project(row) if row.updated == committed))
            );
            for name in [".nvmrc", ".node-version"] {
                assert_eq!(
                    std::fs::read_to_string(dir.path().join(name)).unwrap(),
                    if committed { "22.0.0\n" } else { "20.0.0\n" }
                );
            }
        }
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join(".dcu-transaction-blocked.json"), "{}").unwrap();
        let cli = Cli::parse_from(["dcu", "-u"]);
        assert!(
            run_at(&cli, dir.path(), &registry, false)
                .await
                .unwrap_err()
                .to_string()
                .contains("patch")
        );
        for error in [
            DcuError::PatchFailed {
                path: PathBuf::from("file"),
                detail: "specific detail".into(),
            },
            DcuError::ManifestParse {
                path: PathBuf::from("file"),
                detail: "specific detail".into(),
            },
        ] {
            assert!(diagnostic_message(&error).contains("specific detail"));
        }
    }

    #[tokio::test]
    async fn fixed_metadata_updates_all_existing_ecosystems_through_the_pipeline() {
        use crate::tool_registry::Endpoints;
        use clap::Parser;
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};
        let _ = rustls::crypto::ring::default_provider().install_default();
        let server = MockServer::start().await;
        for (url, body) in [
            (
                "/npm/react",
                serde_json::json!({"dist-tags":{"latest":"2.0.0"},"versions":{"1.0.0":{},"2.0.0":{}}}),
            ),
            (
                "/crates/serde/versions",
                serde_json::json!({"versions":[{"num":"1.0.0","yanked":false},{"num":"2.0.0","yanked":false}]}),
            ),
            (
                "/pytest/json",
                serde_json::json!({"info":{"version":"2.0.0"},"releases":{"1.0.0":[{}],"2.0.0":[{}]}}),
            ),
            (
                "/repos/actions/checkout/tags",
                serde_json::json!([{"name":"v4"},{"name":"v5"}]),
            ),
            (
                "/v2/library/node/tags/list",
                serde_json::json!({"name":"library/node","tags":["20","22"]}),
            ),
        ] {
            Mock::given(path(url))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(&server)
                .await;
        }
        let registry = ToolRegistry::with_endpoints(Endpoints {
            npm: format!("{}/npm", server.uri()),
            crates_io: server.uri(),
            pypi: server.uri(),
            github: server.uri(),
            docker: Some(server.uri()),
            ..Endpoints::default()
        });
        let dir = tempfile::TempDir::new().unwrap();
        for (name, text) in [
            ("package.json", r#"{"dependencies":{"react":"^1.0.0"}}"#),
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.1.0'\n[dependencies]\nserde='1.0.0'\n",
            ),
            (
                "pyproject.toml",
                "[project]\nname='fixture'\ndependencies=['pytest>=1.0.0']\n",
            ),
            ("Dockerfile", "FROM node:20\n"),
            ("compose.yml", "services:\n  app:\n    image: node:20\n"),
            (
                ".github/workflows/test.yml",
                "jobs:\n  test:\n    runs-on: ubuntu-latest\n    container:\n      image: node:20\n    steps:\n      - uses: actions/checkout@v4\n",
            ),
        ] {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let cli = Cli::parse_from(["dcu", "-d", "-u", "--format", "json-report"]);
        let report = execute_at(&cli, dir.path(), &registry, false)
            .await
            .unwrap();
        assert_eq!(report.outcome, ApplyOutcome::Committed);
        assert_eq!(report.items.len(), 7);
        assert!(!report.incomplete());
        assert!(
            report
                .items
                .iter()
                .all(|item| matches!(item, Item::Project(row) if row.updated))
        );
        assert!(
            std::fs::read_to_string(dir.path().join("Cargo.toml"))
                .unwrap()
                .contains("2.0.0")
        );
        assert!(
            std::fs::read_to_string(dir.path().join("pyproject.toml"))
                .unwrap()
                .contains("2.0.0")
        );
        let _ = docker_registry(None); // Client construction never sends a request.
        let remote = job(
            ManifestKind::GitHubWorkflow,
            vec![dep("actions/checkout", DependencySection::GitHubActions)],
        );
        assert!(
            resolve_individual_job(&remote, None, None, None, &registry, TargetLevel::Latest)
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn missing_sources_and_missing_candidates_are_incomplete_not_current() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let job = job(
            ManifestKind::Gradle,
            vec![dep("group:artifact", DependencySection::Maven)],
        );
        let rows = report_rows(&job, &[], &[], None, None, false);
        assert_eq!(rows[0].status, Status::Failed);
        let batch = vec![(
            0,
            Ok(ResolvedVersion {
                latest: None,
                selected: None,
            }),
        )];
        assert_eq!(
            report_rows(&job, &batch, &[], None, None, false)[0].status,
            Status::Unverified
        );
        let registry = ToolRegistry::new();
        let result = resolve_project(&job, &registry, TargetLevel::Latest).await;
        assert!(
            result[0]
                .1
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("source declaration")
        );
    }

    fn dep(name: &str, section: DependencySection) -> DependencySpec {
        DependencySpec {
            name: name.to_owned(),
            current_req: "1".to_owned(),
            section,
            path_version: None,
        }
    }

    fn job(kind: ManifestKind, deps: Vec<DependencySpec>) -> ManifestJob {
        ManifestJob {
            manifest_ref: ManifestRef {
                path: PathBuf::from("manifest"),
                kind,
            },
            display_path: "manifest".to_owned(),
            text: String::new(),
            handler: Box::new(NodeHandler),
            deps,
            document: None,
        }
    }

    fn resolved(idx: usize, version: &str) -> (usize, Result<ResolvedVersion, DcuError>) {
        (
            idx,
            Ok(ResolvedVersion {
                latest: Some(version.to_owned()),
                selected: Some(version.to_owned()),
            }),
        )
    }

    #[tokio::test]
    async fn remote_batch_shares_requests_across_manifests_and_preserves_indices() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };
        let _ = rustls::crypto::ring::default_provider().install_default();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/actions/checkout/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"name":"v4"}, {"name":"v5"}
            ])))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v2/library/node/tags/list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name":"library/node", "tags":["20", "22", "20-alpine", "22-alpine"]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let spec = |name: &str, version: &str, section| {
            let mut d = dep(name, section);
            d.current_req = version.to_owned();
            d
        };
        let jobs = vec![
            job(
                ManifestKind::PackageJson,
                vec![dep("react", DependencySection::Dependencies)],
            ),
            job(
                ManifestKind::GitHubWorkflow,
                vec![
                    spec("actions/checkout", "v4", DependencySection::GitHubActions),
                    spec("node", "20-alpine", DependencySection::DockerImage),
                ],
            ),
            job(
                ManifestKind::Dockerfile,
                vec![spec("node", "20", DependencySection::DockerImage)],
            ),
            job(
                ManifestKind::GitHubWorkflow,
                vec![spec(
                    "actions/checkout",
                    "v4",
                    DependencySection::GitHubActions,
                )],
            ),
        ];
        let (indices, specs) = remote_specs(&jobs);
        assert_eq!(indices, vec![(1, 0), (1, 1), (2, 0), (3, 0)]);
        let github = GitHubActionsRegistry::with_base_url(&server.uri());
        let docker = DockerRegistry::with_base_url(&server.uri());
        let results =
            resolve_workflow(&specs, Some(&github), Some(&docker), TargetLevel::Latest).await;
        let mapped: std::collections::HashMap<_, _> = results
            .into_iter()
            .map(|(i, r)| (indices[i], r.unwrap().selected.unwrap()))
            .collect();
        assert_eq!(mapped[&(1, 0)], "5");
        assert_eq!(mapped[&(1, 1)], "22-alpine");
        assert_eq!(mapped[&(2, 0)], "22");
        assert_eq!(mapped[&(3, 0)], "5");
    }

    /// A registry must be built only when a job of that kind has work — an
    /// empty job, or a job of another kind, must not pull an HTTP client into
    /// existence.
    #[rstest]
    #[case::matching_kind_with_deps(
        ManifestKind::PackageJson,
        vec![dep("react", DependencySection::Dependencies)],
        ManifestKind::PackageJson,
        true
    )]
    #[case::matching_kind_but_empty(
        ManifestKind::PackageJson,
        vec![],
        ManifestKind::PackageJson,
        false
    )]
    #[case::other_kind(
        ManifestKind::CargoToml,
        vec![dep("serde", DependencySection::Dependencies)],
        ManifestKind::PackageJson,
        false
    )]
    fn registry_for_cases(
        #[case] job_kind: ManifestKind,
        #[case] deps: Vec<DependencySpec>,
        #[case] wanted: ManifestKind,
        #[case] expected: bool,
    ) {
        let jobs = vec![job(job_kind, deps)];
        assert_eq!(registry_for(&jobs, wanted, || ()).is_some(), expected);
    }

    /// Section gating is what lets one workflow file pull in both registries —
    /// and what keeps a Dockerfile from constructing the GitHub client.
    #[rstest]
    #[case::workflow_actions_only(
        vec![dep("actions/checkout", DependencySection::GitHubActions)],
        true,
        false
    )]
    #[case::workflow_images_only(vec![dep("node", DependencySection::DockerImage)], false, true)]
    #[case::workflow_mixed(
        vec![
            dep("actions/checkout", DependencySection::GitHubActions),
            dep("node", DependencySection::DockerImage),
        ],
        true,
        true
    )]
    #[case::neither(vec![dep("react", DependencySection::Dependencies)], false, false)]
    #[case::no_deps_at_all(vec![], false, false)]
    fn registry_for_section_cases(
        #[case] deps: Vec<DependencySpec>,
        #[case] wants_github: bool,
        #[case] wants_docker: bool,
    ) {
        let jobs = vec![job(ManifestKind::GitHubWorkflow, deps)];
        assert_eq!(
            registry_for_section(&jobs, DependencySection::GitHubActions, || ()).is_some(),
            wants_github
        );
        assert_eq!(
            registry_for_section(&jobs, DependencySection::DockerImage, || ()).is_some(),
            wants_docker
        );
    }

    #[rstest]
    #[case::interleaved(
        &[
            DependencySection::GitHubActions,
            DependencySection::DockerImage,
            DependencySection::GitHubActions,
        ],
        vec![0, 2],
        vec![1]
    )]
    #[case::actions_only(&[DependencySection::GitHubActions], vec![0], vec![])]
    #[case::images_only(&[DependencySection::DockerImage], vec![], vec![0])]
    #[case::empty(&[], vec![], vec![])]
    fn partition_by_section_cases(
        #[case] sections: &[DependencySection],
        #[case] expected_actions: Vec<usize>,
        #[case] expected_images: Vec<usize>,
    ) {
        let deps: Vec<DependencySpec> = sections.iter().map(|s| dep("x", *s)).collect();
        assert_eq!(
            partition_by_section(&deps),
            (expected_actions, expected_images)
        );
    }

    /// The merge is where an off-by-one would silently attach one dependency's
    /// resolved version to another's row, so it is pinned explicitly: each
    /// sub-batch index maps through its own index list, and the output follows
    /// the document.
    #[test]
    fn merge_resolved_remaps_indices_and_restores_document_order() {
        // Document order: [0] action, [1] image, [2] action.
        let action_indices = vec![0, 2];
        let image_indices = vec![1];

        let merged = merge_resolved(
            &action_indices,
            vec![resolved(0, "v5"), resolved(1, "v9")],
            &image_indices,
            vec![resolved(0, "22-alpine")],
        );

        let rows: Vec<(usize, String)> = merged
            .into_iter()
            .map(|(idx, result)| (idx, result.unwrap().selected.unwrap()))
            .collect();
        assert_eq!(
            rows,
            vec![
                (0, "v5".to_owned()),
                (1, "22-alpine".to_owned()),
                (2, "v9".to_owned()),
            ]
        );
    }

    #[test]
    fn merge_resolved_handles_an_empty_side() {
        // A workflow whose container registry produced nothing must still
        // report its action rows unchanged.
        let merged = merge_resolved(
            &[0, 1],
            vec![resolved(0, "v5"), resolved(1, "v9")],
            &[],
            vec![],
        );
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].0, 0);
        assert_eq!(merged[1].0, 1);
    }

    /// With no registry — or nothing to ask it — the batch resolves to empty
    /// without touching the network.
    #[rstest]
    #[case::no_registry(None, vec![dep("node", DependencySection::DockerImage)])]
    #[case::no_deps(Some(()), vec![])]
    #[tokio::test]
    async fn resolve_with_short_circuits(
        #[case] registry: Option<()>,
        #[case] deps: Vec<DependencySpec>,
    ) {
        let batch = resolve_with(registry.as_ref(), &deps, |(), _| async {
            panic!("registry must not be called")
        })
        .await;
        assert!(batch.is_empty());
    }

    #[tokio::test]
    async fn resolve_with_calls_the_registry_when_there_is_work() {
        let deps = vec![dep("node", DependencySection::DockerImage)];
        let batch = resolve_with(Some(&()), &deps, |(), d| {
            let count = d.len();
            async move { (0..count).map(|i| resolved(i, "22")).collect() }
        })
        .await;
        assert_eq!(batch.len(), 1);
    }
}
