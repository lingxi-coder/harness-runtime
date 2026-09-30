//! `BgSessionForker` — the injected seam behind the 2.1.212 `/fork` (`vAd`):
//! copy the CURRENT conversation into a NEW BACKGROUND session while the
//! interactive session stays live.
//!
//! The redefined `/fork` (`vAd = "Copy this conversation into a new background
//! session and keep working here"`) has NO `load` in the binary — upstream's
//! special dispatcher intercepts `name === "fork"` and routes it through the
//! background (`--bg`/daemon) session-copy path. This engine mirrors that split
//! with an injected seam: [`crate::host::OrchestratorHandle::fork_to_background_session`]
//! reads the live session's history + rendered system prompt and calls this
//! trait; the concrete impl lives in the CLI composition root (which owns the
//! `--bg`/daemon dispatch machinery and mints the new short id), so the leaf
//! `orchestrator` crate never depends on `apps/cli` (correct layering — mirrors
//! the existing `fork_spawner`/`fork_budget` optional-seam on the handle).
//!
//! The impl SNAPSHOTS the passed conversation into the new background session's
//! `<config-home>/projects/<sanitize(cwd)>/<new-uuid>.jsonl` transcript (the
//! same on-disk shape the resume loader reads), then dispatches a detached
//! daemon worker that RESUMES that copy. It returns the system line the LIVE
//! session displays — the composition root owns that text because it alone
//! knows the newly-minted background short id.

use async_trait::async_trait;
use std::sync::Arc;
use thiserror::Error;

/// Failure modes for [`BgSessionForker::fork_to_background`]. Projected to a
/// coarse `HandleError::ActionFailed` by the handle impl (the `/fork` handler
/// renders "Could not fork to background session: …").
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BgForkError {
    /// The conversation snapshot could not be written to the new session's
    /// transcript (I/O / serialization failure).
    #[error("could not snapshot conversation: {0}")]
    Snapshot(String),
    /// The background daemon dispatch (job/roster write or daemon ensure)
    /// failed.
    #[error("could not dispatch background session: {0}")]
    Dispatch(String),
}

/// Copies the current conversation into a new background session.
///
/// Injected into [`crate::host::OrchestratorHandle`] via
/// `ConversationOrchestrator::with_bg_session_forker`; `None` (tests /
/// non-desktop roots) ⇒ `fork_to_background_session` fails with a clear
/// `ActionFailed` rather than panicking.
#[async_trait]
pub trait BgSessionForker: Send + Sync {
    /// Late binding after the host has constructed its session registry.
    fn set_task_registry(
        &self,
        _registry: Arc<dyn crate::host::task_registry::TaskRegistryHandle>,
    ) {
    }

    /// Snapshot `history` (+ the parent's rendered `system_prompt`, when the
    /// live session has completed a turn) into a new background session and
    /// dispatch a detached worker that resumes it. `prompt` is the OPTIONAL
    /// `[prompt]` argument (empty string = no seed turn).
    ///
    /// `model` is the parent session's CURRENT active model: the snapshot
    /// stamps it onto the copied assistant lines so the resumed background
    /// session lands on the parent's model (and re-derives its provider profile
    /// by id) instead of the `DEFAULT_MODEL` seed — `ConversationMessage`s carry
    /// no per-line model, so the live session model is the only source.
    ///
    /// Returns the system line to display in the LIVE (interactive) session —
    /// the composition root owns the exact text since it mints the new short id.
    async fn fork_to_background(
        &self,
        history: &[crate::types::ConversationMessage],
        system_prompt: Option<Arc<str>>,
        prompt: &str,
        model: &str,
    ) -> Result<String, BgForkError>;

    /// Move a live conversation boundary into a background session.
    ///
    /// The default preserves source compatibility for non-CLI hosts, but only
    /// the production CLI override can durably carry composer/queue state into
    /// its PTY launch spec.
    async fn background_conversation(
        &self,
        history: &[crate::types::ConversationMessage],
        system_prompt: Option<Arc<str>>,
        prompt: &str,
        model: &str,
        snapshot: &crate::host::BackgroundingSnapshot,
    ) -> Result<String, BgForkError> {
        let _ = snapshot;
        self.fork_to_background(history, system_prompt, prompt, model)
            .await
    }

    /// Resume an EXISTING on-disk session as a NEW background session — the
    /// 2.1.212 `/resume`-as-background behavior (G06). Unlike
    /// [`Self::fork_to_background`], this does NOT snapshot a live conversation:
    /// `session_id` already exists at its standard transcript path, so the impl
    /// dispatches a detached daemon that RESUMES it directly (reusing the same
    /// `Launch::Resume` dispatch, minus the snapshot write). Returns the system
    /// line to display — again owned by the composition root (it mints the new
    /// short id).
    async fn resume_to_background(&self, session_id: &str) -> Result<String, BgForkError>;
}
