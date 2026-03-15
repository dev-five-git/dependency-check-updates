//! N-API FFI bindings for npm distribution of dcu.
//!
//! Wraps the dcu CLI as an async N-API function callable from Node.js.

use napi::{Error, Result};
use napi_derive::napi;

/// Run the dcu CLI with the current process arguments.
///
/// # Errors
///
/// Returns an error if the CLI command execution fails.
#[napi]
pub async fn main() -> Result<()> {
    dcu_cli::main(&std::env::args().collect::<Vec<String>>())
        .await
        .map_err(|e| Error::from_reason(e.to_string()))
}
