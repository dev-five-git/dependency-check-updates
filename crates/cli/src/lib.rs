//! dependency-check-updates CLI — check and update package dependencies.

#![warn(missing_docs)]

mod cleanup;
mod cli;
mod logging;
mod output;
mod pipeline;
mod run;

pub use cli::{Cli, OutputFormat, parse_args};
pub use run::{main, run};

// Re-exported so bridge crates (napi, maturin) can name the unified error
// type without depending on `dependency-check-updates-core` directly.
pub use dependency_check_updates_core::DcuError;
