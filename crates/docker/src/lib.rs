//! Container image support for dependency-check-updates.
//!
//! Tracks the images a project builds on — Dockerfile `FROM` instructions and
//! the `image:` key of Compose services — and resolves newer tags from any
//! OCI Distribution registry (Docker Hub, `ghcr.io`, `quay.io`,
//! `mcr.microsoft.com`, a self-hosted registry, …).
//!
//! Two properties shape everything here:
//!
//! - **A tag is a version plus a variant.** `node:20-alpine` must become
//!   `node:22-alpine`, never `node:22`. See [`tag`] for how variant groups
//!   keep those lanes separate.
//! - **Moving and immutable pins are left alone.** `:latest`, codenames like
//!   `:bookworm`, `@sha256:` digests, `${VAR}` interpolations, and untagged
//!   references (including multi-stage `FROM builder`) are all skipped on
//!   purpose — each one means the user opted out of tag tracking.
//!
//! A GitHub Actions workflow spells its job containers and service containers
//! with the very same `image:` key a Compose file uses, so
//! [`yaml_image_dependencies`] and [`apply_yaml_image_updates`] expose that
//! scanner to the GitHub crate instead of it growing a second copy.

#![warn(missing_docs)]

mod image;
mod parser;
mod patcher;
mod registry;
mod tag;
mod yaml;

use std::path::Path;

use dependency_check_updates_core::manifest::{ManifestHandler, ParsedManifest};
use dependency_check_updates_core::{
    DcuError, DependencySection, DependencySpec, ManifestKind, ManifestRef, PlannedUpdate,
};

use image::ImageLocation;
pub use registry::DockerRegistry;

/// Turn scanned image locations into dependency specs.
fn to_dependencies(locations: Vec<ImageLocation>) -> Vec<DependencySpec> {
    locations
        .into_iter()
        .map(|location| DependencySpec {
            name: location.name,
            current_req: location.tag,
            section: DependencySection::DockerImage,
            path_version: None,
        })
        .collect()
}

/// Collect the tracked image dependencies of a YAML document.
///
/// Exposed for the GitHub Actions crate, whose workflow manifests can carry
/// `container:` / `services:` image references alongside their `uses:`
/// directives.
#[must_use]
pub fn yaml_image_dependencies(text: &str) -> Vec<DependencySpec> {
    to_dependencies(yaml::scan(text))
}

/// Apply image-tag updates to a YAML document.
///
/// Exposed alongside [`yaml_image_dependencies`] so the GitHub Actions patcher
/// can rewrite `image:` values without duplicating the byte-range machinery.
///
/// # Errors
///
/// Never returns an error through this path; the signature matches
/// [`ManifestHandler::apply_updates`] so the two can share a call site.
///
/// # Panics
///
/// Panics only if two patches would overlap, which the line-based scanner
/// cannot produce: every tag occupies a distinct, byte-disjoint span and each
/// located tag is consumed at most once. A panic here means a scanner bug.
pub fn apply_yaml_image_updates(text: &str, updates: &[PlannedUpdate]) -> Result<String, DcuError> {
    Ok(patcher::apply(text, &yaml::scan(text), updates).expect("image tag patches never overlap"))
}

/// Handler for Dockerfile build definitions.
pub struct DockerfileHandler;

impl ManifestHandler for DockerfileHandler {
    fn parse(&self, text: &str, path: &Path) -> Result<ParsedManifest, DcuError> {
        Ok(ParsedManifest {
            manifest_ref: ManifestRef {
                path: path.to_path_buf(),
                kind: ManifestKind::Dockerfile,
            },
            dependencies: to_dependencies(parser::scan(text)),
        })
    }

    fn apply_updates(&self, text: &str, updates: &[PlannedUpdate]) -> Result<String, DcuError> {
        // `apply` only fails on overlapping patches, which the line-by-line
        // scanner can never produce: each tag occupies a distinct, byte-disjoint
        // span and every location is consumed at most once.
        Ok(patcher::apply(text, &parser::scan(text), updates)
            .expect("image tag patches never overlap"))
    }
}

/// Handler for Docker Compose project files.
pub struct ComposeHandler;

impl ManifestHandler for ComposeHandler {
    fn parse(&self, text: &str, path: &Path) -> Result<ParsedManifest, DcuError> {
        Ok(ParsedManifest {
            manifest_ref: ManifestRef {
                path: path.to_path_buf(),
                kind: ManifestKind::DockerCompose,
            },
            dependencies: to_dependencies(yaml::scan(text)),
        })
    }

    fn apply_updates(&self, text: &str, updates: &[PlannedUpdate]) -> Result<String, DcuError> {
        apply_yaml_image_updates(text, updates)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const DOCKERFILE: &str = concat!(
        "# syntax=docker/dockerfile:1\n",
        "FROM node:20-alpine AS deps\n",
        "WORKDIR /app\n",
        "\n",
        "FROM node:latest AS scratchpad\n",
        "FROM gcr.io/distroless/nodejs20-debian12@sha256:abc123\n",
        "FROM deps\n",
    );

    const COMPOSE: &str = concat!(
        "services:\n",
        "  db:\n",
        "    image: postgres:16-alpine  # pinned\n",
        "  cache:\n",
        "    image: 'redis:7.2'\n",
        "  app:\n",
        "    image: app:${TAG}\n",
    );

    fn update(name: &str, from: &str, to: &str) -> PlannedUpdate {
        PlannedUpdate {
            name: name.to_owned(),
            section: DependencySection::DockerImage,
            from: from.to_owned(),
            to: to.to_owned(),
        }
    }

    #[test]
    fn dockerfile_handler_collects_only_trackable_images() {
        let parsed = DockerfileHandler
            .parse(DOCKERFILE, Path::new("Dockerfile"))
            .expect("Dockerfiles always parse");

        assert_eq!(parsed.manifest_ref.kind, ManifestKind::Dockerfile);
        // `:latest`, the digest pin, and the `FROM deps` stage reference are
        // all deliberately skipped.
        assert_eq!(parsed.dependencies.len(), 1);
        assert_eq!(parsed.dependencies[0].name, "node");
        assert_eq!(parsed.dependencies[0].current_req, "20-alpine");
        assert_eq!(
            parsed.dependencies[0].section,
            DependencySection::DockerImage
        );
    }

    #[test]
    fn dockerfile_handler_rewrites_only_the_tag() {
        let result = DockerfileHandler
            .apply_updates(DOCKERFILE, &[update("node", "20-alpine", "22-alpine")])
            .expect("patch must apply");

        assert!(result.contains("FROM node:22-alpine AS deps"));
        // Everything the scanner skipped must survive byte-for-byte.
        assert!(result.contains("# syntax=docker/dockerfile:1"));
        assert!(result.contains("FROM node:latest AS scratchpad"));
        assert!(result.contains("@sha256:abc123"));
        assert!(result.contains("FROM deps"));
    }

    #[test]
    fn compose_handler_collects_only_trackable_images() {
        let parsed = ComposeHandler
            .parse(COMPOSE, Path::new("compose.yaml"))
            .expect("Compose files always parse");

        assert_eq!(parsed.manifest_ref.kind, ManifestKind::DockerCompose);
        // The `${TAG}` interpolation is skipped; the other two are tracked.
        let names: Vec<&str> = parsed
            .dependencies
            .iter()
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(names, vec!["postgres", "redis"]);
    }

    #[test]
    fn compose_handler_preserves_comments_and_quotes() {
        let result = ComposeHandler
            .apply_updates(
                COMPOSE,
                &[
                    update("postgres", "16-alpine", "17-alpine"),
                    update("redis", "7.2", "7.4"),
                ],
            )
            .expect("patch must apply");

        assert!(result.contains("image: postgres:17-alpine  # pinned"));
        assert!(result.contains("image: 'redis:7.4'"));
        assert!(result.contains("image: app:${TAG}"));
    }

    #[test]
    fn yaml_helpers_serve_the_github_crate() {
        // The workflow shape: a job container plus a service container.
        let workflow = concat!(
            "jobs:\n",
            "  test:\n",
            "    container:\n",
            "      image: node:20-alpine\n",
            "    services:\n",
            "      redis:\n",
            "        image: redis:7.2\n",
            "    steps:\n",
            "      - uses: actions/checkout@v5\n",
        );

        let deps = yaml_image_dependencies(workflow);
        assert_eq!(deps.len(), 2);
        assert_eq!(deps[0].name, "node");
        assert_eq!(deps[1].name, "redis");

        let patched = apply_yaml_image_updates(workflow, &[update("redis", "7.2", "7.4")])
            .expect("patch must apply");
        assert!(patched.contains("image: redis:7.4"));
        // The `uses:` directive belongs to the GitHub scanner and must be
        // untouched by this one.
        assert!(patched.contains("uses: actions/checkout@v5"));
    }
}
