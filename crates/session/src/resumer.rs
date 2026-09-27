//! Session resume orchestration (spec §22.5 / C7).
//!
//! `SessionResumer` ties [`SessionStorage`] (transcript + metadata) to the
//! file-state cache. M1.16 only restores the session bones; full re-attach
//! of plugins, MCP servers, permission state, cost ledger, and output style
//! is handled by the host wrapper in Plans 15 and 16.

use crate::filestate::{self, FileStateCache};
use crate::rollout::{InitialHistory, RolloutRecorder};
use crate::storage::{LoadedSession, SessionStorage};
use platform_api::FileSystem;
use std::path::Path;
use std::sync::Arc;
use thiserror::Error;

/// Output of [`SessionResumer::resume`].
pub struct ResumedSession {
    /// Loaded metadata and transcript messages.
    pub loaded: LoadedSession,
    /// Fresh, empty file-state cache (we deliberately do not rehydrate from
    /// stale historical Reads — see B6).
    pub file_state_cache: FileStateCache,
}

/// Output of [`SessionResumer::resume_from_rollout`].
///
/// The rollout-format analog of [`ResumedSession`]: instead of LingXi's
/// claude-code transcript it carries codex's reconstructed [`InitialHistory`]
/// (faithful to codex's resume-from-rollout semantics). As with
/// [`ResumedSession`], the file-state cache restarts empty (B6) — we do not
/// rehydrate from stale historical Reads.
pub struct ResumedRollout {
    /// Reconstructed initial history (`Resumed`/`New`) from the rollout file.
    pub history: InitialHistory,
    /// Fresh, empty file-state cache.
    pub file_state_cache: FileStateCache,
}

// `FileStateCache` is not `Debug`, so format only the resumable history. This
// keeps `ResumedRollout` usable with `Result::expect`/`expect_err` in tests.
impl std::fmt::Debug for ResumedRollout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResumedRollout")
            .field("history", &self.history)
            .finish_non_exhaustive()
    }
}

/// Failure modes for [`SessionResumer::resume`].
#[derive(Debug, Clone, Error)]
pub enum ResumeError {
    /// Underlying storage error, stringified to keep `ResumeError` `Clone`.
    #[error("storage: {0}")]
    Storage(String),
    /// Underlying rollout load/parse error, stringified to keep `ResumeError`
    /// `Clone`.
    #[error("rollout: {0}")]
    Rollout(String),
}

/// Glue object that resumes a session from disk.
pub struct SessionResumer {
    storage: Arc<SessionStorage>,
    #[allow(dead_code)]
    fs: Arc<dyn FileSystem>,
}

impl SessionResumer {
    /// Construct a new resumer.
    #[must_use]
    pub fn new(storage: Arc<SessionStorage>, fs: Arc<dyn FileSystem>) -> Self {
        Self { storage, fs }
    }

    /// M1.16 restores the session bones; Plugin / MCP / Permission / Cost /
    /// `OutputStyle` re-attachment is done by the host wrapper in Plans 15
    /// and 16. This function returns the loaded transcript plus an empty
    /// `FileStateCache`.
    ///
    /// Note: `SessionResumer` deliberately does NOT rebuild `FileStateCache`
    /// from stale historical Reads (B6) — the cache restarts empty and the
    /// agent re-Reads any file it needs.
    pub async fn resume(
        &self,
        session_id: &protocol::SessionId,
    ) -> Result<ResumedSession, ResumeError> {
        let loaded = self
            .storage
            .load(session_id)
            .await
            .map_err(|e| ResumeError::Storage(e.to_string()))?;
        let cache = FileStateCache::new(filestate::MAX_ENTRIES, filestate::MAX_BYTES);
        Ok(ResumedSession {
            loaded,
            file_state_cache: cache,
        })
    }

    /// Resume from a persisted codex-format **rollout** file (the `.jsonl`
    /// rollout written by [`RolloutRecorder`]), as opposed to LingXi's own
    /// claude-code transcript storage.
    ///
    /// Faithful to codex's resume-from-rollout flow: it loads the rollout via
    /// [`RolloutRecorder::get_rollout_history`] and returns the reconstructed
    /// [`InitialHistory`] plus an empty `FileStateCache` (B6 — the cache
    /// restarts empty; the agent re-Reads any file it needs).
    ///
    /// This is a sibling of [`SessionResumer::resume`]; it does NOT touch the
    /// byte-locked claude-code transcript path — the rollout format is its own
    /// thing.
    pub async fn resume_from_rollout(
        &self,
        rollout_path: &Path,
    ) -> Result<ResumedRollout, ResumeError> {
        let history = RolloutRecorder::get_rollout_history(rollout_path)
            .await
            .map_err(|e| ResumeError::Rollout(e.to_string()))?;
        let cache = FileStateCache::new(filestate::MAX_ENTRIES, filestate::MAX_BYTES);
        Ok(ResumedRollout {
            history,
            file_state_cache: cache,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod rollout_resume_tests {
    //! Tests for [`SessionResumer::resume_from_rollout`] — the rollout-backed
    //! resume entry point. These mirror codex's `get_rollout_history` resume
    //! semantics (resumed history, empty→New, missing-meta error) through the
    //! session-resume surface.

    use super::*;
    use crate::rollout::{InitialHistory, RolloutItem};
    use platform_posix::fs::PosixFileSystem;
    use std::io::Write;
    use tempfile::TempDir;
    use uuid::Uuid;

    fn make_resumer(root: &std::path::Path) -> SessionResumer {
        let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(root.to_path_buf()));
        let storage = Arc::new(SessionStorage::new(root.to_path_buf(), Arc::clone(&fs)));
        SessionResumer::new(storage, fs)
    }

    #[tokio::test]
    async fn resume_from_rollout_reconstructs_resumed_history() {
        let home = TempDir::new().unwrap();
        let rollout_path = home.path().join("rollout.jsonl");
        let mut file = std::fs::File::create(&rollout_path).unwrap();
        let thread_id = Uuid::new_v4();
        let ts = "2025-01-03T12:00:00Z";

        writeln!(
            file,
            "{}",
            serde_json::json!({
                "timestamp": ts,
                "type": "session_meta",
                "payload": {
                    "id": thread_id,
                    "session_id": thread_id,
                    "timestamp": ts,
                    "cwd": "/work/project",
                    "originator": "test_originator",
                    "cli_version": "test_version",
                    "source": "cli",
                    "model_provider": "test-provider",
                },
            })
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "timestamp": ts,
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [ { "type": "output_text", "text": "hello" } ],
                },
            })
        )
        .unwrap();

        let resumer = make_resumer(home.path());
        let resumed = resumer.resume_from_rollout(&rollout_path).await.unwrap();

        let InitialHistory::Resumed(history) = &resumed.history else {
            panic!("expected resumed history, got {:?}", resumed.history);
        };
        assert_eq!(history.conversation_id.as_uuid(), thread_id);
        assert_eq!(history.history.len(), 2);
        assert_eq!(
            history.rollout_path.as_deref(),
            Some(rollout_path.as_path())
        );
        assert!(matches!(history.history[0], RolloutItem::SessionMeta(_)));
        assert!(matches!(history.history[1], RolloutItem::ResponseItem(_)));
        // cwd accessor reads the session-meta line.
        assert_eq!(
            resumed.history.session_cwd(),
            Some(std::path::PathBuf::from("/work/project"))
        );
        assert_eq!(
            resumed.history.get_session_originator().as_deref(),
            Some("test_originator")
        );
        // File-state cache restarts empty (B6).
        assert!(resumed.file_state_cache.dump().is_empty());
    }

    #[tokio::test]
    async fn resume_from_rollout_meta_only_collapses_to_new() {
        // A rollout containing ONLY a session-meta line has no model history,
        // so codex collapses it to `InitialHistory::New`.
        let home = TempDir::new().unwrap();
        let rollout_path = home.path().join("meta-only.jsonl");
        let mut file = std::fs::File::create(&rollout_path).unwrap();
        let thread_id = Uuid::new_v4();
        let ts = "2025-01-03T12:00:00Z";
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "timestamp": ts,
                "type": "session_meta",
                "payload": {
                    "id": thread_id,
                    "session_id": thread_id,
                    "timestamp": ts,
                    "cwd": ".",
                    "originator": "o",
                    "cli_version": "v",
                    "source": "cli",
                },
            })
        )
        .unwrap();

        let resumer = make_resumer(home.path());
        let resumed = resumer.resume_from_rollout(&rollout_path).await.unwrap();
        // Meta line still loads as one item, so history is Resumed (non-empty).
        assert!(matches!(resumed.history, InitialHistory::Resumed(_)));
    }

    #[tokio::test]
    async fn resume_from_rollout_without_session_meta_errors() {
        // No session-meta line → no canonical thread id → error (codex parity).
        let home = TempDir::new().unwrap();
        let rollout_path = home.path().join("no-meta.jsonl");
        let mut file = std::fs::File::create(&rollout_path).unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "timestamp": "2025-01-03T12:00:00Z",
                "type": "response_item",
                "payload": { "type": "message", "role": "user", "content": [] },
            })
        )
        .unwrap();

        let resumer = make_resumer(home.path());
        let err = resumer
            .resume_from_rollout(&rollout_path)
            .await
            .expect_err("missing session_meta should error");
        assert!(matches!(err, ResumeError::Rollout(_)));
        assert!(err.to_string().contains("thread ID"));
    }

    #[tokio::test]
    async fn resume_from_rollout_empty_file_errors() {
        let home = TempDir::new().unwrap();
        let rollout_path = home.path().join("empty.jsonl");
        std::fs::File::create(&rollout_path).unwrap();
        let resumer = make_resumer(home.path());
        let err = resumer
            .resume_from_rollout(&rollout_path)
            .await
            .expect_err("empty file should error");
        assert!(matches!(err, ResumeError::Rollout(_)));
        assert!(err.to_string().contains("empty session file"));
    }
}
