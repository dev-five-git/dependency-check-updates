//! N-API FFI bindings for npm distribution of dependency-check-updates.
//!
//! Wraps the dependency-check-updates CLI as an async N-API function callable from Node.js.

use napi::{Error, Result};
use napi_derive::napi;

/// Run the dependency-check-updates CLI with the given user arguments.
///
/// `args` must contain only the user-supplied arguments (typically
/// `process.argv.slice(2)` on the Node.js side). Do **not** include the Node
/// runtime path or the script path — those are part of Node's `process.argv`
/// but are meaningless to the CLI parser and would be misinterpreted as the
/// first positional argument (`filter`), causing all dependencies to be
/// silently filtered out.
///
/// # Errors
///
/// Returns an error if the CLI command execution fails.
#[napi]
#[cfg(not(tarpaulin_include))]
pub async fn main(args: Vec<String>) -> Result<()> {
    // `Cli::parse_from` treats the first element as the program name and
    // skips it, so we prepend a synthetic program name. This keeps
    // `dependency_check_updates::main` unchanged across the native CLI,
    // the Python bin-bridge, and this N-API bridge.
    let full_args: Vec<String> = std::iter::once("dependency-check-updates".to_string())
        .chain(args)
        .collect();

    dependency_check_updates::main(&full_args)
        .await
        .map_err(|e| Error::from_reason(e.to_string()))
}
