//! Host-only identities for session JSONL rows.
//!
//! The Native transcript remains untouched. This sidecar records the actual
//! outer UUID of each row and the monotonically allocated stream index used by
//! the host session-agent protocol.

use lingxi_core::host::rooted_fs::{
    atomic_write_pinned, open_append_file_pinned, open_read_file_pinned, sync_parent_pinned,
    truncate_file_pinned, AtomicWriteOptions, RootIdentity,
};
use lingxi_core::host::FsError;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const MAX_IDENTITY_RECORD_BYTES: usize = 64 * 1024;

/// Durable row-identity facts for one transcript.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionMessageIdentitySnapshot {
    /// Current transcript outer UUID to its original stream index.
    pub by_uuid: HashMap<String, u64>,
    /// The next index to allocate. Removing a row never reduces this value.
    pub next_message_index: u64,
}

/// The operation log is append-only so adding one row does not rewrite an
/// ever-growing UUID map. Removal entries only affect the current map; prior
/// append entries retain the high-water mark across restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum IdentityLogEntry {
    Bootstrap {
        next_message_index: u64,
    },
    Append {
        uuid: String,
        message_index: u64,
    },
    Remove {
        uuid: String,
        next_message_index: u64,
    },
}

/// In-process cursor and current map for a transcript identity log.
#[derive(Debug, Clone, Default)]
pub(crate) struct IdentityLogCache {
    pub(crate) log_bytes: u64,
    pub(crate) snapshot: SessionMessageIdentitySnapshot,
}

/// Shared, per-writer identity cache. Durable transactions remain the
/// cross-process serialization boundary; this cache avoids replaying the
/// append-only log when one writer handles a sequence of rows.
pub(crate) type IdentityLogStore = Arc<Mutex<HashMap<PathBuf, IdentityLogCache>>>;

pub(crate) fn log_path(transcript_path: &Path) -> Result<PathBuf, FsError> {
    let name = transcript_path
        .file_name()
        .ok_or_else(|| FsError::Io("transcript has no filename".into()))?;
    let mut sidecar = name.to_os_string();
    sidecar.push(branding::SESSION_MESSAGE_IDENTITY_LOG_SUFFIX);
    Ok(PathBuf::from(sidecar))
}

pub(crate) fn lock_path(transcript_path: &Path) -> Result<PathBuf, FsError> {
    let name = transcript_path
        .file_name()
        .ok_or_else(|| FsError::Io("transcript has no filename".into()))?;
    let mut sidecar = name.to_os_string();
    sidecar.push(branding::SESSION_MESSAGE_IDENTITY_LOCK_SUFFIX);
    Ok(PathBuf::from(sidecar))
}

pub(crate) fn encode_entry(entry: &IdentityLogEntry) -> Result<Vec<u8>, FsError> {
    let mut bytes = serde_json::to_vec(entry).map_err(|error| FsError::Io(error.to_string()))?;
    bytes.push(b'\n');
    if bytes.len() > MAX_IDENTITY_RECORD_BYTES {
        return Err(FsError::Io(
            "session identity record exceeds its bound".into(),
        ));
    }
    Ok(bytes)
}

pub(crate) fn apply_entries(
    cache: &mut IdentityLogCache,
    bytes: &[u8],
    tolerate_torn_tail: bool,
) -> Result<(), FsError> {
    let complete_len = match bytes.iter().rposition(|byte| *byte == b'\n') {
        Some(index) => index + 1,
        None if bytes.is_empty() => 0,
        None if tolerate_torn_tail => 0,
        None => {
            return Err(FsError::Io(
                "session identity sidecar has no complete record".into(),
            ))
        }
    };
    let mut start = 0usize;
    for (index, byte) in bytes[..complete_len].iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        let line = &bytes[start..index];
        let entry: IdentityLogEntry = serde_json::from_slice(line)
            .map_err(|error| FsError::Io(format!("invalid session identity record: {error}")))?;
        match entry {
            IdentityLogEntry::Bootstrap { next_message_index } => {
                cache.snapshot.next_message_index =
                    cache.snapshot.next_message_index.max(next_message_index);
            }
            IdentityLogEntry::Append {
                uuid,
                message_index,
            } => {
                if uuid.is_empty() {
                    return Err(FsError::Io("session identity record omitted UUID".into()));
                }
                // Repeated physical JSONL writes for one message keep the
                // original stream index while that UUID remains active. A
                // Remove entry deletes the mapping; a later Append for the
                // same UUID then establishes a fresh identity.
                if !cache.snapshot.by_uuid.contains_key(&uuid) {
                    cache.snapshot.next_message_index = cache.snapshot.next_message_index.max(
                        message_index
                            .checked_add(1)
                            .ok_or_else(|| FsError::Io("session message index exhausted".into()))?,
                    );
                    cache.snapshot.by_uuid.insert(uuid, message_index);
                }
            }
            IdentityLogEntry::Remove {
                uuid,
                next_message_index,
            } => {
                cache.snapshot.by_uuid.remove(&uuid);
                cache.snapshot.next_message_index =
                    cache.snapshot.next_message_index.max(next_message_index);
            }
        }
        start = index + 1;
    }
    cache.log_bytes = cache
        .log_bytes
        .checked_add(complete_len as u64)
        .ok_or_else(|| FsError::Io("session identity sidecar size overflow".into()))?;
    Ok(())
}

fn cache_lock_error() -> FsError {
    FsError::Io("session identity cache lock is poisoned".into())
}

fn refresh_cache_at(
    store: &IdentityLogStore,
    transcript_path: &Path,
    root: &Path,
    identity: &RootIdentity,
    sidecar_relative: &Path,
) -> Result<(), FsError> {
    let mut stores = store.lock().map_err(|_| cache_lock_error())?;
    let cache = stores.entry(transcript_path.to_path_buf()).or_default();
    let mut file = match open_read_file_pinned(root, sidecar_relative, Some(identity)) {
        Ok(file) => file,
        Err(FsError::NotFound(_)) => {
            *cache = IdentityLogCache::default();
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let file_len = file
        .metadata()
        .map_err(|error| FsError::Io(error.to_string()))?
        .len();
    if file_len < cache.log_bytes {
        return Err(FsError::Io(
            "session identity sidecar shrank after this writer observed it".into(),
        ));
    }
    if file_len == cache.log_bytes {
        return Ok(());
    }
    file.seek(SeekFrom::Start(cache.log_bytes))
        .map_err(|error| FsError::Io(error.to_string()))?;
    let mut delta = Vec::new();
    file.read_to_end(&mut delta)
        .map_err(|error| FsError::Io(error.to_string()))?;
    apply_entries(cache, &delta, true)
}

fn append_entry_at(
    store: &IdentityLogStore,
    transcript_path: &Path,
    root: &Path,
    identity: &RootIdentity,
    sidecar_relative: &Path,
    entry: &IdentityLogEntry,
) -> Result<(), FsError> {
    refresh_cache_at(store, transcript_path, root, identity, sidecar_relative)?;
    let encoded = encode_entry(entry)?;
    let mut stores = store.lock().map_err(|_| cache_lock_error())?;
    let cache = stores.entry(transcript_path.to_path_buf()).or_default();
    let (existed, actual_len) = match open_read_file_pinned(root, sidecar_relative, Some(identity))
    {
        Ok(file) => (
            true,
            file.metadata()
                .map_err(|error| FsError::Io(error.to_string()))?
                .len(),
        ),
        Err(FsError::NotFound(_)) => (false, 0),
        Err(error) => return Err(error),
    };
    if actual_len != cache.log_bytes {
        // Ignore only an incomplete tail. Complete external entries were
        // consumed by refresh_cache_at above; any unexplained suffix is unsafe.
        if actual_len < cache.log_bytes {
            return Err(FsError::Io(
                "session identity sidecar changed while locked".into(),
            ));
        }
        truncate_file_pinned(root, sidecar_relative, cache.log_bytes, Some(identity))?;
    }
    let mut file = open_append_file_pinned(root, sidecar_relative, Some(identity))?;
    file.write_all(&encoded)
        .map_err(|error| FsError::Io(error.to_string()))?;
    file.sync_all()
        .map_err(|error| FsError::Io(error.to_string()))?;
    if !existed || actual_len == 0 {
        sync_parent_pinned(root, sidecar_relative, Some(identity))?;
    }
    apply_entries(cache, &encoded, false)
}

/// Reserve and persist a row identity before the corresponding JSONL append.
/// A repeated active UUID reuses its original index; if the transcript append
/// fails, a newly reserved index remains burned. Readers intersect the sidecar
/// with current JSONL UUIDs and do not expose an unpersisted row.
pub(crate) fn append_row_identity_at(
    store: &IdentityLogStore,
    transcript_path: &Path,
    root: &Path,
    identity: &RootIdentity,
    uuid: &str,
) -> Result<u64, FsError> {
    if uuid.is_empty() {
        return Err(FsError::Io("session message UUID must not be empty".into()));
    }
    let sidecar_relative = log_path(transcript_path)?;
    if matches!(
        open_read_file_pinned(root, &sidecar_relative, Some(identity)),
        Err(FsError::NotFound(_))
    ) {
        let transcript_relative = transcript_path
            .file_name()
            .map(PathBuf::from)
            .ok_or_else(|| FsError::Io("transcript has no filename".into()))?;
        match open_read_file_pinned(root, &transcript_relative, Some(identity)) {
            Ok(transcript)
                if transcript
                    .metadata()
                    .map_err(|error| FsError::Io(error.to_string()))?
                    .len()
                    > 0 =>
            {
                return Err(FsError::Io(
                    "nonempty Native transcript requires explicit Host identity bootstrap before append".into(),
                ));
            }
            Ok(_) | Err(FsError::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
    }
    let snapshot = {
        refresh_cache_at(store, transcript_path, root, identity, &sidecar_relative)?;
        let stores = store.lock().map_err(|_| cache_lock_error())?;
        stores
            .get(transcript_path)
            .cloned()
            .unwrap_or_default()
            .snapshot
    };
    let index = snapshot.next_message_index;
    if let Some(existing_index) = snapshot.by_uuid.get(uuid) {
        return Ok(*existing_index);
    }
    append_entry_at(
        store,
        transcript_path,
        root,
        identity,
        &sidecar_relative,
        &IdentityLogEntry::Append {
            uuid: uuid.to_string(),
            message_index: index,
        },
    )?;
    Ok(index)
}

pub(crate) fn remove_row_identity_at(
    store: &IdentityLogStore,
    transcript_path: &Path,
    root: &Path,
    identity: &RootIdentity,
    uuid: &str,
) -> Result<(), FsError> {
    let sidecar_relative = log_path(transcript_path)?;
    refresh_cache_at(store, transcript_path, root, identity, &sidecar_relative)?;
    let next_message_index = store
        .lock()
        .map_err(|_| cache_lock_error())?
        .get(transcript_path)
        .cloned()
        .unwrap_or_default()
        .snapshot
        .next_message_index;
    append_entry_at(
        store,
        transcript_path,
        root,
        identity,
        &sidecar_relative,
        &IdentityLogEntry::Remove {
            uuid: uuid.to_string(),
            next_message_index,
        },
    )
}

pub(crate) fn require_initialized_at(
    transcript_path: &Path,
    root: &Path,
    identity: &RootIdentity,
) -> Result<(), FsError> {
    let sidecar_relative = log_path(transcript_path)?;
    match open_read_file_pinned(root, &sidecar_relative, Some(identity)) {
        Ok(_) => Ok(()),
        Err(FsError::NotFound(_)) => {
            let transcript_relative = transcript_path
                .file_name()
                .map(PathBuf::from)
                .ok_or_else(|| FsError::Io("transcript has no filename".into()))?;
            match open_read_file_pinned(root, &transcript_relative, Some(identity)) {
                Ok(file)
                    if file
                        .metadata()
                        .map_err(|error| FsError::Io(error.to_string()))?
                        .len()
                        > 0 =>
                {
                    Err(FsError::Io(
                        "nonempty Native transcript requires explicit Host identity bootstrap before mutation".into(),
                    ))
                }
                Ok(_) | Err(FsError::NotFound(_)) => Ok(()),
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

/// Initialize persistent identity facts for a Native transcript imported by
/// this Host. Existing sidecar state is left untouched; missing historical
/// indices caused by prior Native deletions cannot be recovered here.
pub(crate) fn bootstrap_at(
    store: &IdentityLogStore,
    transcript_path: &Path,
    root: &Path,
    identity: &RootIdentity,
) -> Result<SessionMessageIdentitySnapshot, FsError> {
    let sidecar_relative = log_path(transcript_path)?;
    match open_read_file_pinned(root, &sidecar_relative, Some(identity)) {
        Ok(mut file) => {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)
                .map_err(|error| FsError::Io(error.to_string()))?;
            let mut cache = IdentityLogCache::default();
            apply_entries(&mut cache, &bytes, true)?;
            store
                .lock()
                .map_err(|_| cache_lock_error())?
                .insert(transcript_path.to_path_buf(), cache.clone());
            return read_snapshot_at(transcript_path, root, identity);
        }
        Err(FsError::NotFound(_)) => {}
        Err(error) => return Err(error),
    }

    let transcript_relative = transcript_path
        .file_name()
        .map(PathBuf::from)
        .ok_or_else(|| FsError::Io("transcript has no filename".into()))?;
    let transcript = match open_read_file_pinned(root, &transcript_relative, Some(identity)) {
        Ok(file) => Some(file),
        Err(FsError::NotFound(_)) => None,
        Err(error) => return Err(error),
    };
    let mut entries = Vec::new();
    let mut next_message_index = 0u64;
    let mut seen_uuids = HashSet::new();
    if let Some(file) = transcript {
        for line in BufReader::new(file).lines() {
            let line = line.map_err(|error| FsError::Io(error.to_string()))?;
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if let Some(uuid) = value.get("uuid").and_then(serde_json::Value::as_str) {
                if !uuid.is_empty() && seen_uuids.insert(uuid.to_owned()) {
                    entries.push(IdentityLogEntry::Append {
                        uuid: uuid.to_string(),
                        message_index: next_message_index,
                    });
                    next_message_index = next_message_index
                        .checked_add(1)
                        .ok_or_else(|| FsError::Io("session message index exhausted".into()))?;
                }
            }
        }
    }
    entries.insert(0, IdentityLogEntry::Bootstrap { next_message_index });
    let mut bytes = Vec::new();
    let mut cache = IdentityLogCache::default();
    for entry in &entries {
        let encoded = encode_entry(entry)?;
        apply_entries(&mut cache, &encoded, false)?;
        bytes.extend_from_slice(&encoded);
    }
    atomic_write_pinned(
        root,
        &sidecar_relative,
        &bytes,
        AtomicWriteOptions {
            overwrite: false,
            create_parents: false,
            dir_mode: 0o700,
            file_mode: 0o600,
        },
        identity,
    )?;
    store
        .lock()
        .map_err(|_| cache_lock_error())?
        .insert(transcript_path.to_path_buf(), cache.clone());
    Ok(cache.snapshot)
}

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

#[cfg(test)]
mod tests {
    use super::{apply_entries, encode_entry, IdentityLogCache, IdentityLogEntry};

    #[test]
    fn active_uuid_updates_keep_original_index_until_remove_then_reallocate() {
        let repeated_updates = [
            IdentityLogEntry::Append {
                uuid: "same-row".into(),
                message_index: 0,
            },
            IdentityLogEntry::Append {
                uuid: "same-row".into(),
                message_index: 1,
            },
        ];
        let mut cache = IdentityLogCache::default();
        let mut bytes = Vec::new();
        for entry in &repeated_updates {
            bytes.extend(encode_entry(entry).unwrap());
        }
        apply_entries(&mut cache, &bytes, false).unwrap();
        assert_eq!(cache.snapshot.by_uuid["same-row"], 0);
        assert_eq!(cache.snapshot.next_message_index, 1);

        let removed_and_recreated = [
            IdentityLogEntry::Append {
                uuid: "next-row".into(),
                message_index: 1,
            },
            IdentityLogEntry::Remove {
                uuid: "same-row".into(),
                next_message_index: 2,
            },
            IdentityLogEntry::Append {
                uuid: "same-row".into(),
                message_index: 2,
            },
        ];
        let mut bytes = Vec::new();
        for entry in &removed_and_recreated {
            bytes.extend(encode_entry(entry).unwrap());
        }
        apply_entries(&mut cache, &bytes, false).unwrap();

        assert_eq!(cache.snapshot.by_uuid["same-row"], 2);
        assert_eq!(cache.snapshot.by_uuid["next-row"], 1);
        assert_eq!(cache.snapshot.next_message_index, 3);
    }
}
