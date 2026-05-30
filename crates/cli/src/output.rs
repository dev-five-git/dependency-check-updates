//! ncu-style table output with colored version diffs.

use std::fmt::Write;

use dependency_check_updates_core::{BumpType, PlannedUpdate};
use owo_colors::OwoColorize;

/// Determine the type of version bump by comparing version strings.
#[must_use]
pub fn detect_bump_type(from: &str, to: &str) -> BumpType {
    let from_parts = parse_version_parts(from);
    let to_parts = parse_version_parts(to);

    if from_parts.0 != to_parts.0 {
        BumpType::Major
    } else if from_parts.1 != to_parts.1 {
        BumpType::Minor
    } else {
        BumpType::Patch
    }
}

/// Parse major.minor.patch from a version string, stripping range prefixes.
fn parse_version_parts(v: &str) -> (u64, u64, u64) {
    let cleaned = v.trim_start_matches(|c: char| !c.is_ascii_digit());
    let mut parts = cleaned.splitn(3, '.');
    let major = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let minor = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let patch = parts
        .next()
        .and_then(|s| {
            // Handle "3-beta.1" -> take digits only
            let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        })
        .unwrap_or(0);
    (major, minor, patch)
}

/// Colorize a version string based on bump type.
fn colorize_version(version: &str, bump: BumpType, use_color: bool) -> String {
    if !use_color {
        return version.to_owned();
    }
    match bump {
        BumpType::Major => format!("{}", version.red()),
        BumpType::Minor => format!("{}", version.cyan()),
        BumpType::Patch => format!("{}", version.green()),
    }
}

/// Render a table of planned updates in ncu-style format.
///
/// Duplicate `(name, from, to)` triples are collapsed to a single row — a
/// GitHub Actions workflow that calls `actions/checkout@v5` from twelve jobs
/// would otherwise emit twelve identical rows, drowning the diff in noise.
/// The patch engine still applies every occurrence; only the display dedupes.
///
/// Format:
/// ```text
///  react          ^17.0.0  ->  ^18.2.0
///  typescript     ^4.0.0   ->  ^5.3.0
/// ```
#[must_use]
pub fn render_table(updates: &[PlannedUpdate], use_color: bool) -> String {
    if updates.is_empty() {
        return String::new();
    }

    let unique = dedupe_updates(updates);

    // Calculate column widths against the deduped set so columns stay tight.
    let max_name = unique.iter().map(|u| u.name.len()).max().unwrap_or(0);
    let max_from = unique.iter().map(|u| u.from.len()).max().unwrap_or(0);
    let max_to = unique.iter().map(|u| u.to.len()).max().unwrap_or(0);

    let mut output = String::new();

    for update in &unique {
        let bump = detect_bump_type(&update.from, &update.to);
        let colored_to = colorize_version(&update.to, bump, use_color);

        let _ = writeln!(
            output,
            " {:<name_w$}  {:>from_w$}  ->  {:<to_w$}",
            update.name,
            update.from,
            colored_to,
            name_w = max_name,
            from_w = max_from,
            to_w = max_to,
        );
    }

    output
}

/// Collapse `updates` by `(name, from, to)` while preserving original order.
///
/// Lifted out of [`render_table`] so [`render_json`] can apply the same dedup
/// without duplicating logic. Returns references so we avoid cloning.
fn dedupe_updates(updates: &[PlannedUpdate]) -> Vec<&PlannedUpdate> {
    let mut seen = std::collections::HashSet::new();
    updates
        .iter()
        .filter(|u| seen.insert((u.name.as_str(), u.from.as_str(), u.to.as_str())))
        .collect()
}

/// Render the header line.
#[must_use]
pub fn render_header(path: &str, upgrading: bool) -> String {
    if upgrading {
        format!("Upgrading {path}\n")
    } else {
        format!("Checking {path}\n")
    }
}

/// Render the footer hint.
#[must_use]
pub fn render_footer(path: &str, upgrading: bool, has_updates: bool, use_color: bool) -> String {
    if !has_updates {
        let smiley = if use_color {
            format!("{}", ":)".green().bold())
        } else {
            ":)".to_owned()
        };
        return format!("All dependencies match the latest package versions {smiley}\n");
    }

    if upgrading {
        "Run your package manager to install new versions.\n".to_owned()
    } else {
        let cmd = if use_color {
            format!("{}", "dcu -u".cyan())
        } else {
            "dcu -u".to_owned()
        };
        format!("\nRun {cmd} to upgrade {path}\n")
    }
}

/// Render updates as JSON.
///
/// Output shape is `{ "name": "to" }`. Duplicate package names (the same
/// action used in many workflow jobs) collapse naturally because JSON object
/// keys are unique — last-write-wins on `to`. For consumers that need full
/// `(name, from, to)` triples, use the table format and parse line-by-line.
#[must_use]
pub fn render_json(updates: &[PlannedUpdate]) -> String {
    let mut map = serde_json::Map::new();
    for update in dedupe_updates(updates) {
        map.insert(
            update.name.clone(),
            serde_json::Value::String(update.to.clone()),
        );
    }
    serde_json::to_string_pretty(&serde_json::Value::Object(map)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dependency_check_updates_core::DependencySection;
    use rstest::rstest;

    /// Build a `PlannedUpdate` with a default `Dependencies` section so cases
    /// stay readable as `(name, from, to)` tuples.
    fn upd(name: &str, from: &str, to: &str) -> PlannedUpdate {
        PlannedUpdate {
            name: name.to_owned(),
            section: DependencySection::Dependencies,
            from: from.to_owned(),
            to: to.to_owned(),
        }
    }

    /// Build a `PlannedUpdate` in the `GitHubActions` section — the source of
    /// real-world duplicate rows the dedup logic guards against.
    fn gha(name: &str, from: &str, to: &str) -> PlannedUpdate {
        PlannedUpdate {
            name: name.to_owned(),
            section: DependencySection::GitHubActions,
            from: from.to_owned(),
            to: to.to_owned(),
        }
    }

    #[rstest]
    #[case::major("^1.0.0", "^2.0.0", BumpType::Major)]
    #[case::minor("^1.0.0", "^1.1.0", BumpType::Minor)]
    #[case::patch("^1.0.0", "^1.0.1", BumpType::Patch)]
    fn detect_bump_type_cases(
        #[case] from: &str,
        #[case] to: &str,
        #[case] expected: BumpType,
    ) {
        assert_eq!(detect_bump_type(from, to), expected);
    }

    #[test]
    fn test_render_table_basic() {
        let updates = vec![
            upd("react", "^17.0.0", "^18.2.0"),
            upd("lodash", "^4.17.0", "^4.17.21"),
        ];

        let output = render_table(&updates, false);
        assert!(output.contains("react"));
        assert!(output.contains("^17.0.0"));
        assert!(output.contains("^18.2.0"));
        assert!(output.contains("lodash"));
        assert!(output.contains("->"));
    }

    #[test]
    fn test_render_table_empty() {
        let output = render_table(&[], false);
        assert!(output.is_empty());
    }

    #[rstest]
    // (upgrading, has_updates, use_color, expected substring)
    #[case::no_updates(false, false, false, "All dependencies match")]
    #[case::with_updates_dry_run(false, true, false, "dcu -u")]
    #[case::after_upgrade(true, true, false, "install new versions")]
    #[case::no_updates_with_color(false, false, true, "All dependencies match")]
    #[case::dry_run_with_color(false, true, true, "dcu -u")]
    fn render_footer_cases(
        #[case] upgrading: bool,
        #[case] has_updates: bool,
        #[case] use_color: bool,
        #[case] needle: &str,
    ) {
        let output = render_footer("package.json", upgrading, has_updates, use_color);
        assert!(
            output.contains(needle),
            "expected {needle:?} in footer, got: {output:?}"
        );
    }

    #[test]
    fn test_render_json() {
        let updates = vec![upd("react", "^17.0.0", "^18.2.0")];

        let output = render_json(&updates);
        assert!(output.contains("\"react\""));
        assert!(output.contains("\"^18.2.0\""));
    }

    #[rstest]
    // Colored cases assert the bare version digits survive and that the
    // returned string is NOT equal to the input (ANSI escapes were applied).
    #[case::major_with_color("^2.0.0", BumpType::Major, true, "2.0.0", false)]
    #[case::minor_with_color("^1.1.0", BumpType::Minor, true, "1.1.0", false)]
    #[case::patch_with_color("^1.0.1", BumpType::Patch, true, "1.0.1", false)]
    // No color: the result must equal the input verbatim.
    #[case::no_color("^2.0.0", BumpType::Major, false, "^2.0.0", true)]
    fn colorize_version_cases(
        #[case] version: &str,
        #[case] bump: BumpType,
        #[case] use_color: bool,
        #[case] expected_substr: &str,
        #[case] expect_equal_input: bool,
    ) {
        let result = colorize_version(version, bump, use_color);
        assert!(
            result.contains(expected_substr),
            "{result:?} should contain {expected_substr:?}"
        );
        if expect_equal_input {
            assert_eq!(result, version);
        } else {
            assert_ne!(result, version, "expected ANSI color codes to be applied");
        }
    }

    #[test]
    fn test_render_table_with_color() {
        let updates = vec![upd("react", "^17.0.0", "^18.2.0")];
        let output = render_table(&updates, true);
        assert!(output.contains("react"));
        assert!(output.contains("^17.0.0"));
        // Colored version should have ANSI codes
        assert!(output.len() > "react  ^17.0.0  ->  ^18.2.0\n".len());
    }

    #[rstest]
    #[case::checking("package.json", false, "Checking package.json\n")]
    #[case::upgrading("Cargo.toml", true, "Upgrading Cargo.toml\n")]
    fn render_header_cases(
        #[case] path: &str,
        #[case] upgrading: bool,
        #[case] expected: &str,
    ) {
        assert_eq!(render_header(path, upgrading), expected);
    }

    #[test]
    fn test_parse_version_parts_with_prerelease() {
        // "3-beta.1" should parse major=3
        let (major, minor, patch) = parse_version_parts("3.0.0-beta.1");
        assert_eq!(major, 3);
        assert_eq!(minor, 0);
        assert_eq!(patch, 0);
    }

    #[test]
    fn test_render_json_empty() {
        let output = render_json(&[]);
        assert_eq!(output, "{}");
    }

    #[test]
    fn test_render_table_dedupes_duplicate_updates() {
        // Real GitHub Actions case: `actions/checkout@v5` used in 3 jobs of
        // one workflow. compute_updates emits 3 identical PlannedUpdates.
        // render_table should show only 1 row.
        let updates = vec![
            gha("actions/checkout", "v5", "v6"),
            gha("actions/checkout", "v5", "v6"),
            gha("actions/checkout", "v5", "v6"),
        ];
        let output = render_table(&updates, false);
        let occurrences = output.matches("actions/checkout").count();
        assert_eq!(occurrences, 1, "got: {output}");
    }

    #[test]
    fn test_render_table_keeps_different_from_versions_separate() {
        // Same name, different `from` — the workflow uses two different pinned
        // versions of the same action. Both rows must survive.
        let updates = vec![
            gha("actions/checkout", "v4", "v6"),
            gha("actions/checkout", "v5", "v6"),
        ];
        let output = render_table(&updates, false);
        let occurrences = output.matches("actions/checkout").count();
        assert_eq!(occurrences, 2, "got: {output}");
        assert!(output.contains("v4"));
        assert!(output.contains("v5"));
    }

    #[test]
    fn test_render_json_dedupes() {
        let updates = vec![
            gha("actions/checkout", "v5", "v6"),
            gha("actions/checkout", "v5", "v6"),
        ];
        let output = render_json(&updates);
        // Even before dedup, JSON map collapses same key; after dedup the
        // output should still be a single-entry object.
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed.as_object().unwrap().len(), 1);
    }

    #[test]
    fn test_render_json_multiple() {
        let updates = vec![
            upd("a", "1.0", "2.0"),
            PlannedUpdate {
                name: "b".to_owned(),
                section: DependencySection::DevDependencies,
                from: "3.0".to_owned(),
                to: "4.0".to_owned(),
            },
        ];
        let output = render_json(&updates);
        assert!(output.contains("\"a\""));
        assert!(output.contains("\"b\""));
        assert!(output.contains("\"2.0\""));
        assert!(output.contains("\"4.0\""));
    }
}
