//! Stub [`WorktreeManager`] — real `git worktree` shell-out lands in Plan 17.

use async_trait::async_trait;
use lingxi_core::host::{WorktreeError, WorktreeHandle, WorktreeInfo, WorktreeManager};
use std::path::PathBuf;
use std::time::Duration;

/// Stub worktree manager.
#[derive(Default)]
pub struct PosixWorktree;

impl PosixWorktree {
    /// Construct.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl WorktreeManager for PosixWorktree {
    async fn create_worktree(
        &self,
        _slug: &str,
        _base_branch: Option<&str>,
        _copy_includes: &[PathBuf],
    ) -> Result<WorktreeHandle, WorktreeError> {
        Err(WorktreeError::Unsupported)
    }

    async fn remove_worktree(&self, _handle: &WorktreeHandle) -> Result<(), WorktreeError> {
        Ok(())
    }

    async fn list_worktrees(&self) -> Result<Vec<WorktreeInfo>, WorktreeError> {
        Ok(vec![])
    }

    async fn cleanup_stale(&self, _max_age: Duration) -> Result<Vec<PathBuf>, WorktreeError> {
        Ok(vec![])
    }

    fn is_supported(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod default_impl_tests {
    use super::*;

    /// `PosixWorktree` never overrides `enter_existing`, so it must fall
    /// through to the trait's default (`Err(WorktreeError::Unsupported)`) —
    /// the degradation policy mirrored from `worktree_change_summary`'s
    /// default `Ok(None)`. Proves existing impls that predate the method
    /// still compile and behave gracefully.
    #[tokio::test]
    async fn enter_existing_default_is_unsupported() {
        let mgr = PosixWorktree::new();
        let err = mgr
            .enter_existing(std::path::Path::new("/tmp/whatever"))
            .await
            .unwrap_err();
        assert!(matches!(err, WorktreeError::Unsupported));
    }
}
