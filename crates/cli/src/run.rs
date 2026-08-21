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

/// Resolved version batch from a registry, indexed into the dependency slice
/// the registry was handed.
type ResolvedBatch = Vec<(usize, Result<ResolvedVersion, DcuError>)>;

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

/// Intermediate state for processing a single manifest.
pub(crate) struct ManifestJob {
    pub(crate) manifest_ref: dependency_check_updates_core::ManifestRef,
    pub(crate) display_path: String,
    pub(crate) text: String,
    pub(crate) handler: &'static (dyn ManifestHandler + Send + Sync),
    pub(crate) deps: Vec<DependencySpec>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use dependency_check_updates_core::ManifestRef;
    use rstest::rstest;
    use std::path::PathBuf;

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
            handler: &NODE_HANDLER,
            deps,
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
