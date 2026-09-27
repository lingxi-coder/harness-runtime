//! Smoke test: `kill_tree_windows` tolerates "process not found".
//!
//! On Windows, `taskkill` returns exit 128 for "process not found", which
//! `kill_tree_windows` maps to `Ok(())`. On non-Windows dev hosts taskkill
//! isn't installed; we expect a spawn-level [`ProcessError::Io`] error so
//! the test still pulls the function into the build graph.

use platform_windows::process::kill_tree::kill_tree_windows;

#[tokio::test]
async fn kill_tree_windows_handles_nonexistent_pid_gracefully() {
    let result = kill_tree_windows(999_999_999).await;
    #[cfg(target_os = "windows")]
    assert!(
        result.is_ok(),
        "expected Ok on Windows for not-found pid: {result:?}"
    );
    #[cfg(not(target_os = "windows"))]
    {
        // On non-Windows, taskkill isn't installed; we expect Io(spawn) error.
        assert!(result.is_err(), "expected Err on non-Windows: {result:?}");
    }
}
