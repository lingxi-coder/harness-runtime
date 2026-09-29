//! Crash-safe JSONL reader (spec §22.4 / B5).
//!
//! On read, the function walks the file line-by-line. If a torn / corrupt
//! line is encountered, the file is truncated to the last known-good byte
//! offset and the recovered prefix is returned.

use crate::transcript::TranscriptEntry;
use lingxi_core::host::{FileSystem, FsError};
use thiserror::Error;

/// Failure modes for session storage operations.
#[derive(Debug, Clone, Error)]
pub enum StorageError {
    /// Underlying filesystem error.
    #[error(transparent)]
    Fs(#[from] FsError),
    /// File contained an unparseable record at the given byte offset.
    #[error("corrupted at byte {0}")]
    Corrupted(u64),
}

/// Outcome of a recovery read. `truncated_at` is the byte offset the file
/// now ends at — equal to the original length if no truncation occurred.
pub struct RecoveryResult {
    /// Successfully parsed entries (in file order).
    pub entries: Vec<TranscriptEntry>,
    /// Byte offset of the new file end after any truncation.
    pub truncated_at: u64,
}

/// Read `path` line-by-line; on the first parse failure, truncate the file
/// to the last-good byte offset and return the recovered prefix.
pub async fn read_recover(fs: &dyn FileSystem, path: &str) -> Result<RecoveryResult, StorageError> {
    let content = fs.read_file(path, None, None).await?.content;
    let mut entries = Vec::new();
    let mut last_valid_offset: u64 = 0;
    for line in content.lines() {
        if let Ok(e) = serde_json::from_str::<TranscriptEntry>(line) {
            entries.push(e);
            let line_bytes = u64::try_from(line.len()).unwrap_or(u64::MAX);
            last_valid_offset = last_valid_offset.saturating_add(line_bytes.saturating_add(1));
        } else {
            fs.truncate(path, last_valid_offset).await?;
            return Ok(RecoveryResult {
                entries,
                truncated_at: last_valid_offset,
            });
        }
    }
    Ok(RecoveryResult {
        entries,
        truncated_at: last_valid_offset,
    })
}
