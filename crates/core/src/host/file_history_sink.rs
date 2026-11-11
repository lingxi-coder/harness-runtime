//! `FileHistorySink` — the write tools' hook into `/rewind`'s file-history.
//!
//! The `Edit`/`Write`/`NotebookEdit` tools call [`FileHistorySink::track_edit`]
//! BEFORE they write, so `/rewind` can restore each file's pre-edit content. The
//! real implementation is `session::FileHistory`, but `tool-api` cannot depend
//! on `session`, so the tools see it through this object-safe trait riding on
//! `tool_api::ToolUseContext`.

use async_trait::async_trait;

/// Pre-write backup hook for `/rewind` file checkpointing (claude-code
/// `fileHistoryTrackEdit`).
#[async_trait]
pub trait FileHistorySink: Send + Sync {
    /// Back up `file_path`'s CURRENT (pre-edit) content into the active turn's
    /// snapshot, if not already tracked. Best-effort — a no-op when no turn
    /// snapshot is open or checkpointing is unavailable.
    async fn track_edit(&self, file_path: &str);

    /// Bind a previously missing parent to the directory that actually
    /// performed a successful rooted write. A changed parent remains unbound.
    /// This records an in-memory handle receipt inside the owned synchronous
    /// commit; implementations must not wait for asynchronous work.
    fn track_completed_write(&self, file_path: &str, parent: super::rooted_fs::RootedWriteParent);
}
