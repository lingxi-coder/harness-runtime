//! Tree-kill on Windows via `taskkill /T /F /PID`.
//!
//! `/T` walks the process tree, `/F` forces termination. Mirrors the POSIX
//! `killpg(2)` semantics in [`crate::process`]'s sibling crate
//! `lingxi-platform-posix`: terminate the entire descendant tree of a
//! background process and treat "process not found" as success.

use lingxi_core::host::ProcessError;
use tokio::process::Command;

/// Terminate the process tree rooted at `pid` via `taskkill /T /F /PID`.
///
/// Exit code `128` from taskkill means "process not found" — we treat
/// that as success because the caller's only goal is "the tree is gone".
/// Any other non-zero exit is reported as [`ProcessError::Io`].
///
/// # Errors
/// Returns [`ProcessError::Io`] if taskkill cannot be spawned (e.g. the
/// binary is missing on non-Windows hosts) or if it exits with a non-zero,
/// non-128 status.
pub async fn kill_tree_windows(pid: u32) -> Result<(), ProcessError> {
    let output = Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .output()
        .await
        .map_err(|e| ProcessError::Io(format!("spawn taskkill: {e}")))?;
    if output.status.success() {
        return Ok(());
    }
    // Exit code 128 from taskkill means "process not found" — treat as success.
    if output.status.code() == Some(128) {
        return Ok(());
    }
    Err(ProcessError::Io(format!(
        "taskkill exited {}: {}",
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stderr)
    )))
}
