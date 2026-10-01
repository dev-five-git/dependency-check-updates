//! dependency-check-updates CLI — check and update package dependencies.

#![warn(missing_docs)]

mod cleanup;
mod cleanup_progress;
mod cli;
mod compatibility;
mod compatibility_rules;
mod compatible;
mod local_tools;
mod logging;
mod maven_access;
mod output;
mod pipeline;
mod project;
#[cfg(test)]
mod project_tests;
mod report;
mod run;
mod tool_registry;
mod transaction;

pub use cli::{Cli, OutputFormat, RecoveryMode, parse_args};
pub use run::{main, run, run_cli};

// Re-exported so bridge crates (napi, maturin) can name the unified error
// type without depending on `dependency-check-updates-core` directly.
pub use dependency_check_updates_core::DcuError;
