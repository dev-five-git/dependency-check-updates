//! Small cross-ecosystem helpers shared by the registry clients.
//!
//! These were previously copy-pasted byte-for-byte into each ecosystem crate
//! (npm, crates.io, `PyPI`). Centralising them keeps the concurrency and
//! version-string handling in one place.

use tracing::warn;

/// Strip a leading semver range operator from a requirement string, returning
/// the bare numeric version portion.
///
/// Trims every leading character that is not an ASCII digit, so `^1.2.3`,
/// `~1.2.3`, `>=1.0.0`, and `=2.0.0` all collapse to their numeric tail. A
/// spec with no digits (e.g. `*`) yields an empty string.
///
/// ```
/// use dependency_check_updates_core::strip_range_prefix;
/// assert_eq!(strip_range_prefix("^1.2.3"), "1.2.3");
/// assert_eq!(strip_range_prefix(">=2.0.0"), "2.0.0");
/// assert_eq!(strip_range_prefix("*"), "");
/// ```
#[must_use]
pub fn strip_range_prefix(req_str: &str) -> &str {
    req_str.trim_start_matches(|c: char| !c.is_ascii_digit())
}

/// Await a set of spawned tasks, collecting their values and logging (then
/// dropping) any that panicked.
///
/// A `JoinError` means the task panicked or was cancelled; such tasks are
/// omitted from the result rather than aborting the whole batch, so one bad
/// registry lookup never sinks the others.
pub async fn collect_task_results<T>(handles: Vec<tokio::task::JoinHandle<T>>) -> Vec<T> {
    let mut results = Vec::with_capacity(handles.len());
    for handle in handles {
        match handle.await {
            Ok(result) => results.push(result),
            Err(e) => warn!("task join error: {e}"),
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_range_prefix() {
        assert_eq!(strip_range_prefix("^1.2.3"), "1.2.3");
        assert_eq!(strip_range_prefix("~1.0"), "1.0");
        assert_eq!(strip_range_prefix(">=2.0.0"), "2.0.0");
        assert_eq!(strip_range_prefix("=1.0.0"), "1.0.0");
        assert_eq!(strip_range_prefix("1.0.0"), "1.0.0");
        assert_eq!(strip_range_prefix("*"), "");
        assert_eq!(strip_range_prefix(""), "");
    }

    #[tokio::test]
    async fn test_collect_task_results_drops_panicked() {
        // Suppress panic output from the intentionally-panicking task.
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));

        let handles = vec![
            tokio::spawn(async { 1_usize }),
            tokio::spawn(async { panic!("simulated join error") }),
            tokio::spawn(async { 3_usize }),
        ];
        let results = collect_task_results(handles).await;

        std::panic::set_hook(prev_hook);

        // The panicking task is dropped; only the two successful values survive.
        assert_eq!(results.len(), 2);
    }
}
