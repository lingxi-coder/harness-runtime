//! Drive the platform [`WorktreeManager`] impls through the canonical
//! contract suite. Each driver builds a tempdir, hands it to the factory
//! closure, and lets the suite initialise + exercise the repo.
//!
//! Requires `git` on PATH — assumed available on every M2 CI runner.

use tempfile::TempDir;
use test_harness::contracts::worktree::worktree_manager_contract_tests;

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn posix_worktree_passes_contract() {
    let tmp = TempDir::new().expect("tempdir");
    worktree_manager_contract_tests(tmp.path(), |root| {
        platform_posix::PosixWorktreeManager::new(root.to_path_buf())
    })
    .await;
}

#[cfg(target_os = "windows")]
#[tokio::test]
async fn windows_worktree_passes_contract() {
    let tmp = TempDir::new().expect("tempdir");
    worktree_manager_contract_tests(tmp.path(), |root| {
        platform_windows::WindowsWorktreeManager::new(root.to_path_buf())
    })
    .await;
}
