//! Host-only identities for session JSONL rows.
//!
//! The Native transcript remains untouched. This sidecar records the actual
//! outer UUID of each row and the monotonically allocated stream index used by
//! the host session-agent protocol.

use lingxi_core::host::{FsError, rooted_fs::{open_read_file_pinned, RootIdentity}};
use std::{collections::HashMap, io::{BufRead, BufReader, Read}, path::{Path, PathBuf}};

pub(crate) fn read_snapshot_at(
    transcript_path: &Path,
    root: &Path,
    identity: &RootIdentity,
) -> Result<SessionMessageIdentitySnapshot, FsError> {
    let sidecar_relative = log_path(transcript_path)?;
    let mut sidecar = match open_read_file_pinned(root, &sidecar_relative, Some(identity)) {
        Ok(file) => file,
        Err(FsError::NotFound(_)) => return Ok(SessionMessageIdentitySnapshot::default()),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    sidecar
        .read_to_end(&mut bytes)
        .map_err(|error| FsError::Io(error.to_string()))?;
    let mut cache = IdentityLogCache::default();
    apply_entries(&mut cache, &bytes, true)?;

    // A crash may happen after a durable tombstone replacement but before its
    // sidecar Remove event. Intersect with current outer UUIDs so the reader
    // never advertises a row that is no longer present in Native JSONL.
    let transcript_relative = transcript_path
        .file_name()
        .map(PathBuf::from)
        .ok_or_else(|| FsError::Io("transcript has no filename".into()))?;
    let transcript = match open_read_file_pinned(root, &transcript_relative, Some(identity)) {
        Ok(file) => file,
        Err(FsError::NotFound(_)) => {
            return Ok(SessionMessageIdentitySnapshot {
                by_uuid: HashMap::new(),
                next_message_index: cache.snapshot.next_message_index,
            })
        }
        Err(error) => return Err(error),
    };
    let mut present = std::collections::HashSet::new();
    for line in BufReader::new(transcript).lines() {
        let line = line.map_err(|error| FsError::Io(error.to_string()))?;
        if let Ok(exact) = crate::jsonl::exact_json::parse_exact_json(&line) {
            if let Some(uuid) = exact.value.get("uuid").and_then(serde_json::Value::as_str) {
                present.insert(uuid.to_string());
            }
        }
    }
    cache
        .snapshot
        .by_uuid
        .retain(|uuid, _| present.contains(uuid));
    Ok(cache.snapshot)
}
