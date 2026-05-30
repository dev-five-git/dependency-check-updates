use std::path::PathBuf;

use clap::Parser;

use dependency_check_updates_core::TargetLevel;

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

/// How results are rendered to stdout.
#[derive(Debug, Clone, Copy, Default, clap::ValueEnum)]
pub enum OutputFormat {
    /// Human-readable ncu-style table (default).
    #[default]
    Table,
    /// Machine-readable JSON object (`{ "name": "to" }`).
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
