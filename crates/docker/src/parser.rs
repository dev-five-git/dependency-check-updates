//! Line-based scanner for Dockerfile `FROM` instructions.
//!
//! Dockerfiles have no round-trippable object model — `docker` itself parses
//! them line by line — so the scanner walks the text the same way, records the
//! absolute byte offsets of each tracked image tag, and lets the patcher
//! replace only those bytes.
//!
//! Only `FROM` is scanned. `COPY --from=<image>` can also name an image, but
//! it far more often names a build stage, and the two are textually
//! indistinguishable at the line level; leaving it alone keeps the scanner
//! from ever rewriting a stage reference.

use crate::image::{ImageLocation, locate};

/// The instruction keyword introducing a base image.
const FROM: &str = "FROM";

/// Scan Dockerfile text and return every tracked `FROM` image tag.
///
/// Infallible: a malformed line simply yields no match rather than aborting
/// the scan.
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

/// Scan a single line, returning its image location iff the line is a `FROM`
/// instruction naming an image this tool may update.
fn scan_line(line: &str, line_offset: usize) -> Option<ImageLocation> {
    let indent = line.len() - line.trim_start().len();
    let rest = line.get(indent..)?;

    // `FROM` is case-insensitive in Dockerfiles, and must be followed by
    // whitespace — `FROMAGE` is not an instruction, and a `# FROM …` comment
    // fails the check because the `#` is the first non-whitespace byte.
    if !rest
        .get(..FROM.len())
        .is_some_and(|kw| kw.eq_ignore_ascii_case(FROM))
    {
        return None;
    }
    if !rest
        .as_bytes()
        .get(FROM.len())
        .is_some_and(u8::is_ascii_whitespace)
    {
        return None;
    }

    // Walk past any build flags (`--platform=linux/amd64`) that sit between
    // the keyword and the image reference.
    let mut cursor = indent + FROM.len();
    loop {
        let (token, start) = next_token(line, cursor)?;
        cursor = start + token.len();
        if token.starts_with("--") {
            continue;
        }
        return locate(token, line_offset + start);
    }
}

/// Return the next whitespace-delimited token in `line` at or after byte
/// offset `from`, together with its start offset.
///
/// Splitting on ASCII whitespace only means every index produced here lands on
/// a UTF-8 boundary, so the returned slice is always valid. A trailing `\n` or
/// `\r\n` terminates the final token like any other whitespace.
fn next_token(line: &str, from: usize) -> Option<(&str, usize)> {
    let bytes = line.as_bytes();
    let mut i = from;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    let start = i;
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    Some((&line[start..i], start))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    // Canonical single-stage build.
    #[case::simple("FROM node:20-alpine\n", "node", "20-alpine")]
    // Multi-stage: the `AS <stage>` tail must not bleed into the reference.
    #[case::with_stage_alias("FROM node:20-alpine AS builder\n", "node", "20-alpine")]
    // Build flags sit between the keyword and the image.
    #[case::platform_flag("FROM --platform=linux/amd64 node:20-alpine\n", "node", "20-alpine")]
    #[case::multiple_flags(
        "FROM --platform=$BUILDPLATFORM --foo=bar python:3.12-slim\n",
        "python",
        "3.12-slim"
    )]
    // The keyword is case-insensitive and may be indented.
    #[case::lowercase_keyword("from node:20\n", "node", "20")]
    #[case::mixed_case_keyword("FrOm node:20\n", "node", "20")]
    #[case::indented("   FROM node:20\n", "node", "20")]
    // Line-ending and EOF variations.
    #[case::crlf("FROM node:20\r\n", "node", "20")]
    #[case::no_trailing_newline("FROM node:20", "node", "20")]
    // Registry hosts, including one with a port that must not be read as a tag.
    #[case::registry_host("FROM ghcr.io/org/app:v1.2.3\n", "ghcr.io/org/app", "v1.2.3")]
    #[case::ported_host("FROM localhost:5000/app:1.2\n", "localhost:5000/app", "1.2")]
    fn scan_yields_single_match(
        #[case] dockerfile: &str,
        #[case] expected_name: &str,
        #[case] expected_tag: &str,
    ) {
        let locations = scan(dockerfile);
        assert_eq!(locations.len(), 1, "got: {locations:?}");
        assert_eq!(locations[0].name, expected_name);
        assert_eq!(locations[0].tag, expected_tag);
        // The recorded offsets must slice the original text back to the tag.
        assert_eq!(
            &dockerfile[locations[0].tag_start..locations[0].tag_end],
            expected_tag
        );
    }

    #[rstest]
    // Not a FROM instruction.
    #[case::run_line("RUN echo FROM node:20\n")]
    #[case::comment("# FROM node:20\n")]
    #[case::keyword_prefix_only("FROMAGE node:20\n")]
    #[case::bare_keyword("FROM\n")]
    // A FROM whose image this tool deliberately leaves alone.
    #[case::stage_reference("FROM builder\n")]
    #[case::scratch("FROM scratch\n")]
    #[case::implicit_latest("FROM node\n")]
    #[case::explicit_latest("FROM node:latest\n")]
    #[case::codename_tag("FROM debian:bookworm\n")]
    #[case::digest_pin("FROM node:20@sha256:abc123\n")]
    #[case::build_arg_tag("FROM node:${NODE_VERSION}\n")]
    #[case::build_arg_name("FROM $REGISTRY/node:20\n")]
    fn scan_yields_no_matches(#[case] dockerfile: &str) {
        let locations = scan(dockerfile);
        assert!(
            locations.is_empty(),
            "expected no matches, got {locations:?}"
        );
    }

    #[test]
    fn scan_multi_stage_build_records_every_tracked_stage() {
        let dockerfile = concat!(
            "FROM node:20-alpine AS deps\n",
            "WORKDIR /app\n",
            "\n",
            "FROM node:20-alpine AS builder\n",
            "COPY --from=deps /app/node_modules ./node_modules\n",
            "\n",
            "# The runtime stage intentionally pins a digest.\n",
            "FROM gcr.io/distroless/nodejs20-debian12@sha256:abc123\n",
            "FROM builder\n",
        );

        let locations = scan(dockerfile);
        assert_eq!(locations.len(), 2, "got: {locations:?}");
        for location in &locations {
            assert_eq!(location.name, "node");
            assert_eq!(location.tag, "20-alpine");
            assert_eq!(
                &dockerfile[location.tag_start..location.tag_end],
                "20-alpine"
            );
        }
        // The two occurrences must be recorded at distinct, ascending offsets
        // so the patcher can rewrite each independently.
        assert!(locations[0].tag_start < locations[1].tag_start);
    }

    #[rstest]
    #[case::from_start("  a bc  ", 0, Some(("a", 2)))]
    #[case::mid_line("  a bc  ", 3, Some(("bc", 4)))]
    #[case::trailing_whitespace_only("  a bc  ", 6, None)]
    #[case::past_end("  a", 99, None)]
    fn next_token_cases(
        #[case] line: &str,
        #[case] from: usize,
        #[case] expected: Option<(&str, usize)>,
    ) {
        assert_eq!(next_token(line, from), expected);
    }
}
