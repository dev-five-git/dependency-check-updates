//! Container image reference parsing.
//!
//! An image reference is `[registry[:port]/]name[:tag][@digest]`. Splitting it
//! correctly is subtle in exactly two places, and both are load-bearing here:
//!
//! 1. The `:` that introduces a **port** (`localhost:5000/app`) looks
//!    identical to the one that introduces a **tag**. Only a `:` that appears
//!    after the last `/` can be a tag.
//! 2. Whether the first path segment is a **registry host** or the first half
//!    of a Docker Hub namespace (`library/node` vs `ghcr.io/org/app`) is
//!    decided by the same rule the Docker CLI itself uses: a segment
//!    containing `.` or `:`, or spelled exactly `localhost`, is a host.

use crate::tag::TagShape;

/// The default registry a bare image name resolves against.
pub(crate) const DEFAULT_REGISTRY_HOST: &str = "registry-1.docker.io";

/// Namespace Docker Hub gives to its own official (single-segment) images.
const DOCKER_HUB_OFFICIAL_NAMESPACE: &str = "library";

/// A parsed image reference, borrowed from the source text.
#[derive(Debug, PartialEq, Eq)]
pub struct ImageRef<'a> {
    /// Everything before the tag / digest, exactly as written
    /// (`node`, `ghcr.io/dev-five-git/api`).
    pub name: &'a str,
    /// The tag, if one was written. `None` for a bare `node` (implicitly
    /// `latest`) or a digest-only pin.
    pub tag: Option<&'a str>,
    /// The `sha256:…` digest, if the reference is content-addressed.
    pub digest: Option<&'a str>,
}

impl<'a> ImageRef<'a> {
    /// Parse `text` as an image reference.
    ///
    /// Returns `None` only for an empty name, which the callers treat as
    /// "not an image" rather than as an error — a malformed `FROM` line is
    /// the Docker daemon's problem to report, not this tool's.
    #[must_use]
    pub fn parse(text: &'a str) -> Option<Self> {
        // The digest is unambiguous: `@` cannot appear anywhere else.
        let (before_digest, digest) = match text.split_once('@') {
            Some((head, digest)) => (head, Some(digest)),
            None => (text, None),
        };

        // A `:` only introduces a tag when it sits after the final `/`;
        // otherwise it is a registry port (`localhost:5000/app`).
        let last_slash = before_digest.rfind('/').map_or(0, |i| i + 1);
        let (name, tag) = match before_digest[last_slash..].find(':') {
            Some(rel) => {
                let at = last_slash + rel;
                (&before_digest[..at], Some(&before_digest[at + 1..]))
            }
            None => (before_digest, None),
        };

        if name.is_empty() {
            return None;
        }

        Some(Self { name, tag, digest })
    }

    /// Split [`Self::name`] into the registry host to query and the repository
    /// path within it.
    ///
    /// ```text
    /// node                    → ("registry-1.docker.io", "library/node")
    /// grafana/grafana         → ("registry-1.docker.io", "grafana/grafana")
    /// ghcr.io/org/app         → ("ghcr.io",              "org/app")
    /// localhost:5000/app      → ("localhost:5000",       "app")
    /// ```
    ///
    /// The repository is `Cow`-free: the Docker Hub official-image case is the
    /// only one that needs to synthesise a string, and it is returned owned so
    /// callers get one uniform type to key their per-repository cache on.
    #[must_use]
    pub fn registry_target(&self) -> (&'a str, String) {
        match self.name.split_once('/') {
            Some((first, rest)) if is_registry_host(first) => (first, rest.to_owned()),
            // Two-segment Hub reference (`grafana/grafana`) — already a full
            // repository path.
            Some(_) => (DEFAULT_REGISTRY_HOST, self.name.to_owned()),
            // Single segment — a Docker Hub official image, which lives under
            // the implicit `library/` namespace.
            None => (
                DEFAULT_REGISTRY_HOST,
                format!("{DOCKER_HUB_OFFICIAL_NAMESPACE}/{}", self.name),
            ),
        }
    }
}

/// A tracked image reference located in the source text.
///
/// Produced by both the Dockerfile `FROM` scanner and the YAML `image:`
/// scanner, and consumed by the patcher — the byte range covers exactly the
/// tag, so quotes, trailing comments, and the image name itself are never
/// touched by a rewrite.
#[derive(Debug)]
pub(crate) struct ImageLocation {
    /// Image name without tag or digest, verbatim from the source.
    pub name: String,
    /// The tag as written.
    pub tag: String,
    /// Absolute byte offset (inclusive) of the first tag byte.
    pub tag_start: usize,
    /// Absolute byte offset (exclusive) one past the last tag byte.
    pub tag_end: usize,
}

/// Decide whether `reference` is an image pin this tool may update, and if so
/// locate its tag.
///
/// `ref_offset` is the absolute byte offset at which `reference` begins in the
/// document, so the returned range is document-absolute.
///
/// This is the single place every "leave it alone" rule lives, so the
/// Dockerfile and YAML scanners cannot drift apart:
///
/// - **`$` interpolation** (`node:${NODE_VERSION}`, `image: app:$TAG`) — the
///   effective tag is decided elsewhere (a build arg, a `.env` file), so
///   rewriting the literal text would be guesswork.
/// - **Digest pins** (`node:20@sha256:…`) — the digest, not the tag, decides
///   what gets pulled. Moving the tag alone changes nothing at pull time while
///   making the reference self-contradictory.
/// - **No tag** (`FROM node`, `FROM builder`) — an implicit `latest`, or a
///   multi-stage build stage name. Both are moving targets by construction,
///   and stage names never carry a tag, so this rule covers them too.
/// - **Non-version tags** (`:latest`, `:bookworm`, `:1a2b3c4`) — rejected by
///   [`TagShape::parse`], the same way `@main` is for GitHub Actions.
pub(crate) fn locate(reference: &str, ref_offset: usize) -> Option<ImageLocation> {
    if reference.contains('$') {
        return None;
    }

    let parsed = ImageRef::parse(reference)?;
    if parsed.digest.is_some() {
        return None;
    }
    let tag = parsed.tag?;
    TagShape::parse(tag)?;

    // With no digest, the tag is always a suffix of `reference`, so its start
    // is a pure length subtraction — no pointer arithmetic needed.
    let tag_start = ref_offset + (reference.len() - tag.len());

    Some(ImageLocation {
        name: parsed.name.to_owned(),
        tag: tag.to_owned(),
        tag_start,
        tag_end: tag_start + tag.len(),
    })
}

/// Whether the first path segment of an image name is a registry host rather
/// than a Docker Hub namespace.
///
/// This mirrors the Docker CLI's own heuristic (`reference.splitDockerDomain`):
/// a host must contain a `.` (`ghcr.io`, `mcr.microsoft.com`) or a `:` port
/// (`localhost:5000`), or be exactly `localhost`. Everything else — `library`,
/// `grafana`, `bitnami` — is a Hub namespace.
fn is_registry_host(segment: &str) -> bool {
    segment == "localhost" || segment.contains('.') || segment.contains(':')
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    // Bare official image, no tag.
    #[case::bare("node", "node", None, None)]
    // The common cases: official and namespaced images with a tag.
    #[case::official_tagged("node:20-alpine", "node", Some("20-alpine"), None)]
    #[case::namespaced_tagged("grafana/grafana:11.3.0", "grafana/grafana", Some("11.3.0"), None)]
    // Explicit registry hosts.
    #[case::ghcr("ghcr.io/org/app:v1.2.3", "ghcr.io/org/app", Some("v1.2.3"), None)]
    #[case::mcr(
        "mcr.microsoft.com/dotnet/sdk:8.0",
        "mcr.microsoft.com/dotnet/sdk",
        Some("8.0"),
        None
    )]
    // A registry PORT must not be mistaken for a tag — the `:` before the
    // last `/` belongs to the host.
    #[case::host_port_no_tag("localhost:5000/app", "localhost:5000/app", None, None)]
    #[case::host_port_with_tag("localhost:5000/app:1.2", "localhost:5000/app", Some("1.2"), None)]
    // Digest pins, with and without an accompanying tag.
    #[case::digest_only("node@sha256:abc123", "node", None, Some("sha256:abc123"))]
    #[case::tag_and_digest(
        "node:20-alpine@sha256:abc123",
        "node",
        Some("20-alpine"),
        Some("sha256:abc123")
    )]
    fn parse_cases(
        #[case] input: &str,
        #[case] name: &str,
        #[case] tag: Option<&str>,
        #[case] digest: Option<&str>,
    ) {
        let parsed = ImageRef::parse(input).expect("reference must parse");
        assert_eq!(parsed, ImageRef { name, tag, digest });
    }

    #[rstest]
    // An empty name is the one input treated as "not an image".
    #[case::empty("")]
    #[case::tag_without_name(":1.0")]
    #[case::digest_without_name("@sha256:abc")]
    fn parse_rejects_empty_name(#[case] input: &str) {
        assert_eq!(ImageRef::parse(input), None);
    }

    #[rstest]
    // Single segment → Docker Hub official namespace.
    #[case::official("node:20", DEFAULT_REGISTRY_HOST, "library/node")]
    // Two segments whose head is not host-like → Hub user namespace.
    #[case::hub_namespace("grafana/grafana:11", DEFAULT_REGISTRY_HOST, "grafana/grafana")]
    // Dotted head → registry host.
    #[case::ghcr("ghcr.io/org/app:v1", "ghcr.io", "org/app")]
    #[case::deep_path(
        "mcr.microsoft.com/dotnet/aspnet/runtime:8.0",
        "mcr.microsoft.com",
        "dotnet/aspnet/runtime"
    )]
    // `localhost` (no dot, no port) is host-like by special case.
    #[case::localhost("localhost/app:1", "localhost", "app")]
    // Ported host.
    #[case::localhost_port("localhost:5000/team/app:1", "localhost:5000", "team/app")]
    fn registry_target_cases(
        #[case] input: &str,
        #[case] expected_host: &str,
        #[case] expected_repo: &str,
    ) {
        let parsed = ImageRef::parse(input).expect("reference must parse");
        let (host, repo) = parsed.registry_target();
        assert_eq!(host, expected_host);
        assert_eq!(repo, expected_repo);
    }

    #[rstest]
    // Happy path: the tag range slices back to exactly the tag, with the
    // offset shifted by the caller-supplied document position.
    #[case::official("node:20-alpine", 7, Some(("node", "20-alpine")))]
    #[case::registry_host("ghcr.io/org/app:v1.2.3", 0, Some(("ghcr.io/org/app", "v1.2.3")))]
    #[case::ported_host("localhost:5000/app:1.2", 3, Some(("localhost:5000/app", "1.2")))]
    // Every skip rule, one case each.
    #[case::interpolated_tag("node:${NODE_VERSION}", 0, None)]
    #[case::interpolated_name("$REGISTRY/app:1.0", 0, None)]
    #[case::digest_pin("node:20@sha256:abc123", 0, None)]
    #[case::digest_only("node@sha256:abc123", 0, None)]
    #[case::no_tag("node", 0, None)]
    #[case::stage_alias("builder", 0, None)]
    #[case::latest("node:latest", 0, None)]
    #[case::codename("debian:bookworm", 0, None)]
    #[case::build_hash("app:1a2b3c4", 0, None)]
    fn locate_cases(
        #[case] reference: &str,
        #[case] offset: usize,
        #[case] expected: Option<(&str, &str)>,
    ) {
        let located = locate(reference, offset);
        match expected {
            Some((name, tag)) => {
                let located = located.expect("reference must be tracked");
                assert_eq!(located.name, name);
                assert_eq!(located.tag, tag);
                // The recorded range must slice the ORIGINAL reference back to
                // the tag once the caller's offset is removed.
                assert_eq!(
                    &reference[located.tag_start - offset..located.tag_end - offset],
                    tag
                );
            }
            None => assert!(located.is_none(), "{reference} must not be tracked"),
        }
    }

    #[rstest]
    #[case::dotted("ghcr.io", true)]
    #[case::dotted_deep("mcr.microsoft.com", true)]
    #[case::localhost("localhost", true)]
    #[case::ported("registry:5000", true)]
    #[case::hub_namespace("grafana", false)]
    #[case::hub_library("library", false)]
    fn is_registry_host_cases(#[case] segment: &str, #[case] expected: bool) {
        assert_eq!(is_registry_host(segment), expected);
    }
}
