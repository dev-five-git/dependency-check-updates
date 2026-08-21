//! Line-based scanner for YAML `image:` keys.
//!
//! Compose services and GitHub Actions job containers spell a container image
//! exactly the same way:
//!
//! ```yaml
//! services:
//!   db:
//!     image: postgres:16-alpine   # Compose
//! jobs:
//!   test:
//!     container:
//!       image: node:20-alpine     # workflow job container
//!     services:
//!       redis:
//!         image: redis:7-alpine   # workflow service container
//! ```
//!
//! One scanner therefore serves both, and the GitHub Actions crate calls into
//! it rather than growing a second copy.
//!
//! Compose services that only `build:` locally still carry an `image:` naming
//! the built artefact. Those names are scanned like any other, and the
//! registry lookup for a purely local name simply fails — the failure is
//! reported per-dependency and nothing is written, so the manifest is never
//! corrupted by a name the registry has never heard of.

use dependency_check_updates_core::scalar_value_bounds;

use crate::image::{ImageLocation, locate};

/// Scan YAML text and return every tracked `image:` value.
///
/// Infallible: lines that are not an `image:` mapping key, and images this
/// tool deliberately leaves alone, are skipped without aborting the scan.
pub(crate) fn scan(text: &str) -> Vec<ImageLocation> {
    let mut locations = Vec::new();
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        if let Some(location) = scan_line(line, offset) {
            locations.push(location);
        }
        offset += line.len();
    }
    locations
}

/// Scan a single line for an `image:` mapping key naming a tracked image.
fn scan_line(line: &str, line_offset: usize) -> Option<ImageLocation> {
    let (start, end) = scalar_value_bounds(line, "image:")?;
    locate(line.get(start..end)?, line_offset + start)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    // Compose service, at the indentation Compose files actually use.
    #[case::compose_service(
        "services:\n  db:\n    image: postgres:16-alpine\n",
        "postgres",
        "16-alpine"
    )]
    // Workflow job container / service container.
    #[case::workflow_container(
        "jobs:\n  test:\n    container:\n      image: node:20-alpine\n",
        "node",
        "20-alpine"
    )]
    // Quoting and trailing comments must not bleed into the tag.
    #[case::single_quoted("    image: 'redis:7.4'\n", "redis", "7.4")]
    #[case::double_quoted("    image: \"redis:7.4\"\n", "redis", "7.4")]
    #[case::trailing_comment("    image: redis:7.4  # pinned\n", "redis", "7.4")]
    // List-item form and CRLF endings.
    #[case::list_item("  - image: redis:7.4\n", "redis", "7.4")]
    #[case::crlf("    image: redis:7.4\r\n", "redis", "7.4")]
    // Registry hosts, including a port that must not be read as a tag.
    #[case::registry_host("    image: ghcr.io/org/app:v1.2.3\n", "ghcr.io/org/app", "v1.2.3")]
    #[case::ported_host("    image: localhost:5000/app:1.2\n", "localhost:5000/app", "1.2")]
    fn scan_yields_single_match(
        #[case] yaml: &str,
        #[case] expected_name: &str,
        #[case] expected_tag: &str,
    ) {
        let locations = scan(yaml);
        assert_eq!(locations.len(), 1, "got: {locations:?}");
        assert_eq!(locations[0].name, expected_name);
        assert_eq!(locations[0].tag, expected_tag);
        assert_eq!(
            &yaml[locations[0].tag_start..locations[0].tag_end],
            expected_tag
        );
    }

    #[rstest]
    // `image:` present, but not as a mapping key.
    #[case::inside_comment("    # image: redis:7.4\n")]
    #[case::inside_longer_key("    myimage: redis:7.4\n")]
    #[case::valueless("    image:\n")]
    // Images this tool deliberately leaves alone.
    #[case::latest("    image: redis:latest\n")]
    #[case::no_tag("    image: redis\n")]
    #[case::digest_pin("    image: redis:7.4@sha256:abc123\n")]
    #[case::interpolated("    image: redis:${REDIS_TAG}\n")]
    #[case::compose_env_interpolation("    image: app:${TAG:-latest}\n")]
    fn scan_yields_no_matches(#[case] yaml: &str) {
        let locations = scan(yaml);
        assert!(
            locations.is_empty(),
            "expected no matches, got {locations:?}"
        );
    }

    #[test]
    fn scan_records_every_service_at_distinct_offsets() {
        let yaml = concat!(
            "services:\n",
            "  db:\n",
            "    image: postgres:16-alpine\n",
            "  cache:\n",
            "    image: redis:7-alpine\n",
            "  app:\n",
            "    build: .\n",
            "    image: postgres:16-alpine\n",
        );

        let locations = scan(yaml);
        assert_eq!(locations.len(), 3, "got: {locations:?}");
        assert_eq!(locations[0].name, "postgres");
        assert_eq!(locations[1].name, "redis");
        // The repeated image is recorded again at its own offset so the
        // patcher updates both occurrences.
        assert_eq!(locations[2].name, "postgres");
        assert!(locations[0].tag_start < locations[2].tag_start);
        for location in &locations {
            assert_eq!(&yaml[location.tag_start..location.tag_end], location.tag);
        }
    }
}
