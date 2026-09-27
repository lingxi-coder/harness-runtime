//! Root-pinned append-only journal primitives for durable session state.
//!
//! The journal is deliberately generic.  The application layer owns the
//! event payload (cost vectors, terminal Fusion facts, and outbox projections)
//! while this module owns ordering, stable event identity, fsync boundaries,
//! and fail-closed recovery.  All methods are synchronous: callers must run
//! them on a blocking worker when used from an async runtime.

use platform_api::rooted_fs::{
    atomic_write_pinned, lock_exclusive_pinned, open_append_file_pinned, open_read_file_pinned,
    root_identity, sync_parent_pinned, truncate_file_pinned, AtomicWriteOptions, RootIdentity,
};
use platform_api::FsError;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use thiserror::Error;

/// Current envelope schema.  Unknown versions are never skipped: doing so
/// could make a replayed cost or terminal record appear durable when it was
/// written under a different interpretation.
pub const JOURNAL_SCHEMA_VERSION: u16 = 1;
/// Authoritative append-only state log name.
pub const JOURNAL_FILE_NAME: &str = "ledger.v1.jsonl";
/// Derivative snapshot name.
pub const SNAPSHOT_FILE_NAME: &str = "snapshot.v1.json";
/// Stable transaction lock shared by journal and snapshot operations.
pub const JOURNAL_LOCK_FILE_NAME: &str = "ledger.lock";
/// Owner-only session state directory mode.
pub const SESSION_STATE_DIR_MODE: u32 = 0o700;
/// Owner-only journal/snapshot mode.
pub const SESSION_STATE_FILE_MODE: u32 = 0o600;
/// Default maximum encoded envelope size.  Recovery never reads an
/// unbounded attacker-controlled line into memory.
pub const DEFAULT_MAX_RECORD_BYTES: usize = 4 * 1024 * 1024;

/// One versioned journal envelope.  `event_id` is the idempotency key and is
/// retained even when the caller's acknowledgement receiver has been dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEnvelope<E> {
    /// Envelope schema version.
    pub schema_version: u16,
    /// Strictly increasing sequence assigned by the journal owner.
    pub journal_revision: u64,
    /// Stable mutation/delivery identity.
    pub event_id: String,
    /// Typed event payload.
    pub event: E,
}

/// A validated journal event after replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEntry {
    /// Monotonic journal revision.
    pub journal_revision: u64,
    /// Stable event identity.
    pub event_id: String,
    /// JSON payload retained for app-tier decoding.
    pub event: Value,
}

/// Replay result, including whether a malformed final tail was repaired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalReplay {
    /// Valid events in revision order.
    pub entries: Vec<JournalEntry>,
    /// Last authoritative revision (zero for an empty journal).
    pub last_revision: u64,
    /// True only when a malformed, non-newline-terminated final tail was
    /// truncated under the journal lock.
    pub repaired_final_tail: bool,
    /// Whether the authoritative WAL file existed, including an empty file.
    /// Recovery uses this to reject a derivative snapshot that has no WAL.
    pub journal_present: bool,
}

/// Result of an append-once operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalAppend {
    /// Revision assigned to the new event, or the existing event on a
    /// duplicate idempotent append.
    pub journal_revision: u64,
    /// Whether the event was already present with identical content.
    pub duplicate: bool,
}

/// Snapshot envelope.  Snapshots are derivatives and can always be rebuilt
/// from a valid WAL prefix; they are never treated as authoritative alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalSnapshot<S> {
    /// Snapshot schema version.
    pub schema_version: u16,
    /// Last journal revision folded into `state`.
    pub last_journal_revision: u64,
    /// Typed folded state.
    pub state: S,
}

/// Fail-closed journal errors.  The original bytes are left untouched for
/// interior/newline-final corruption, version mismatches, gaps, and event-id
/// conflicts.
#[derive(Debug, Error)]
pub enum JournalError {
    /// Rooted filesystem operation failed.
    #[error(transparent)]
    Fs(#[from] FsError),
    /// JSON envelope or snapshot could not be decoded.
    #[error("journal JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// A malformed final non-newline tail may be repaired only when this is
    /// the final physical record.
    #[error("malformed final journal tail at byte {offset}")]
    MalformedFinalTail {
        /// Byte offset of the invalid tail.
        offset: u64,
    },
    /// Corruption in an interior or newline-terminated record.
    #[error("journal corruption at byte {offset}: {reason}")]
    Corrupted {
        /// Byte offset of the offending line.
        offset: u64,
        /// Stable diagnostic.
        reason: String,
    },
    /// Envelope schema is not understood by this binary.
    #[error("unsupported journal schema version {version}")]
    UnsupportedVersion {
        /// Unknown version observed on disk.
        version: u16,
    },
    /// A revision was skipped or regressed.
    #[error("journal revision {actual} does not follow {expected}")]
    RevisionGap {
        /// Revision found on disk.
        actual: u64,
        /// Revision required by the prefix.
        expected: u64,
    },
    /// The same event id was reused for a different payload.
    #[error("journal event id conflict: {event_id}")]
    EventConflict {
        /// Conflicting stable identity.
        event_id: String,
    },
    /// A record exceeds the bounded scanner limit.
    #[error("journal record exceeds {limit} bytes")]
    RecordTooLarge {
        /// Configured limit.
        limit: usize,
    },
    /// No next journal revision can be represented.
    #[error("journal revision overflow")]
    RevisionOverflow,
    /// The session-state root is not a real directory.
    #[error("session-state root is not a regular directory: {0}")]
    InvalidRoot(String),
}

/// Where a damaged ledger and snapshot were set aside, and why.
#[derive(Debug, Clone)]
pub struct QuarantineReport {
    /// Paths the damaged files were moved to. Empty when neither existed.
    pub moved: Vec<PathBuf>,
    /// The failure that triggered the quarantine.
    pub reason: String,
}

/// Root-pinned journal owner. A `DurableJournal` is cheap to clone by value and
/// carries the exact identity of the opened session directory.
///
/// Production mutation requires the canonical session's exclusive writer
/// claim. All cooperating writers must use this type and its `ledger.lock`;
/// within that boundary clones share a validated-prefix index and appends do
/// not rescan historical payloads. Metadata changes trigger a full replay, but
/// the cache does not claim to detect a hostile writer that rewrites bytes and
/// restores every observable file attribute. The WAL, never this derivative
/// index, remains authoritative on recovery.
#[derive(Debug, Clone)]
pub struct DurableJournal {
    root: PathBuf,
    identity: RootIdentity,
    max_record_bytes: usize,
    /// Derivative metadata shared by clones of one pinned writer. The WAL is
    /// still authoritative; this index is discarded and rebuilt whenever its
    /// exact-handle fingerprint no longer describes a known append-only
    /// prefix.
    index: Arc<Mutex<Option<JournalIndex>>>,
    #[cfg(test)]
    diagnostics: Arc<JournalDiagnostics>,
}

struct JournalScan {
    replay: JournalReplay,
    locations: std::collections::HashMap<String, JournalRecordLocation>,
    validated_len: u64,
    fingerprint: Option<JournalFileFingerprint>,
}

struct JournalScanState {
    entries: Vec<JournalEntry>,
    /// Thin metadata only. Cumulative cost vectors can make each event large;
    /// retaining every decoded payload would recreate a whole-WAL allocation.
    /// On the exceptional repeated-id path the scanner rereads just the one
    /// prior bounded record for an exact comparison.
    seen: std::collections::HashMap<String, JournalRecordLocation>,
    next_revision: Option<u64>,
    last_revision: u64,
}

#[derive(Debug, Clone, Copy)]
struct JournalRecordLocation {
    offset: u64,
    content_len: usize,
    journal_revision: u64,
}

#[derive(Debug)]
struct JournalIndex {
    journal_present: bool,
    validated_len: u64,
    last_revision: u64,
    locations: std::collections::HashMap<String, JournalRecordLocation>,
    fingerprint: Option<JournalFileFingerprint>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct JournalFileFingerprint {
    len: u64,
    modified: Option<std::time::SystemTime>,
    created: Option<std::time::SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
    #[cfg(windows)]
    creation_ticks: u64,
    #[cfg(windows)]
    last_write_ticks: u64,
    #[cfg(windows)]
    attributes: u32,
}

#[cfg(test)]
#[derive(Debug, Default)]
struct JournalDiagnostics {
    scanned_records: std::sync::atomic::AtomicU64,
    indexed_record_reads: std::sync::atomic::AtomicU64,
    full_scans: std::sync::atomic::AtomicU64,
    suffix_scans: std::sync::atomic::AtomicU64,
    fail_next_append_sync: std::sync::atomic::AtomicBool,
    fail_next_parent_sync: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct JournalDiagnosticSnapshot {
    scanned_records: u64,
    indexed_record_reads: u64,
    full_scans: u64,
    suffix_scans: u64,
}

#[derive(Debug, Clone, Copy)]
enum JournalScanKind {
    Full,
    Suffix,
}

impl JournalScanState {
    fn new() -> Self {
        Self::after_revision(0)
    }

    fn after_revision(last_revision: u64) -> Self {
        Self {
            entries: Vec::new(),
            seen: std::collections::HashMap::new(),
            next_revision: last_revision.checked_add(1),
            last_revision,
        }
    }

    fn accept<F>(
        &mut self,
        envelope: JournalEnvelope<Value>,
        location: JournalRecordLocation,
        prior_locations: Option<&std::collections::HashMap<String, JournalRecordLocation>>,
        compare_prior: F,
    ) -> Result<Option<JournalEntry>, JournalError>
    where
        F: FnOnce(JournalRecordLocation, &Value) -> Result<bool, JournalError>,
    {
        if envelope.schema_version != JOURNAL_SCHEMA_VERSION {
            return Err(JournalError::UnsupportedVersion {
                version: envelope.schema_version,
            });
        }
        let Some(expected_revision) = self.next_revision else {
            return Err(JournalError::RevisionOverflow);
        };
        if envelope.journal_revision != expected_revision {
            return Err(JournalError::RevisionGap {
                actual: envelope.journal_revision,
                expected: expected_revision,
            });
        }
        self.last_revision = envelope.journal_revision;
        self.next_revision = envelope.journal_revision.checked_add(1);

        let previous = self.seen.get(&envelope.event_id).copied().or_else(|| {
            prior_locations.and_then(|locations| locations.get(&envelope.event_id).copied())
        });
        let duplicate = if let Some(previous) = previous {
            if !compare_prior(previous, &envelope.event)? {
                return Err(JournalError::EventConflict {
                    event_id: envelope.event_id.clone(),
                });
            }
            true
        } else {
            self.seen.insert(envelope.event_id.clone(), location);
            false
        };
        if duplicate {
            Ok(None)
        } else {
            Ok(Some(JournalEntry {
                journal_revision: envelope.journal_revision,
                event_id: envelope.event_id,
                event: envelope.event,
            }))
        }
    }
}

impl JournalIndex {
    fn from_scan(scan: &mut JournalScan) -> Self {
        Self {
            journal_present: scan.replay.journal_present,
            validated_len: scan.validated_len,
            last_revision: scan.replay.last_revision,
            locations: std::mem::take(&mut scan.locations),
            fingerprint: scan.fingerprint.clone(),
        }
    }
}

impl JournalFileFingerprint {
    fn from_file(file: &fs::File) -> Result<Self, JournalError> {
        let metadata = file
            .metadata()
            .map_err(|error| FsError::Io(error.to_string()))?;
        Ok(Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
            #[cfg(unix)]
            device: {
                use std::os::unix::fs::MetadataExt;
                metadata.dev()
            },
            #[cfg(unix)]
            inode: {
                use std::os::unix::fs::MetadataExt;
                metadata.ino()
            },
            #[cfg(unix)]
            changed_seconds: {
                use std::os::unix::fs::MetadataExt;
                metadata.ctime()
            },
            #[cfg(unix)]
            changed_nanoseconds: {
                use std::os::unix::fs::MetadataExt;
                metadata.ctime_nsec()
            },
            #[cfg(windows)]
            creation_ticks: {
                use std::os::windows::fs::MetadataExt;
                metadata.creation_time()
            },
            #[cfg(windows)]
            last_write_ticks: {
                use std::os::windows::fs::MetadataExt;
                metadata.last_write_time()
            },
            #[cfg(windows)]
            attributes: {
                use std::os::windows::fs::MetadataExt;
                metadata.file_attributes()
            },
        })
    }

    fn same_leaf(&self, other: &Self) -> bool {
        #[cfg(unix)]
        {
            self.device == other.device && self.inode == other.inode
        }
        #[cfg(windows)]
        {
            return self.creation_ticks == other.creation_ticks
                && self.attributes == other.attributes;
        }
        #[cfg(not(any(unix, windows)))]
        {
            self.created == other.created
        }
    }
}

impl DurableJournal {
    /// Open a session-state directory and capture its identity.  Creation is
    /// intentionally limited to the final directory path; callers that derive
    /// the path from untrusted input should first use a rooted parent helper.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, JournalError> {
        let root = root.into();
        if let Ok(metadata) = fs::symlink_metadata(&root) {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(JournalError::InvalidRoot(root.display().to_string()));
            }
        } else {
            fs::create_dir_all(&root).map_err(|error| FsError::Io(error.to_string()))?;
        }
        let identity = root_identity(&root)?;
        Ok(Self::from_pinned(root, identity))
    }

    /// Create/open a session directory below an already trusted root using
    /// the platform's no-follow directory-handle walk. The returned root path
    /// and identity refer to the same opened directory.
    pub fn open_under(root: &Path, relative: &Path) -> Result<Self, JournalError> {
        let identity = platform_api::rooted_fs::ensure_private_directory(
            root,
            relative,
            SESSION_STATE_DIR_MODE,
        )?;
        Ok(Self::from_pinned(root.join(relative), identity))
    }

    /// Construct from an already verified session directory identity.
    #[must_use]
    pub fn from_pinned(root: PathBuf, identity: RootIdentity) -> Self {
        Self {
            root,
            identity,
            max_record_bytes: DEFAULT_MAX_RECORD_BYTES,
            index: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            diagnostics: Arc::new(JournalDiagnostics::default()),
        }
    }

    /// Set a smaller/larger bounded record scanner for tests and hosts.
    #[must_use]
    pub fn with_max_record_bytes(mut self, max_record_bytes: usize) -> Self {
        self.max_record_bytes = max_record_bytes.max(1);
        // A clone may already have validated records under a different bound.
        // The configured scanner limit is part of the index interpretation, so
        // changing it starts a fresh derivative cache for this view.
        self.index = Arc::new(Mutex::new(None));
        #[cfg(test)]
        {
            self.diagnostics = Arc::new(JournalDiagnostics::default());
        }
        self
    }

    /// Session directory used as the root for every operation.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Captured identity of [`Self::root`].
    #[must_use]
    pub fn root_identity(&self) -> RootIdentity {
        self.identity
    }

    fn lock(&self) -> Result<platform_api::RootedFileLock, JournalError> {
        Ok(lock_exclusive_pinned(
            &self.root,
            Path::new(JOURNAL_LOCK_FILE_NAME),
            SESSION_STATE_DIR_MODE,
            SESSION_STATE_FILE_MODE,
            Some(&self.identity),
        )?)
    }

    /// Move this session's damaged ledger and snapshot aside, under the
    /// exclusive `ledger.lock`, and return where they went.
    ///
    /// The reader is deliberately untouched: it must keep refusing to parse a
    /// corrupt record, so recovery can never come from loosening it. What
    /// recovers is the caller, which decides that a derivative money ledger is
    /// worth less than the session it would otherwise block. The bytes are
    /// preserved, never deleted, so a damaged ledger stays inspectable.
    pub fn quarantine(&self, reason: &str) -> Result<QuarantineReport, JournalError> {
        let _lock = self.lock()?;
        // Both paths are validated relative names under a root whose identity
        // was verified when the journal was opened and re-verified here by the
        // lock. There is no rooted rename primitive; a rename between two
        // checked names inside that one directory is the narrowest operation
        // that preserves the evidence.
        let identity = platform_api::rooted_fs::root_identity(&self.root)?;
        if identity != self.identity {
            return Err(JournalError::InvalidRoot(
                self.root.to_string_lossy().into_owned(),
            ));
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis());
        let directory = platform_api::rooted_fs::checked_join(&self.root, Path::new("quarantine"))?;
        std::fs::create_dir_all(&directory).map_err(|error| FsError::Io(error.to_string()))?;
        let mut moved = Vec::new();
        for name in [JOURNAL_FILE_NAME, SNAPSHOT_FILE_NAME] {
            let from = platform_api::rooted_fs::checked_join(&self.root, Path::new(name))?;
            if !from.exists() {
                continue;
            }
            let to = directory.join(format!("{name}.{stamp}"));
            std::fs::rename(&from, &to).map_err(|error| FsError::Io(error.to_string()))?;
            moved.push(to);
        }
        platform_api::rooted_fs::sync_parent_pinned(
            &self.root,
            Path::new(JOURNAL_FILE_NAME),
            Some(&self.identity),
        )?;
        // The cached prefix index describes bytes that are no longer there.
        *self
            .index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        tracing::warn!(%reason, "cost ledger quarantined");
        Ok(QuarantineReport {
            moved,
            reason: reason.to_string(),
        })
    }

    fn read_locked(&self) -> Result<JournalReplay, JournalError> {
        let mut scan = self.scan_locked(true, None)?;
        self.replace_index_from_scan(&mut scan);
        Ok(scan.replay)
    }

    fn scan_locked(
        &self,
        collect_entries: bool,
        visitor: Option<&mut dyn FnMut(JournalEntry)>,
    ) -> Result<JournalScan, JournalError> {
        self.note_scan(JournalScanKind::Full);
        let relative = Path::new(JOURNAL_FILE_NAME);
        let file = match open_read_file_pinned(&self.root, relative, Some(&self.identity)) {
            Ok(file) => file,
            Err(FsError::NotFound(_)) => {
                return Ok(JournalScan {
                    replay: JournalReplay {
                        entries: Vec::new(),
                        last_revision: 0,
                        repaired_final_tail: false,
                        journal_present: false,
                    },
                    locations: std::collections::HashMap::new(),
                    validated_len: 0,
                    fingerprint: None,
                });
            }
            Err(error) => return Err(error.into()),
        };
        let mut scan = self.scan_file_locked(file, 0, 0, None, collect_entries, visitor)?;
        scan.fingerprint = self.current_fingerprint_locked()?;
        Ok(scan)
    }

    fn scan_suffix_locked(
        &self,
        start_offset: u64,
        last_revision: u64,
        prior_locations: &std::collections::HashMap<String, JournalRecordLocation>,
    ) -> Result<JournalScan, JournalError> {
        self.note_scan(JournalScanKind::Suffix);
        let relative = Path::new(JOURNAL_FILE_NAME);
        let mut file = open_read_file_pinned(&self.root, relative, Some(&self.identity))?;
        let length = JournalFileFingerprint::from_file(&file)?.len;
        if length < start_offset {
            return Err(JournalError::Corrupted {
                offset: length,
                reason: "journal shrank before suffix validation".into(),
            });
        }
        file.seek(SeekFrom::Start(start_offset))
            .map_err(|error| FsError::Io(error.to_string()))?;
        let mut scan = self.scan_file_locked(
            file,
            start_offset,
            last_revision,
            Some(prior_locations),
            false,
            None,
        )?;
        scan.fingerprint = self.current_fingerprint_locked()?;
        Ok(scan)
    }

    fn scan_file_locked(
        &self,
        file: fs::File,
        start_offset: u64,
        last_revision: u64,
        prior_locations: Option<&std::collections::HashMap<String, JournalRecordLocation>>,
        collect_entries: bool,
        mut visitor: Option<&mut dyn FnMut(JournalEntry)>,
    ) -> Result<JournalScan, JournalError> {
        let relative = Path::new(JOURNAL_FILE_NAME);
        let mut reader = BufReader::with_capacity(16 * 1024, file);
        let mut state = if start_offset == 0 {
            JournalScanState::new()
        } else {
            JournalScanState::after_revision(last_revision)
        };
        let mut line = Vec::new();
        let mut line_offset = start_offset;
        let mut repaired_final_tail = false;
        let mut missing_final_delimiter = false;
        let mut truncate_final_tail_at = None;

        loop {
            let available = reader
                .fill_buf()
                .map_err(|error| FsError::Io(error.to_string()))?;
            if available.is_empty() {
                break;
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let take = newline.map_or(available.len(), |index| index + 1);
            let content_len = newline.unwrap_or(take);
            let record_len = line
                .len()
                .checked_add(content_len)
                .and_then(|len| len.checked_add(usize::from(newline.is_some())))
                .ok_or(JournalError::RecordTooLarge {
                    limit: self.max_record_bytes,
                })?;
            if record_len > self.max_record_bytes {
                return Err(JournalError::RecordTooLarge {
                    limit: self.max_record_bytes,
                });
            }
            line.extend_from_slice(&available[..content_len]);
            reader.consume(take);
            if newline.is_some() {
                self.note_scanned_record();
                let envelope =
                    serde_json::from_slice::<JournalEnvelope<Value>>(&line).map_err(|error| {
                        JournalError::Corrupted {
                            offset: line_offset,
                            reason: error.to_string(),
                        }
                    })?;
                let location = JournalRecordLocation {
                    offset: line_offset,
                    content_len: line.len(),
                    journal_revision: envelope.journal_revision,
                };
                if let Some(entry) =
                    state.accept(envelope, location, prior_locations, |prior, event| {
                        self.entry_at(prior)
                            .map(|previous| previous.event == *event)
                    })?
                {
                    if collect_entries {
                        state.entries.push(entry);
                    } else if let Some(visitor) = visitor.as_deref_mut() {
                        visitor(entry);
                    }
                }
                let physical_record_len =
                    line.len()
                        .checked_add(1)
                        .ok_or(JournalError::RecordTooLarge {
                            limit: self.max_record_bytes,
                        })?;
                line_offset = line_offset
                    .checked_add(u64::try_from(physical_record_len).unwrap_or(u64::MAX))
                    .ok_or(JournalError::RevisionOverflow)?;
                line.clear();
            }
        }

        if !line.is_empty() {
            self.note_scanned_record();
            match serde_json::from_slice::<JournalEnvelope<Value>>(&line) {
                Ok(envelope) => {
                    let location = JournalRecordLocation {
                        offset: line_offset,
                        content_len: line.len(),
                        journal_revision: envelope.journal_revision,
                    };
                    if let Some(entry) =
                        state.accept(envelope, location, prior_locations, |prior, event| {
                            self.entry_at(prior)
                                .map(|previous| previous.event == *event)
                        })?
                    {
                        if collect_entries {
                            state.entries.push(entry);
                        } else if let Some(visitor) = visitor {
                            visitor(entry);
                        }
                    }
                    let physical_record_len =
                        line.len()
                            .checked_add(1)
                            .ok_or(JournalError::RecordTooLarge {
                                limit: self.max_record_bytes,
                            })?;
                    line_offset = line_offset
                        .checked_add(u64::try_from(physical_record_len).unwrap_or(u64::MAX))
                        .ok_or(JournalError::RevisionOverflow)?;
                    missing_final_delimiter = true;
                }
                Err(_) => {
                    truncate_final_tail_at = Some(line_offset);
                    repaired_final_tail = true;
                }
            }
        }

        drop(reader);
        if let Some(offset) = truncate_final_tail_at {
            truncate_file_pinned(&self.root, relative, offset, Some(&self.identity))?;
        }
        if missing_final_delimiter {
            let mut file = open_append_file_pinned(&self.root, relative, Some(&self.identity))?;
            file.write_all(b"\n")
                .map_err(|error| FsError::Io(error.to_string()))?;
            file.sync_all()
                .map_err(|error| FsError::Io(error.to_string()))?;
        }
        Ok(JournalScan {
            replay: JournalReplay {
                entries: state.entries,
                last_revision: state.last_revision,
                repaired_final_tail,
                journal_present: true,
            },
            locations: state.seen,
            validated_len: line_offset,
            fingerprint: None,
        })
    }

    fn current_fingerprint_locked(&self) -> Result<Option<JournalFileFingerprint>, JournalError> {
        match open_read_file_pinned(
            &self.root,
            Path::new(JOURNAL_FILE_NAME),
            Some(&self.identity),
        ) {
            Ok(file) => Ok(Some(JournalFileFingerprint::from_file(&file)?)),
            Err(FsError::NotFound(_)) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn replace_index_from_scan(&self, scan: &mut JournalScan) {
        *self
            .index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(JournalIndex::from_scan(scan));
    }

    fn reconcile_index_locked(
        &self,
        cached: &mut Option<JournalIndex>,
    ) -> Result<(), JournalError> {
        let current = self.current_fingerprint_locked()?;
        let action_is_suffix = cached.as_ref().is_some_and(|index| {
            matches!((&index.fingerprint, &current), (Some(previous), Some(observed))
                if index.journal_present
                    && previous.same_leaf(observed)
                    && previous.len == index.validated_len
                    && observed.len > index.validated_len)
        });
        let unchanged = cached.as_ref().is_some_and(|index| {
            index.fingerprint == current
                && index.validated_len == current.as_ref().map_or(0, |fingerprint| fingerprint.len)
        });
        if unchanged {
            return Ok(());
        }
        if action_is_suffix {
            let index = cached
                .as_mut()
                .expect("suffix action requires cached index");
            let mut scan = self.scan_suffix_locked(
                index.validated_len,
                index.last_revision,
                &index.locations,
            )?;
            index.locations.extend(std::mem::take(&mut scan.locations));
            index.validated_len = scan.validated_len;
            index.last_revision = scan.replay.last_revision;
            index.journal_present = scan.replay.journal_present;
            index.fingerprint = scan.fingerprint;
            return Ok(());
        }

        // Missing/replaced/truncated or metadata-detectable same-length edits
        // get a complete authoritative rebuild. The cache is not evidence and
        // is replaced only after the entire scan succeeds.
        let mut scan = self.scan_locked(false, None)?;
        *cached = Some(JournalIndex::from_scan(&mut scan));
        Ok(())
    }

    fn validate_synced_fingerprint(
        &self,
        previous: Option<&JournalFileFingerprint>,
        journal_present: bool,
        validated_len: u64,
        observed: Option<JournalFileFingerprint>,
    ) -> Result<Option<JournalFileFingerprint>, JournalError> {
        if !journal_present {
            return if observed.is_none() && validated_len == 0 {
                Ok(None)
            } else {
                Err(JournalError::Corrupted {
                    offset: validated_len,
                    reason: "missing journal changed during durability sync".into(),
                })
            };
        }
        let observed = observed.ok_or_else(|| JournalError::Corrupted {
            offset: validated_len,
            reason: "journal disappeared during durability sync".into(),
        })?;
        if observed.len != validated_len
            || previous.is_some_and(|previous| !previous.same_leaf(&observed))
        {
            return Err(JournalError::Corrupted {
                offset: validated_len,
                reason: "journal identity or length changed during durability sync".into(),
            });
        }
        Ok(Some(observed))
    }

    #[cfg(test)]
    fn note_scan(&self, kind: JournalScanKind) {
        use std::sync::atomic::Ordering;
        match kind {
            JournalScanKind::Full => {
                self.diagnostics.full_scans.fetch_add(1, Ordering::Relaxed);
            }
            JournalScanKind::Suffix => {
                self.diagnostics
                    .suffix_scans
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    #[cfg(not(test))]
    fn note_scan(&self, _kind: JournalScanKind) {}

    #[cfg(test)]
    fn note_scanned_record(&self) {
        self.diagnostics
            .scanned_records
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    #[cfg(not(test))]
    fn note_scanned_record(&self) {}

    #[cfg(test)]
    fn note_indexed_record_read(&self) {
        self.diagnostics
            .indexed_record_reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    #[cfg(not(test))]
    fn note_indexed_record_read(&self) {}

    #[cfg(test)]
    fn diagnostic_snapshot(&self) -> JournalDiagnosticSnapshot {
        use std::sync::atomic::Ordering;
        JournalDiagnosticSnapshot {
            scanned_records: self.diagnostics.scanned_records.load(Ordering::Relaxed),
            indexed_record_reads: self
                .diagnostics
                .indexed_record_reads
                .load(Ordering::Relaxed),
            full_scans: self.diagnostics.full_scans.load(Ordering::Relaxed),
            suffix_scans: self.diagnostics.suffix_scans.load(Ordering::Relaxed),
        }
    }

    #[cfg(test)]
    fn reset_diagnostics(&self) {
        use std::sync::atomic::Ordering;
        self.diagnostics.scanned_records.store(0, Ordering::Relaxed);
        self.diagnostics
            .indexed_record_reads
            .store(0, Ordering::Relaxed);
        self.diagnostics.full_scans.store(0, Ordering::Relaxed);
        self.diagnostics.suffix_scans.store(0, Ordering::Relaxed);
    }

    #[cfg(test)]
    fn fail_next_append_sync_for_test(&self) {
        self.diagnostics
            .fail_next_append_sync
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(test)]
    fn fail_next_parent_sync_for_test(&self) {
        self.diagnostics
            .fail_next_parent_sync
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(test)]
    fn check_append_sync_failpoint(&self) -> Result<(), JournalError> {
        if self
            .diagnostics
            .fail_next_append_sync
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(FsError::Io("synthetic journal append sync failure".into()).into());
        }
        Ok(())
    }

    #[cfg(not(test))]
    fn check_append_sync_failpoint(&self) -> Result<(), JournalError> {
        Ok(())
    }

    #[cfg(test)]
    fn check_parent_sync_failpoint(&self) -> Result<(), JournalError> {
        if self
            .diagnostics
            .fail_next_parent_sync
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(FsError::Io("synthetic journal parent sync failure".into()).into());
        }
        Ok(())
    }

    #[cfg(not(test))]
    fn check_parent_sync_failpoint(&self) -> Result<(), JournalError> {
        Ok(())
    }

    fn entry_at(&self, location: JournalRecordLocation) -> Result<JournalEntry, JournalError> {
        self.note_indexed_record_read();
        let relative = Path::new(JOURNAL_FILE_NAME);
        let mut file = open_read_file_pinned(&self.root, relative, Some(&self.identity))?;
        file.seek(SeekFrom::Start(location.offset))
            .map_err(|error| FsError::Io(error.to_string()))?;
        let mut bytes = vec![0_u8; location.content_len];
        file.read_exact(&mut bytes)
            .map_err(|error| FsError::Io(error.to_string()))?;
        let envelope =
            serde_json::from_slice::<JournalEnvelope<Value>>(&bytes).map_err(|error| {
                JournalError::Corrupted {
                    offset: location.offset,
                    reason: error.to_string(),
                }
            })?;
        if envelope.journal_revision != location.journal_revision {
            return Err(JournalError::Corrupted {
                offset: location.offset,
                reason: "indexed journal revision changed".into(),
            });
        }
        Ok(JournalEntry {
            journal_revision: envelope.journal_revision,
            event_id: envelope.event_id,
            event: envelope.event,
        })
    }

    /// Make a validated, already-present prefix durable before it can seed
    /// success acknowledgements after process recovery. Syncing the directory
    /// as well closes the crash window where complete file bytes were visible
    /// but the first directory entry had never been persisted.
    fn sync_durable_prefix_locked(
        &self,
        journal_present: bool,
    ) -> Result<Option<JournalFileFingerprint>, JournalError> {
        if !journal_present {
            return Ok(None);
        }
        let relative = Path::new(JOURNAL_FILE_NAME);
        let file = open_append_file_pinned(&self.root, relative, Some(&self.identity))?;
        file.sync_all()
            .map_err(|error| FsError::Io(error.to_string()))?;
        sync_parent_pinned(&self.root, relative, Some(&self.identity))?;
        Ok(Some(JournalFileFingerprint::from_file(&file)?))
    }

    /// Validate/replay the authoritative WAL.  A recoverable tail is repaired
    /// while the same transaction lock is held.
    pub fn replay(&self) -> Result<JournalReplay, JournalError> {
        let _lock = self.lock()?;
        self.read_locked()
    }

    /// Validate and durably sync the authoritative WAL while delivering each
    /// unique event to `visitor` one record at a time. The returned replay
    /// carries summary metadata with an empty `entries` vector; callers that
    /// need all payloads can continue to use [`Self::replay`].
    pub fn replay_durable_with<F>(&self, mut visitor: F) -> Result<JournalReplay, JournalError>
    where
        F: FnMut(JournalEntry),
    {
        let _lock = self.lock()?;
        let mut scan = self.scan_locked(false, Some(&mut visitor))?;
        let synced = self.sync_durable_prefix_locked(scan.replay.journal_present)?;
        scan.fingerprint = self.validate_synced_fingerprint(
            scan.fingerprint.as_ref(),
            scan.replay.journal_present,
            scan.validated_len,
            synced,
        )?;
        self.replace_index_from_scan(&mut scan);
        Ok(scan.replay)
    }

    /// Find one exact event without materializing unrelated payloads, and sync
    /// the validated prefix before returning it as durable truth.
    pub fn find_event_durable(&self, event_id: &str) -> Result<Option<JournalEntry>, JournalError> {
        let _lock = self.lock()?;
        let mut cached = self
            .index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.reconcile_index_locked(&mut cached)?;
        let index = cached
            .as_mut()
            .expect("journal index is initialized by reconciliation");
        let location = index.locations.get(event_id).copied();
        let entry = location
            .map(|location| self.entry_at(location))
            .transpose()?;
        if entry
            .as_ref()
            .is_some_and(|entry| entry.event_id != event_id)
        {
            return Err(JournalError::Corrupted {
                offset: location.map_or(0, |location| location.offset),
                reason: "indexed journal event id changed".into(),
            });
        }
        let synced = self.sync_durable_prefix_locked(index.journal_present)?;
        index.fingerprint = self.validate_synced_fingerprint(
            index.fingerprint.as_ref(),
            index.journal_present,
            index.validated_len,
            synced,
        )?;
        Ok(entry)
    }

    /// Append a typed event exactly once, assigning the next journal revision.
    /// The filesystem handle is fsynced before this method returns success.
    pub fn append_once<E: Serialize>(
        &self,
        event_id: impl Into<String>,
        event: &E,
    ) -> Result<JournalAppend, JournalError> {
        let event_id = event_id.into();
        let event = serde_json::to_value(event)?;
        let _lock = self.lock()?;
        let mut cached = self
            .index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.reconcile_index_locked(&mut cached)?;
        let index = cached
            .as_mut()
            .expect("journal index is initialized by reconciliation");
        if let Some(location) = index.locations.get(&event_id).copied() {
            let previous = self.entry_at(location)?;
            if previous.event_id != event_id {
                return Err(JournalError::Corrupted {
                    offset: location.offset,
                    reason: "indexed journal event id changed".into(),
                });
            }
            if previous.event == event {
                let synced = self.sync_durable_prefix_locked(index.journal_present)?;
                index.fingerprint = self.validate_synced_fingerprint(
                    index.fingerprint.as_ref(),
                    index.journal_present,
                    index.validated_len,
                    synced,
                )?;
                return Ok(JournalAppend {
                    journal_revision: previous.journal_revision,
                    duplicate: true,
                });
            }
            return Err(JournalError::EventConflict { event_id });
        }
        let envelope = JournalEnvelope {
            schema_version: JOURNAL_SCHEMA_VERSION,
            journal_revision: index
                .last_revision
                .checked_add(1)
                .ok_or(JournalError::RevisionOverflow)?,
            event_id: event_id.clone(),
            event,
        };
        let mut line = serde_json::to_vec(&envelope)?;
        line.push(b'\n');
        if line.len() > self.max_record_bytes {
            return Err(JournalError::RecordTooLarge {
                limit: self.max_record_bytes,
            });
        }
        let mut file = open_append_file_pinned(
            &self.root,
            Path::new(JOURNAL_FILE_NAME),
            Some(&self.identity),
        )?;
        file.write_all(&line)
            .map_err(|error| FsError::Io(error.to_string()))?;
        self.check_append_sync_failpoint()?;
        file.sync_all()
            .map_err(|error| FsError::Io(error.to_string()))?;
        if !index.journal_present {
            self.check_parent_sync_failpoint()?;
            sync_parent_pinned(
                &self.root,
                Path::new(JOURNAL_FILE_NAME),
                Some(&self.identity),
            )?;
        }
        let expected_len = index
            .validated_len
            .checked_add(u64::try_from(line.len()).unwrap_or(u64::MAX))
            .ok_or(JournalError::RevisionOverflow)?;
        let written = JournalFileFingerprint::from_file(&file)?;
        let current =
            self.current_fingerprint_locked()?
                .ok_or_else(|| JournalError::Corrupted {
                    offset: index.validated_len,
                    reason: "journal disappeared after append".into(),
                })?;
        if !written.same_leaf(&current)
            || written.len != expected_len
            || current.len != expected_len
        {
            return Err(JournalError::Corrupted {
                offset: index.validated_len,
                reason: "journal identity or length changed during append".into(),
            });
        }
        index.locations.insert(
            event_id,
            JournalRecordLocation {
                offset: index.validated_len,
                content_len: line.len().saturating_sub(1),
                journal_revision: envelope.journal_revision,
            },
        );
        index.validated_len = expected_len;
        index.last_revision = envelope.journal_revision;
        index.journal_present = true;
        index.fingerprint = Some(current);
        Ok(JournalAppend {
            journal_revision: envelope.journal_revision,
            duplicate: false,
        })
    }

    /// Read a derivative snapshot through the pinned root.  Snapshot parse or
    /// revision mismatch is reported to the caller, which can rebuild it from
    /// [`Self::replay`].
    pub fn read_snapshot<S: DeserializeOwned>(
        &self,
    ) -> Result<Option<JournalSnapshot<S>>, JournalError> {
        let _lock = self.lock()?;
        let file = match open_read_file_pinned(
            &self.root,
            Path::new(SNAPSHOT_FILE_NAME),
            Some(&self.identity),
        ) {
            Ok(file) => file,
            Err(FsError::NotFound(_)) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut bytes = Vec::new();
        file.take(
            u64::try_from(self.max_record_bytes)
                .unwrap_or(u64::MAX)
                .saturating_add(1),
        )
        .read_to_end(&mut bytes)
        .map_err(|error| FsError::Io(error.to_string()))?;
        if bytes.len() > self.max_record_bytes {
            return Err(JournalError::RecordTooLarge {
                limit: self.max_record_bytes,
            });
        }
        let snapshot = serde_json::from_slice::<JournalSnapshot<S>>(&bytes)?;
        if snapshot.schema_version != JOURNAL_SCHEMA_VERSION {
            return Err(JournalError::UnsupportedVersion {
                version: snapshot.schema_version,
            });
        }
        Ok(Some(snapshot))
    }

    /// Write a derivative snapshot.  WAL append remains the authoritative ack
    /// boundary; callers should log/repair a snapshot failure rather than
    /// freezing an already acknowledged mutation.
    pub fn write_snapshot<S: Serialize>(
        &self,
        last_journal_revision: u64,
        state: &S,
    ) -> Result<(), JournalError> {
        let snapshot = JournalSnapshot {
            schema_version: JOURNAL_SCHEMA_VERSION,
            last_journal_revision,
            state,
        };
        let bytes = serde_json::to_vec(&snapshot)?;
        let _lock = self.lock()?;
        atomic_write_pinned(
            &self.root,
            Path::new(SNAPSHOT_FILE_NAME),
            &bytes,
            AtomicWriteOptions {
                overwrite: true,
                create_parents: false,
                dir_mode: SESSION_STATE_DIR_MODE,
                file_mode: SESSION_STATE_FILE_MODE,
            },
            // `main` requires an identity here rather than accepting `None`:
            // this write always has one, and demanding it keeps a caller from
            // silently skipping the pinned-root check.
            &self.identity,
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn journal() -> (tempfile::TempDir, DurableJournal) {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = DurableJournal::open(dir.path()).expect("journal");
        (dir, journal)
    }

    #[test]
    fn append_once_reuses_revision_for_identical_id() {
        let (_dir, journal) = journal();
        let first = journal.append_once("m1", &json!({"total": 1})).unwrap();
        let duplicate = journal.append_once("m1", &json!({"total": 1})).unwrap();
        assert!(!first.duplicate);
        assert!(duplicate.duplicate);
        assert_eq!(duplicate.journal_revision, first.journal_revision);
        assert_eq!(journal.replay().unwrap().entries.len(), 1);
    }

    #[test]
    fn conflicting_duplicate_and_interior_corruption_fail_closed() {
        let (dir, journal) = journal();
        journal.append_once("m1", &json!({"total": 1})).unwrap();
        assert!(matches!(
            journal.append_once("m1", &json!({"total": 2})),
            Err(JournalError::EventConflict { .. })
        ));
        std::fs::write(dir.path().join(JOURNAL_FILE_NAME), b"not-json\n").unwrap();
        assert!(matches!(
            journal.replay(),
            Err(JournalError::Corrupted { .. })
        ));
        assert_eq!(
            std::fs::read(dir.path().join(JOURNAL_FILE_NAME)).unwrap(),
            b"not-json\n"
        );
    }

    #[test]
    fn malformed_non_newline_tail_is_the_only_repaired_tail() {
        let (dir, journal) = journal();
        journal.append_once("m1", &json!({"total": 1})).unwrap();
        let path = dir.path().join(JOURNAL_FILE_NAME);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(b"{broken");
        std::fs::write(&path, &bytes).unwrap();
        let replay = journal.replay().unwrap();
        assert!(replay.repaired_final_tail);
        assert_eq!(replay.entries.len(), 1);
        assert_eq!(std::fs::read(&path).unwrap(), bytes[..bytes.len() - 7]);
    }

    #[test]
    fn invalid_utf8_final_tail_is_repaired_without_decoding_the_whole_wal() {
        let (dir, journal) = journal();
        journal.append_once("m1", &json!({"total": 1})).unwrap();
        let path = dir.path().join(JOURNAL_FILE_NAME);
        let mut bytes = std::fs::read(&path).unwrap();
        let valid_prefix_len = bytes.len();
        bytes.extend_from_slice(&[0xff, 0xfe, 0xfd]);
        std::fs::write(&path, &bytes).unwrap();

        let replay = journal.replay().unwrap();

        assert!(replay.repaired_final_tail);
        assert_eq!(replay.entries.len(), 1);
        assert_eq!(std::fs::read(path).unwrap().len(), valid_prefix_len);
    }

    #[test]
    fn valid_final_record_without_newline_gets_a_safe_delimiter() {
        let (dir, journal) = journal();
        let first = JournalEnvelope {
            schema_version: JOURNAL_SCHEMA_VERSION,
            journal_revision: 1,
            event_id: "m1".to_string(),
            event: json!({"total": 1}),
        };
        let path = dir.path().join(JOURNAL_FILE_NAME);
        std::fs::write(&path, serde_json::to_vec(&first).unwrap()).unwrap();

        journal.append_once("m2", &json!({"total": 2})).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.ends_with(b"\n"));
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 2);
        let replay = journal.replay().unwrap();
        assert_eq!(replay.entries.len(), 2);
        assert_eq!(replay.last_revision, 2);
    }

    #[test]
    fn long_wal_is_scanned_with_a_per_record_bound() {
        let (dir, journal) = journal();
        let journal = journal.with_max_record_bytes(256);
        let path = dir.path().join(JOURNAL_FILE_NAME);
        let mut file = std::fs::File::create(&path).unwrap();
        for revision in 1..=2_000_u64 {
            let envelope = JournalEnvelope {
                schema_version: JOURNAL_SCHEMA_VERSION,
                journal_revision: revision,
                event_id: format!("m{revision}"),
                event: json!({"total": revision}),
            };
            serde_json::to_writer(&mut file, &envelope).unwrap();
            file.write_all(b"\n").unwrap();
        }
        file.sync_all().unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() > 256);

        let replay = journal.replay().unwrap();

        assert!(replay.journal_present);
        assert_eq!(replay.entries.len(), 2_000);
        assert_eq!(replay.last_revision, 2_000);
    }

    #[test]
    fn validated_prefix_index_makes_subsequent_appends_linear() {
        let (dir, journal) = journal();
        let path = dir.path().join(JOURNAL_FILE_NAME);
        let mut file = std::fs::File::create(&path).unwrap();
        for revision in 1..=2_000_u64 {
            let envelope = JournalEnvelope {
                schema_version: JOURNAL_SCHEMA_VERSION,
                journal_revision: revision,
                event_id: format!("seed-{revision}"),
                event: json!({"total": revision}),
            };
            serde_json::to_writer(&mut file, &envelope).unwrap();
            file.write_all(b"\n").unwrap();
        }
        file.sync_all().unwrap();
        drop(file);

        assert_eq!(journal.replay().unwrap().last_revision, 2_000);
        assert_eq!(journal.diagnostic_snapshot().scanned_records, 2_000);
        journal.reset_diagnostics();

        let mut last = None;
        for revision in 2_001..=3_000_u64 {
            last = Some(
                journal
                    .append_once(format!("new-{revision}"), &json!({"total": revision}))
                    .unwrap(),
            );
        }

        assert_eq!(last.unwrap().journal_revision, 3_000);
        assert_eq!(
            journal.diagnostic_snapshot(),
            JournalDiagnosticSnapshot {
                scanned_records: 0,
                indexed_record_reads: 0,
                full_scans: 0,
                suffix_scans: 0,
            },
            "appends on the unchanged owned prefix must not decode old WAL records"
        );
    }

    #[test]
    fn durable_replay_publishes_its_post_sync_fingerprint_for_the_first_append() {
        let (_dir, journal) = journal();
        journal.append_once("m1", &json!({"total": 1})).unwrap();
        assert_eq!(
            journal.replay_durable_with(|_| {}).unwrap().last_revision,
            1
        );
        journal.reset_diagnostics();

        let appended = journal.append_once("m2", &json!({"total": 2})).unwrap();

        assert_eq!(appended.journal_revision, 2);
        assert_eq!(
            journal.diagnostic_snapshot(),
            JournalDiagnosticSnapshot {
                scanned_records: 0,
                indexed_record_reads: 0,
                full_scans: 0,
                suffix_scans: 0,
            },
            "production hydration sync must leave the append cache warm"
        );
    }

    #[test]
    fn clones_share_the_thin_index_and_duplicates_read_one_record() {
        let (_dir, journal) = journal();
        journal.append_once("m1", &json!({"total": 1})).unwrap();
        let clone = journal.clone();
        journal.reset_diagnostics();

        clone.append_once("m2", &json!({"total": 2})).unwrap();
        let found = journal.find_event_durable("m1").unwrap().unwrap();
        assert_eq!(found.event, json!({"total": 1}));
        let duplicate = clone.append_once("m2", &json!({"total": 2})).unwrap();

        assert!(duplicate.duplicate);
        assert_eq!(duplicate.journal_revision, 2);
        assert_eq!(
            journal.diagnostic_snapshot(),
            JournalDiagnosticSnapshot {
                scanned_records: 0,
                indexed_record_reads: 2,
                full_scans: 0,
                suffix_scans: 0,
            }
        );
    }

    #[test]
    fn cooperating_handle_validates_only_the_new_suffix() {
        let (dir, first) = journal();
        first.append_once("m1", &json!({"total": 1})).unwrap();
        let second = DurableJournal::open(dir.path()).unwrap();
        second.append_once("m2", &json!({"total": 2})).unwrap();
        first.reset_diagnostics();

        let appended = first.append_once("m3", &json!({"total": 3})).unwrap();

        assert_eq!(appended.journal_revision, 3);
        assert_eq!(
            first.diagnostic_snapshot(),
            JournalDiagnosticSnapshot {
                scanned_records: 1,
                indexed_record_reads: 0,
                full_scans: 0,
                suffix_scans: 1,
            }
        );
    }

    #[test]
    fn complete_unacknowledged_suffix_is_reconciled_as_a_durable_duplicate() {
        let (dir, journal) = journal();
        journal.append_once("m1", &json!({"total": 1})).unwrap();
        let envelope = JournalEnvelope {
            schema_version: JOURNAL_SCHEMA_VERSION,
            journal_revision: 2,
            event_id: "m2".to_string(),
            event: json!({"total": 2}),
        };
        let path = dir.path().join(JOURNAL_FILE_NAME);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        serde_json::to_writer(&mut file, &envelope).unwrap();
        drop(file);
        journal.reset_diagnostics();

        let duplicate = journal.append_once("m2", &json!({"total": 2})).unwrap();

        assert!(duplicate.duplicate);
        assert_eq!(duplicate.journal_revision, 2);
        assert!(std::fs::read(&path).unwrap().ends_with(b"\n"));
        assert_eq!(journal.diagnostic_snapshot().scanned_records, 1);
        assert_eq!(journal.diagnostic_snapshot().suffix_scans, 1);
        assert_eq!(journal.diagnostic_snapshot().indexed_record_reads, 1);
    }

    #[test]
    fn torn_suffix_is_repaired_before_the_next_indexed_append() {
        let (dir, journal) = journal();
        journal.append_once("m1", &json!({"total": 1})).unwrap();
        let path = dir.path().join(JOURNAL_FILE_NAME);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(b"{broken");
        std::fs::write(&path, bytes).unwrap();
        journal.reset_diagnostics();

        let appended = journal.append_once("m2", &json!({"total": 2})).unwrap();

        assert_eq!(appended.journal_revision, 2);
        assert!(!appended.duplicate);
        assert!(!std::fs::read_to_string(&path).unwrap().contains("{broken"));
        assert_eq!(journal.diagnostic_snapshot().scanned_records, 1);
        assert_eq!(journal.diagnostic_snapshot().suffix_scans, 1);
    }

    #[test]
    fn failed_append_sync_keeps_the_old_prefix_for_exact_retry_reconciliation() {
        let (_dir, journal) = journal();
        journal.append_once("m1", &json!({"total": 1})).unwrap();
        journal.fail_next_append_sync_for_test();
        assert!(matches!(
            journal.append_once("m2", &json!({"total": 2})),
            Err(JournalError::Fs(FsError::Io(message)))
                if message.contains("append sync failure")
        ));
        journal.reset_diagnostics();

        let retry = journal.append_once("m2", &json!({"total": 2})).unwrap();

        assert!(retry.duplicate);
        assert_eq!(retry.journal_revision, 2);
        assert_eq!(journal.diagnostic_snapshot().scanned_records, 1);
        assert_eq!(journal.diagnostic_snapshot().suffix_scans, 1);
        assert_eq!(journal.replay().unwrap().entries.len(), 2);
    }

    #[test]
    fn failed_first_parent_sync_is_reconciled_without_a_second_record() {
        let (_dir, journal) = journal();
        journal.fail_next_parent_sync_for_test();
        assert!(matches!(
            journal.append_once("m1", &json!({"total": 1})),
            Err(JournalError::Fs(FsError::Io(message)))
                if message.contains("parent sync failure")
        ));
        journal.reset_diagnostics();

        let retry = journal.append_once("m1", &json!({"total": 1})).unwrap();

        assert!(retry.duplicate);
        assert_eq!(retry.journal_revision, 1);
        assert_eq!(journal.diagnostic_snapshot().full_scans, 1);
        assert_eq!(journal.diagnostic_snapshot().scanned_records, 1);
        assert_eq!(journal.replay().unwrap().entries.len(), 1);
    }

    #[test]
    fn replacement_and_truncation_never_reuse_the_stale_prefix_index() {
        let (dir, journal) = journal();
        journal.append_once("m1", &json!({"total": 1})).unwrap();
        journal.append_once("m2", &json!({"total": 2})).unwrap();
        let path = dir.path().join(JOURNAL_FILE_NAME);
        let first_line = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_string();
        let replacement = dir.path().join("replacement.jsonl");
        std::fs::write(&replacement, format!("{first_line}\n")).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        journal.reset_diagnostics();

        let replacement_append = journal
            .append_once("replacement-m2", &json!({"total": 2}))
            .unwrap();
        assert_eq!(replacement_append.journal_revision, 2);
        assert_eq!(journal.diagnostic_snapshot().full_scans, 1);
        assert_eq!(journal.diagnostic_snapshot().scanned_records, 1);

        let first_line_len = u64::try_from(first_line.len() + 1).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(first_line_len)
            .unwrap();
        journal.reset_diagnostics();
        let truncation_append = journal
            .append_once("truncated-m2", &json!({"total": 3}))
            .unwrap();
        assert_eq!(truncation_append.journal_revision, 2);
        assert_eq!(journal.diagnostic_snapshot().full_scans, 1);
        assert_eq!(journal.diagnostic_snapshot().scanned_records, 1);
    }

    #[test]
    fn corrupt_incremental_suffix_does_not_publish_a_partial_index() {
        let (dir, journal) = journal();
        journal.append_once("m1", &json!({"total": 1})).unwrap();
        let path = dir.path().join(JOURNAL_FILE_NAME);
        let gap = JournalEnvelope {
            schema_version: JOURNAL_SCHEMA_VERSION,
            journal_revision: 3,
            event_id: "gap".to_string(),
            event: json!({"total": 3}),
        };
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        serde_json::to_writer(&mut file, &gap).unwrap();
        file.write_all(b"\n").unwrap();
        file.sync_all().unwrap();
        drop(file);
        journal.reset_diagnostics();

        assert!(matches!(
            journal.append_once("m2", &json!({"total": 2})),
            Err(JournalError::RevisionGap {
                actual: 3,
                expected: 2
            })
        ));
        assert_eq!(journal.diagnostic_snapshot().suffix_scans, 1);
        assert_eq!(journal.diagnostic_snapshot().scanned_records, 1);
        assert!(matches!(
            journal.append_once("m2", &json!({"total": 2})),
            Err(JournalError::RevisionGap {
                actual: 3,
                expected: 2
            })
        ));
        assert_eq!(journal.diagnostic_snapshot().suffix_scans, 2);
    }

    #[test]
    fn conflicting_incremental_duplicate_fails_without_replacing_the_index() {
        let (dir, journal) = journal();
        journal.append_once("m1", &json!({"total": 1})).unwrap();
        let path = dir.path().join(JOURNAL_FILE_NAME);
        let conflict = JournalEnvelope {
            schema_version: JOURNAL_SCHEMA_VERSION,
            journal_revision: 2,
            event_id: "m1".to_string(),
            event: json!({"total": 9}),
        };
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        serde_json::to_writer(&mut file, &conflict).unwrap();
        file.write_all(b"\n").unwrap();
        file.sync_all().unwrap();
        drop(file);
        journal.reset_diagnostics();

        assert!(matches!(
            journal.append_once("m2", &json!({"total": 2})),
            Err(JournalError::EventConflict { event_id }) if event_id == "m1"
        ));
        assert_eq!(journal.diagnostic_snapshot().suffix_scans, 1);
        assert_eq!(journal.diagnostic_snapshot().scanned_records, 1);
        assert_eq!(journal.diagnostic_snapshot().indexed_record_reads, 1);
        assert!(matches!(
            journal.append_once("m2", &json!({"total": 2})),
            Err(JournalError::EventConflict { event_id }) if event_id == "m1"
        ));
        assert_eq!(journal.diagnostic_snapshot().suffix_scans, 2);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_leaf_never_reuses_the_validated_prefix() {
        use std::os::unix::fs::symlink;

        let (dir, journal) = journal();
        journal.append_once("m1", &json!({"total": 1})).unwrap();
        let path = dir.path().join(JOURNAL_FILE_NAME);
        let saved = dir.path().join("saved-ledger.jsonl");
        std::fs::rename(&path, &saved).unwrap();
        symlink(&saved, &path).unwrap();

        assert!(matches!(
            journal.append_once("m2", &json!({"total": 2})),
            Err(JournalError::Fs(_))
        ));
    }

    #[test]
    fn same_length_prefix_corruption_forces_a_full_rebuild() {
        let (dir, journal) = journal();
        journal.append_once("m1", &json!({"total": 1})).unwrap();
        let path = dir.path().join(JOURNAL_FILE_NAME);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[0] = b'!';
        std::fs::write(&path, bytes).unwrap();
        journal.reset_diagnostics();

        assert!(matches!(
            journal.append_once("m2", &json!({"total": 2})),
            Err(JournalError::Corrupted { offset: 0, .. })
        ));
        assert_eq!(journal.diagnostic_snapshot().full_scans, 1);
        assert_eq!(journal.diagnostic_snapshot().scanned_records, 1);
    }

    #[test]
    fn durable_streaming_replay_does_not_collect_cumulative_payloads() {
        let (_dir, journal) = journal();
        journal.append_once("m1", &json!({"total": 1})).unwrap();
        journal.append_once("m2", &json!({"total": 2})).unwrap();
        let mut visited = Vec::new();

        let replay = journal
            .replay_durable_with(|entry| visited.push(entry))
            .unwrap();

        assert!(replay.entries.is_empty());
        assert_eq!(replay.last_revision, 2);
        assert_eq!(
            visited
                .iter()
                .map(|entry| entry.event_id.as_str())
                .collect::<Vec<_>>(),
            vec!["m1", "m2"]
        );
    }

    #[test]
    fn streaming_scan_rereads_one_prior_record_for_exact_duplicate_comparison() {
        let (dir, journal) = journal();
        let path = dir.path().join(JOURNAL_FILE_NAME);
        let envelopes = [
            JournalEnvelope {
                schema_version: JOURNAL_SCHEMA_VERSION,
                journal_revision: 1,
                event_id: "m1".to_string(),
                event: json!({"total": 1}),
            },
            JournalEnvelope {
                schema_version: JOURNAL_SCHEMA_VERSION,
                journal_revision: 2,
                event_id: "m1".to_string(),
                event: json!({"total": 1}),
            },
            JournalEnvelope {
                schema_version: JOURNAL_SCHEMA_VERSION,
                journal_revision: 3,
                event_id: "m2".to_string(),
                event: json!({"total": 2}),
            },
        ];
        let mut file = std::fs::File::create(&path).unwrap();
        for envelope in envelopes {
            serde_json::to_writer(&mut file, &envelope).unwrap();
            file.write_all(b"\n").unwrap();
        }
        file.sync_all().unwrap();
        let mut visited = Vec::new();

        let replay = journal
            .replay_durable_with(|entry| visited.push(entry.event_id))
            .unwrap();

        assert_eq!(replay.last_revision, 3);
        assert_eq!(visited, vec!["m1", "m2"]);

        let conflicting = JournalEnvelope {
            schema_version: JOURNAL_SCHEMA_VERSION,
            journal_revision: 4,
            event_id: "m1".to_string(),
            event: json!({"total": 9}),
        };
        serde_json::to_writer(&mut file, &conflicting).unwrap();
        file.write_all(b"\n").unwrap();
        file.sync_all().unwrap();
        assert!(matches!(
            journal.replay_durable_with(|_| {}),
            Err(JournalError::EventConflict { event_id }) if event_id == "m1"
        ));
    }

    #[test]
    fn revision_gaps_are_not_repaired() {
        let (dir, journal) = journal();
        let envelope = JournalEnvelope {
            schema_version: JOURNAL_SCHEMA_VERSION,
            journal_revision: 2,
            event_id: "m2".to_string(),
            event: json!({"total": 2}),
        };
        let mut bytes = serde_json::to_vec(&envelope).unwrap();
        bytes.push(b'\n');
        std::fs::write(dir.path().join(JOURNAL_FILE_NAME), &bytes).unwrap();
        assert!(matches!(
            journal.replay(),
            Err(JournalError::RevisionGap { .. })
        ));
        assert_eq!(
            std::fs::read(dir.path().join(JOURNAL_FILE_NAME)).unwrap(),
            bytes
        );
    }
}
