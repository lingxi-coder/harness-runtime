//! Append + fsync + flock storage layer for transcripts and metadata
//! (spec §22.3 / B5).

use crate::jsonl::{read_recover, StorageError};
use crate::metadata::SessionMetadata;
use crate::transcript::TranscriptEntry;
use lingxi_core::host::FileSystem;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Owns the on-disk layout for one base directory of sessions.
pub struct SessionStorage {
    base_dir: PathBuf,
    fs: Arc<dyn FileSystem>,
    write_lock: Mutex<()>,
}

/// What [`SessionStorage::load`] returns: the metadata plus the recovered
/// conversation messages (other transcript variants are dropped here; the
/// resumer handles those separately).
#[derive(Debug, Clone)]
pub struct LoadedSession {
    /// Persisted session metadata.
    pub metadata: SessionMetadata,
    /// Recovered conversation messages in arrival order.
    pub messages: Vec<lingxi_core::types::ConversationMessage>,
}

impl SessionStorage {
    /// Construct a storage layer rooted at `base_dir`.
    #[must_use]
    pub fn new(base_dir: PathBuf, fs: Arc<dyn FileSystem>) -> Self {
        Self {
            base_dir,
            fs,
            write_lock: Mutex::new(()),
        }
    }

    /// Directory holding `metadata.json` and `transcript.jsonl` for one session.
    #[must_use]
    pub fn session_dir(&self, session_id: &lingxi_core::types::SessionId) -> PathBuf {
        self.base_dir.join(session_id.as_uuid().to_string())
    }

    /// Full path to a session's transcript.
    #[must_use]
    pub fn transcript_path(&self, session_id: &lingxi_core::types::SessionId) -> PathBuf {
        self.session_dir(session_id).join("transcript.jsonl")
    }

    /// Append one entry to the transcript with flock + fsync.
    pub async fn append(
        &self,
        session_id: &lingxi_core::types::SessionId,
        entry: TranscriptEntry,
    ) -> Result<(), StorageError> {
        let _guard = self.write_lock.lock().await;
        let path = self.transcript_path(session_id);
        let line = format!("{}\n", serde_json::to_string(&entry).unwrap());
        let path_str = path.to_str().expect("utf-8 path");
        let _flock = self.fs.flock_exclusive(path_str).await?;
        self.fs.append_file(path_str, &line).await?;
        self.fs.fsync(path_str).await?;
        Ok(())
    }

    /// Save metadata.json atomically (write + fsync) under an exclusive flock.
    pub async fn save_metadata(&self, metadata: &SessionMetadata) -> Result<(), StorageError> {
        let path = self.session_dir(&metadata.session_id).join("metadata.json");
        let path_str = path.to_str().expect("utf-8 path");
        let _flock = self.fs.flock_exclusive(path_str).await?;
        self.fs
            .write_file(path_str, &serde_json::to_string_pretty(metadata).unwrap())
            .await?;
        self.fs.fsync(path_str).await?;
        Ok(())
    }

    /// Load metadata + recover transcript messages.
    pub async fn load(
        &self,
        session_id: &lingxi_core::types::SessionId,
    ) -> Result<LoadedSession, StorageError> {
        let meta_path = self.session_dir(session_id).join("metadata.json");
        let meta_str = self
            .fs
            .read_file(meta_path.to_str().expect("utf-8 path"), None, None)
            .await?
            .content;
        let metadata: SessionMetadata =
            serde_json::from_str(&meta_str).map_err(|_| StorageError::Corrupted(0))?;

        let transcript_path = self.transcript_path(session_id);
        let recovery =
            read_recover(&*self.fs, transcript_path.to_str().expect("utf-8 path")).await?;
        let messages: Vec<_> = recovery
            .entries
            .into_iter()
            .filter_map(|e| {
                if let TranscriptEntry::Message { message, .. } = e {
                    Some(message)
                } else {
                    None
                }
            })
            .collect();
        Ok(LoadedSession { metadata, messages })
    }
}
