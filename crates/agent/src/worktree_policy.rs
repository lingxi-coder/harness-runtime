//! Worktree creation policy with graceful degradation.
//!
//! [`create_worktree_or_degrade`] implements the matrix from spec §10.7:
//! `Required` agents fail when worktrees are not supported, `Optional`
//! agents try to create one and log a warning on failure, and `None`
//! agents skip worktrees entirely.

use crate::definition::WorktreeRequirement;
use lingxi_core::host::{WorktreeError, WorktreeHandle, WorktreeManager};

/// Try to create a worktree for the agent identified by `slug`, applying
/// the policy from `requirement`:
///
/// * `Required` + unsupported → [`WorktreeError::Unsupported`].
/// * `Required` + supported → propagate the manager's result.
/// * `Optional` + supported → create one; on error, log and return `Ok(None)`
///   so the caller can fall back to in-place execution.
/// * `Optional` + unsupported or `None` → `Ok(None)`.
pub async fn create_worktree_or_degrade(
    manager: &dyn WorktreeManager,
    requirement: WorktreeRequirement,
    slug: &str,
) -> Result<Option<WorktreeHandle>, WorktreeError> {
    match (requirement, manager.is_supported()) {
        (WorktreeRequirement::Required, false) => Err(WorktreeError::Unsupported),
        (WorktreeRequirement::Required, true) => {
            Ok(Some(manager.create_worktree(slug, None, &[]).await?))
        }
        (WorktreeRequirement::Optional, true) => {
            match manager.create_worktree(slug, None, &[]).await {
                Ok(h) => Ok(Some(h)),
                Err(e) => {
                    tracing::warn!("worktree degraded: {e}");
                    Ok(None)
                }
            }
        }
        (WorktreeRequirement::Optional, false) | (WorktreeRequirement::None, _) => Ok(None),
    }
}
