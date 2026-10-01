//! The seam between the service and whatever keeps an app's source history.
//!
//! The service decides when a checkpoint is wanted and under which locks; a
//! backend decides how it is stored. The Git implementation is
//! [`crate::checkpoints`], compiled with the `git-checkpoints` feature,
//! because libgit2 links native code and one dependency graph may link it
//! once: whoever builds the final binary chooses where git2 comes from, or
//! brings a backend of its own.

use crate::error::AppError;
use crate::manifest::AppLayout;
use crate::types::{AppCheckpoint, AppCheckpointKind};
use std::sync::Arc;

/// Keeps retained snapshots of one app's source workspace.
///
/// Calls block on the filesystem. The service runs them on its blocking pool
/// and holds the app's build lock around [`create`](Self::create) and
/// [`restore`](Self::restore), so a backend does not lock against builds.
pub trait CheckpointBackend: Send + Sync {
    /// Retained checkpoints of the app, newest first. An app with no history
    /// yields an empty list, not an error.
    fn list(&self, layout: &AppLayout) -> Result<Vec<AppCheckpoint>, AppError>;

    /// Record the workspace's exact current contents as a new checkpoint.
    fn create(
        &self,
        layout: &AppLayout,
        kind: AppCheckpointKind,
        label: &str,
        created_at_ms: u64,
    ) -> Result<AppCheckpoint, AppError>;

    /// Move the source workspace back to `checkpoint_id` and return the
    /// `pre_restore` checkpoint taken first. Data, runtime and build paths are
    /// never part of the source workspace and are not touched.
    fn restore(
        &self,
        layout: &AppLayout,
        checkpoint_id: &str,
        created_at_ms: u64,
    ) -> Result<AppCheckpoint, AppError>;
}

/// The backend of a build that has none: no history, and a plain refusal to
/// start one. Callers already handle the "version control is not available"
/// answer, since an app can opt out of it.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoCheckpoints;

impl NoCheckpoints {
    fn unavailable() -> AppError {
        AppError::NotYetAvailable("no checkpoint backend is configured".into())
    }
}

impl CheckpointBackend for NoCheckpoints {
    fn list(&self, _layout: &AppLayout) -> Result<Vec<AppCheckpoint>, AppError> {
        Ok(Vec::new())
    }

    fn create(
        &self,
        _layout: &AppLayout,
        _kind: AppCheckpointKind,
        _label: &str,
        _created_at_ms: u64,
    ) -> Result<AppCheckpoint, AppError> {
        Err(Self::unavailable())
    }

    fn restore(
        &self,
        _layout: &AppLayout,
        _checkpoint_id: &str,
        _created_at_ms: u64,
    ) -> Result<AppCheckpoint, AppError> {
        Err(Self::unavailable())
    }
}

/// Checkpoints kept as commits in the workspace's own Git repository.
#[cfg(feature = "git-checkpoints")]
#[derive(Debug, Clone, Copy, Default)]
pub struct GitCheckpoints;

#[cfg(feature = "git-checkpoints")]
impl CheckpointBackend for GitCheckpoints {
    fn list(&self, layout: &AppLayout) -> Result<Vec<AppCheckpoint>, AppError> {
        crate::checkpoints::AppCheckpointStore::new(layout).list()
    }

    fn create(
        &self,
        layout: &AppLayout,
        kind: AppCheckpointKind,
        label: &str,
        created_at_ms: u64,
    ) -> Result<AppCheckpoint, AppError> {
        crate::checkpoints::AppCheckpointStore::new(layout).create(kind, label, created_at_ms)
    }

    fn restore(
        &self,
        layout: &AppLayout,
        checkpoint_id: &str,
        created_at_ms: u64,
    ) -> Result<AppCheckpoint, AppError> {
        crate::checkpoints::AppCheckpointStore::new(layout).restore(checkpoint_id, created_at_ms)
    }
}

/// The backend a service starts with: Git when this build has it, none
/// otherwise. [`AppService::with_checkpoint_backend`] replaces it.
///
/// [`AppService::with_checkpoint_backend`]: crate::service::AppService::with_checkpoint_backend
pub(crate) fn default_backend() -> Arc<dyn CheckpointBackend> {
    #[cfg(feature = "git-checkpoints")]
    {
        Arc::new(GitCheckpoints)
    }
    #[cfg(not(feature = "git-checkpoints"))]
    {
        Arc::new(NoCheckpoints)
    }
}
