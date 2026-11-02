//! Effectively-once transcript delivery under a session-state lock.
//!
//! `JsonlWriter` owns the session transcript targets. This writer supplies
//! their durable path for terminal/outbox records: it uses the
//! pinned session-state directory as the lock root, checks the stable delivery
//! id under that same lock, appends through the exact no-follow handle, and
//! fsyncs before acknowledging success.

use crate::jsonl::exact_json::{payload_without_delivery_id, ExactJsonError};
use crate::jsonl::journal::{SESSION_STATE_DIR_MODE, SESSION_STATE_FILE_MODE};
use crate::jsonl::message_identity::{self, IdentityLogStore};
use lingxi_core::host::rooted_fs::{
    atomic_write_stream_pinned_checked, lock_exclusive_pinned, open_append_file_pinned,
    open_read_file_pinned, root_identity, sync_parent_pinned, AtomicWriteOptions, RootIdentity,
};
use lingxi_core::host::FsError;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use thiserror::Error;

/// Stable lock filename shared by ordinary transcript appends, `/cd`
/// relocation, and durable outbox delivery.
pub const TRANSCRIPT_LOCK_FILE_NAME: &str = "transcript.lock";

/// Reject an unterminated source before any sidecar or transcript mutation.
pub(crate) fn require_complete_transcript_tail(file: &mut std::fs::File) -> Result<(), FsError> {
    let length = file
        .metadata()
        .map_err(|error| FsError::Io(error.to_string()))?
        .len();
    if length > 0 {
        file.seek(SeekFrom::End(-1))
            .map_err(|error| FsError::Io(error.to_string()))?;
        let mut last = [0u8; 1];
        file.read_exact(&mut last)
            .map_err(|error| FsError::Io(error.to_string()))?;
        if last != [b'\n'] {
            return Err(FsError::Io(
                "transcript has an incomplete final record".into(),
            ));
        }
    }
    Ok(())
}
/// In-memory record comparison and new Fusion record bound. Larger ordinary
/// history rows are validated by streaming, retaining only identity metadata.
pub const DEFAULT_MAX_TRANSCRIPT_SCAN_BYTES: usize = 2 * 1024 * 1024;
const TOMBSTONE_TAIL_BYTES: u64 = 64 * 1024;
const TOMBSTONE_REWRITE_LIMIT_BYTES: u64 = 50 * 1024 * 1024;

/// Append-once result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptAppendOutcome {
    /// A new line was fsynced.
    Appended,
    /// An identical stable delivery id was already present.
    AlreadyPresent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DuplicatePayloadPolicy {
    Exact,
    PendingPrompt,
}

/// Durable transcript errors.  A conflict is never repaired by appending a
/// second interpretation of the same delivery id.
#[derive(Debug, Error)]
pub enum TranscriptWriterError {
    /// Exact JavaScript JSON string encoding failed.
    #[error(transparent)]
    ExactJson(#[from] ExactJsonError),
    /// Flat source JSON projection failed validation or serialization.
    #[error(transparent)]
    Projection(#[from] lingxi_core::types::utf16_json::Utf16JsonProjectionError),
    /// Rooted filesystem failure.
    #[error(transparent)]
    Fs(#[from] FsError),
    /// A fresh complete row was written, but its durability acknowledgement
    /// failed. The visible chain may reference it; publication must still
    /// remain failed until a retry crosses the durability boundary.
    #[error("transcript row was written but durability failed: {0}")]
    WrittenButNotDurable(#[source] FsError),
    /// Payload must be an object so the host can attach the delivery id.
    #[error("transcript payload must be a JSON object")]
    PayloadNotObject,
    /// Durable outbox messages must carry their deterministic transcript UUID.
    #[error("transcript payload is missing a string uuid")]
    MissingMessageUuid,
    /// A native UUID-only row cannot be converted from a delivery-id row.
    #[error("native transcript payload must not carry deliveryId")]
    NativeDeliveryIdField,
    /// A stable delivery id was found with different content.
    #[error("transcript delivery id conflict: {delivery_id}")]
    DeliveryConflict {
        /// Conflicting id.
        delivery_id: String,
    },
    /// Bounded duplicate scan refused an oversized individual record.
    #[error("transcript record exceeds {limit} bytes")]
    ScanTooLarge {
        /// Scan bound.
        limit: usize,
    },
    /// Existing line was not valid JSON while checking idempotency.
    #[error("transcript line at byte {offset} is not valid JSON")]
    CorruptLine {
        /// Byte offset.
        offset: u64,
    },
}

impl From<lingxi_core::host::rooted_fs::AtomicPublishError> for TranscriptWriterError {
    fn from(error: lingxi_core::host::rooted_fs::AtomicPublishError) -> Self {
        if error.published {
            Self::WrittenButNotDurable(error.error)
        } else {
            Self::Fs(error.error)
        }
    }
}

/// Root-pinned transcript append owner.
#[derive(Debug, Clone)]
pub struct DurableTranscriptWriter {
    root: PathBuf,
    identity: RootIdentity,
    transcript_authority: Arc<RwLock<Option<TranscriptFileAuthority>>>,
    /// A published rewrite whose final state could not be confirmed must not
    /// be followed by another write using the old in-memory transcript tip.
    rewrite_uncertain: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(test)]
    authority_after_scan: Arc<std::sync::Mutex<Option<AuthorityTestGate>>>,
    #[cfg(test)]
    authority_after_write: Arc<std::sync::Mutex<Option<AuthorityTestGate>>>,
    #[cfg(test)]
    authority_after_rewrite: Arc<std::sync::Mutex<Option<AuthorityTestGate>>>,
    #[cfg(test)]
    authority_before_publish: Arc<std::sync::Mutex<Option<AuthorityTestGate>>>,
    max_record_bytes: usize,
    #[cfg(test)]
    fail_next_existing_sync: std::sync::Arc<std::sync::atomic::AtomicBool>,
    #[cfg(test)]
    fail_next_append_sync: std::sync::Arc<std::sync::atomic::AtomicBool>,
    #[cfg(test)]
    fail_next_append_write: std::sync::Arc<std::sync::atomic::AtomicBool>,
    #[cfg(test)]
    duplicate_scans: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Counts `fsync` calls made by this writer, so a regression can assert
    /// which appends pay for durability and which do not.
    #[cfg(test)]
    syncs: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
type AuthorityTestGate = (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);

/// Physical identity of an existing transcript captured by its host.
/// The root is trusted; each relative component is opened without following
/// links. Cooperative owners serialize through the session lease and durable
/// transaction. Observable external replacements fail or produce an uncertain
/// result; pathname checks do not provide an OS-level inode compare-and-swap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptFileAuthority {
    pub(crate) root: PathBuf,
    pub(crate) relative: PathBuf,
    pub(crate) root_identity: RootIdentity,
    pub(crate) file_identity: RootIdentity,
}

impl TranscriptFileAuthority {
    pub fn from_capture(
        root: PathBuf,
        relative: PathBuf,
        root_identity: RootIdentity,
        file_identity: RootIdentity,
    ) -> Result<Self, FsError> {
        if !root.is_absolute() {
            return Err(FsError::Io(
                "transcript authority root must be absolute".into(),
            ));
        }
        lingxi_core::host::rooted_fs::validate_relative_path(&relative)?;
        Ok(Self {
            root,
            relative,
            root_identity,
            file_identity,
        })
    }

    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.root.join(&self.relative)
    }

    /// Compare independently pinned views of the same physical source.
    #[must_use]
    pub fn same_source(&self, other: &Self) -> bool {
        let left = self.path();
        let right = other.path();
        let same_path = left == right
            || std::fs::canonicalize(&left)
                .ok()
                .zip(std::fs::canonicalize(&right).ok())
                .is_some_and(|(left, right)| left == right);
        same_path && self.file_identity == other.file_identity
    }

    /// Carry the same captured identity and bytes through a hot resume.
    #[must_use]
    pub fn pinned_resume_source(
        &self,
        expected_content: String,
    ) -> lingxi_core::host::PinnedResumeSource {
        lingxi_core::host::PinnedResumeSource {
            root: self.root.clone(),
            relative: self.relative.clone(),
            root_identity: self.root_identity,
            file_identity: self.file_identity,
            expected_content,
        }
    }

    /// Capture the first no-follow file identity and a stable source revision.
    pub fn capture(root: PathBuf, relative: PathBuf) -> Result<(Self, Vec<u8>), FsError> {
        let root_identity = lingxi_core::host::rooted_fs::root_identity(&root)?;
        let mut file = open_read_file_pinned(&root, &relative, Some(&root_identity))?;
        lingxi_core::host::rooted_fs::validate_open_file_single_link(&file)?;
        let file_identity = lingxi_core::host::rooted_fs::opened_file_identity(&file)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|error| FsError::Io(error.to_string()))?;
        let authority = Self::from_capture(root, relative, root_identity, file_identity)?;
        if authority.read_checked()? != bytes {
            return Err(FsError::Io("transcript changed during capture".into()));
        }
        Ok((authority, bytes))
    }

    /// Reread this same opened-file identity without following path links.
    pub fn read_checked(&self) -> Result<Vec<u8>, FsError> {
        let mut file =
            open_read_file_pinned(&self.root, &self.relative, Some(&self.root_identity))?;
        lingxi_core::host::rooted_fs::validate_open_file_single_link(&file)?;
        if lingxi_core::host::rooted_fs::opened_file_identity(&file)? != self.file_identity {
            return Err(FsError::Io("transcript file identity changed".into()));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|error| FsError::Io(error.to_string()))?;
        Ok(bytes)
    }

    /// Recheck the pinned root and file identity without rereading its body.
    pub fn verify_current(&self) -> Result<(), FsError> {
        self.open_checked().map(|_| ())
    }

    pub(crate) fn digest_checked(&self) -> Result<[u8; 32], FsError> {
        let mut file = self.open_checked()?;
        let mut digest = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = file
                .read(&mut buffer)
                .map_err(|error| FsError::Io(error.to_string()))?;
            if count == 0 {
                break;
            }
            digest.update(&buffer[..count]);
        }
        self.verify_current()?;
        Ok(digest.finalize().into())
    }

    pub(crate) fn verify_revision(&self, expected: &[u8; 32]) -> Result<(), FsError> {
        if &self.digest_checked()? != expected {
            return Err(FsError::Io(
                "transcript changed while staging rewrite".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn open_checked(&self) -> Result<std::fs::File, FsError> {
        let file = lingxi_core::host::rooted_fs::open_recovery_file(
            &self.root,
            &self.relative,
            &self.root_identity,
            &self.file_identity,
        )?;
        lingxi_core::host::rooted_fs::validate_open_file_single_link(&file)?;
        Ok(file)
    }
}

/// One stable transcript transaction. Target resolution, relocation, and the
/// append itself can all run while this guard owns the session-state lock,
/// without attempting to acquire the same lock recursively.
pub struct DurableTranscriptTransaction<'a> {
    writer: &'a DurableTranscriptWriter,
    _lock: lingxi_core::host::RootedFileLock,
}

#[derive(Default)]
struct TranscriptIdentity {
    uuid: Option<String>,
    delivery: Option<String>,
}

struct TranscriptExpectation {
    projection: Utf16JsonProjection,
    delivery_id: Option<String>,
}

struct IdentitySeed<'a> {
    budget: &'a Cell<Option<usize>>,
    limit: usize,
}

impl<'de> serde::de::DeserializeSeed<'de> for IdentitySeed<'_> {
    type Value = TranscriptIdentity;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> serde::de::Visitor<'de> for IdentitySeed<'_> {
    type Value = TranscriptIdentity;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("an ordinary transcript object")
    }

    fn visit_map<M: serde::de::MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
        let mut identity = TranscriptIdentity::default();
        loop {
            // Even adversarial top-level keys/identity values may not cause
            // serde's string scratch buffer to grow with the history body.
            self.budget.set(Some(self.limit));
            let key = map.next_key::<String>()?;
            self.budget.set(None);
            let Some(key) = key else { break };
            if matches!(key.as_str(), "uuid" | "deliveryId") {
                self.budget.set(Some(self.limit));
                let value = map.next_value::<Value>()?;
                self.budget.set(None);
                let value = value.as_str().map(str::to_owned);
                if key == "uuid" {
                    identity.uuid = value;
                } else {
                    identity.delivery = value;
                }
            } else {
                map.next_value::<serde::de::IgnoredAny>()?;
            }
        }
        Ok(identity)
    }
}

/// `IgnoredAny` skips large strings without allocating, but serde deliberately
/// does not validate their UTF-8 or escapes and uses a depth-sized scratch
/// stack. Guard those properties; JSON admits escaped lone surrogate units.
struct MetadataReader<'a, R> {
    reader: R,
    budget: &'a Cell<Option<usize>>,
    validation: StreamingStringValidation,
}

#[derive(Default)]
struct StreamingStringValidation {
    utf8_left: u8,
    utf8_min: u8,
    utf8_max: u8,
    depth: usize,
    in_string: bool,
    escape: StringEscape,
}

#[derive(Default)]
enum StringEscape {
    #[default]
    None,
    Escaped,
    Unicode {
        left: u8,
    },
}

impl StreamingStringValidation {
    fn accept(&mut self, byte: u8) -> bool {
        if self.utf8_left != 0 {
            if byte < self.utf8_min || byte > self.utf8_max {
                return false;
            }
            self.utf8_left -= 1;
            self.utf8_min = 0x80;
            self.utf8_max = 0xbf;
        } else {
            let (left, min, max) = match byte {
                0..=0x7f => (0, 0, 0),
                0xc2..=0xdf => (1, 0x80, 0xbf),
                0xe0 => (2, 0xa0, 0xbf),
                0xe1..=0xec | 0xee..=0xef => (2, 0x80, 0xbf),
                0xed => (2, 0x80, 0x9f),
                0xf0 => (3, 0x90, 0xbf),
                0xf1..=0xf3 => (3, 0x80, 0xbf),
                0xf4 => (3, 0x80, 0x8f),
                _ => return false,
            };
            self.utf8_left = left;
            self.utf8_min = min;
            self.utf8_max = max;
        }
        if !self.in_string {
            match byte {
                b'"' => self.in_string = true,
                b'[' | b'{' => {
                    self.depth += 1;
                    if self.depth > 128 {
                        return false;
                    }
                }
                b']' | b'}' => self.depth = self.depth.saturating_sub(1),
                _ => {}
            }
            return true;
        }
        self.escape = match std::mem::take(&mut self.escape) {
            StringEscape::None => match byte {
                b'"' => {
                    self.in_string = false;
                    StringEscape::None
                }
                b'\\' => StringEscape::Escaped,
                0..=0x1f => return false,
                _ => StringEscape::None,
            },
            StringEscape::Escaped => match byte {
                b'u' => StringEscape::Unicode { left: 4 },
                b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => StringEscape::None,
                _ => return false,
            },
            StringEscape::Unicode { left } => {
                if !byte.is_ascii_hexdigit() {
                    return false;
                }
                if left > 1 {
                    StringEscape::Unicode { left: left - 1 }
                } else {
                    StringEscape::None
                }
            }
        };
        true
    }
}

impl<R: Read> Read for MetadataReader<'_, R> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        let limit = self.budget.get().unwrap_or(output.len()).min(output.len());
        if limit == 0 {
            return Err(std::io::Error::other(
                "transcript identity metadata exceeds scan bound",
            ));
        }
        let count = self.reader.read(&mut output[..limit])?;
        if let Some(remaining) = self.budget.get() {
            self.budget.set(Some(remaining - count));
        }
        if !output[..count]
            .iter()
            .all(|byte| self.validation.accept(*byte))
            || (count == 0 && self.validation.utf8_left != 0)
        {
            return Err(std::io::Error::other(
                "invalid transcript string or excessive JSON nesting",
            ));
        }
        Ok(count)
    }
}

impl DurableTranscriptWriter {
    /// Revert an unacknowledged durable append while the transaction owns
    /// the transcript lock. Failure stays explicit if rollback cannot sync.
    fn rollback_undurable_append(
        &self,
        file: &mut std::fs::File,
        previous_len: u64,
        error: String,
    ) -> TranscriptWriterError {
        let rollback = file.set_len(previous_len).and_then(|()| {
            #[cfg(test)]
            self.syncs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            file.sync_all()
        });
        let message = match rollback {
            Ok(()) => error,
            Err(rollback) => {
                format!("{error}; restoring the previous transcript failed: {rollback}")
            }
        };
        TranscriptWriterError::WrittenButNotDurable(FsError::Io(message))
    }

    /// A failed write_all may leave half a JSON row. Roll back only bytes from
    /// this locked append so retrying cannot turn a full disk into lost history.
    fn append_complete_line(
        &self,
        file: &mut std::fs::File,
        line: &[u8],
    ) -> Result<(), TranscriptWriterError> {
        // The admitted read/write handle is append-mode on Unix; Windows
        // opens it for update, so explicitly seek to the end on both paths.
        file.seek(SeekFrom::End(0))
            .map_err(|error| FsError::Io(error.to_string()))?;
        let before = file
            .metadata()
            .map_err(|error| FsError::Io(error.to_string()))?
            .len();
        #[cfg(test)]
        let fail = self
            .fail_next_append_write
            .swap(false, std::sync::atomic::Ordering::SeqCst);
        #[cfg(not(test))]
        let fail = false;
        let result = if fail {
            file.write_all(&line[..line.len() / 2])
                .and_then(|()| Err(std::io::Error::from_raw_os_error(28)))
        } else {
            file.write_all(line)
        };
        if let Err(error) = result {
            file.set_len(before)
                .and_then(|()| file.sync_all())
                .map_err(|rollback| {
                    FsError::Io(format!(
                        "{error}; restoring the incomplete transcript append failed: {rollback}"
                    ))
                })?;
            return Err(FsError::Io(error.to_string()).into());
        }
        Ok(())
    }

    /// Open a pre-created session-state directory and capture its identity.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, TranscriptWriterError> {
        let root = root.into();
        let identity = root_identity(&root)?;
        Ok(Self::from_pinned(root, identity))
    }

    /// Construct from a caller-owned root identity.
    #[must_use]
    pub fn from_pinned(root: PathBuf, identity: RootIdentity) -> Self {
        Self {
            root,
            identity,
            transcript_authority: Arc::new(RwLock::new(None)),
            rewrite_uncertain: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(test)]
            authority_after_scan: Arc::new(std::sync::Mutex::new(None)),
            #[cfg(test)]
            authority_after_write: Arc::new(std::sync::Mutex::new(None)),
            #[cfg(test)]
            authority_after_rewrite: Arc::new(std::sync::Mutex::new(None)),
            #[cfg(test)]
            authority_before_publish: Arc::new(std::sync::Mutex::new(None)),
            max_record_bytes: DEFAULT_MAX_TRANSCRIPT_SCAN_BYTES,
            #[cfg(test)]
            fail_next_existing_sync: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(test)]
            fail_next_append_sync: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(test)]
            fail_next_append_write: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(test)]
            syncs: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            duplicate_scans: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    pub(crate) fn transcript_authority(&self) -> Option<TranscriptFileAuthority> {
        self.transcript_authority
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn bind_transcript_authority(
        &self,
        path: &Path,
        authority: Option<TranscriptFileAuthority>,
    ) -> Result<(), FsError> {
        let mut current = self
            .transcript_authority
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(authority) = authority {
            if authority.path() != path {
                return Err(FsError::Io(
                    "transcript authority does not match its target".into(),
                ));
            }
            // Recheck before publishing any part of the destination owner.
            let _file = authority.open_checked()?;
            *current = Some(authority);
        } else if current
            .as_ref()
            .is_some_and(|authority| authority.path() != path)
        {
            return Err(FsError::Io(
                "existing transcript authority cannot be discarded".into(),
            ));
        }
        Ok(())
    }

    fn open_authoritative_target(
        &self,
        root: &Path,
        relative: &Path,
    ) -> Result<Option<std::fs::File>, FsError> {
        let Some(authority) = self.transcript_authority() else {
            return Ok(None);
        };
        if root.join(relative) != authority.path() {
            return Err(FsError::Io(
                "transcript target differs from captured authority".into(),
            ));
        }
        authority.open_checked().map(Some)
    }

    fn verify_authoritative_target(&self, root: &Path, relative: &Path) -> Result<(), FsError> {
        self.open_authoritative_target(root, relative).map(|_| ())
    }

    fn confirm_authoritative_target_after_write(
        &self,
        root: &Path,
        relative: &Path,
    ) -> Result<(), TranscriptWriterError> {
        self.verify_authoritative_target(root, relative)
            .map_err(TranscriptWriterError::WrittenButNotDurable)
    }

    #[cfg(test)]
    pub(crate) fn install_authority_test_gate(&self, after_write: bool, gate: AuthorityTestGate) {
        let slot = if after_write {
            &self.authority_after_write
        } else {
            &self.authority_after_scan
        };
        *slot.lock().unwrap() = Some(gate);
    }

    #[cfg(test)]
    pub(crate) fn install_authority_rewrite_test_gate(&self, gate: AuthorityTestGate) {
        *self.authority_after_rewrite.lock().unwrap() = Some(gate);
    }

    #[cfg(test)]
    pub(crate) fn wait_authority_rewrite_test_gate(&self) {
        Self::wait_authority_test_gate(&self.authority_after_rewrite);
    }

    #[cfg(test)]
    pub(crate) fn install_authority_publish_test_gate(&self, gate: AuthorityTestGate) {
        *self.authority_before_publish.lock().unwrap() = Some(gate);
    }

    #[cfg(test)]
    pub(crate) fn wait_authority_publish_test_gate(&self) {
        Self::wait_authority_test_gate(&self.authority_before_publish);
    }

    #[cfg(test)]
    fn wait_authority_test_gate(slot: &std::sync::Mutex<Option<AuthorityTestGate>>) {
        if let Some((reached, release)) = slot.lock().unwrap().take() {
            let _ = reached.send(());
            let _ = release.recv();
        }
    }

    /// Bound the duplicate scan.
    #[must_use]
    pub fn with_max_scan_bytes(mut self, max_scan_bytes: usize) -> Self {
        self.max_record_bytes = max_scan_bytes.max(1);
        self
    }

    /// Begin one stable transcript transaction.
    pub fn begin_transaction(
        &self,
    ) -> Result<DurableTranscriptTransaction<'_>, TranscriptWriterError> {
        let lock = lock_exclusive_pinned(
            &self.root,
            Path::new(TRANSCRIPT_LOCK_FILE_NAME),
            SESSION_STATE_DIR_MODE,
            SESSION_STATE_FILE_MODE,
            Some(&self.identity),
        )?;
        // Check only after taking the shared lock: a previous rewrite can
        // become uncertain while this transaction is waiting for that lock.
        self.ensure_rewrite_is_resolved()?;
        Ok(DurableTranscriptTransaction {
            writer: self,
            _lock: lock,
        })
    }

    /// Fence this owner after an uncertain published rewrite. Call while the
    /// rewrite's transaction is still held; a validated remount creates a new
    /// owner after restoring the on-disk state.
    pub(crate) fn mark_rewrite_uncertain(&self) {
        self.rewrite_uncertain
            .store(true, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn rewrite_is_uncertain(&self) -> bool {
        self.rewrite_uncertain
            .load(std::sync::atomic::Ordering::Acquire)
    }

    fn ensure_rewrite_is_resolved(&self) -> Result<(), TranscriptWriterError> {
        if self.rewrite_is_uncertain() {
            return Err(TranscriptWriterError::WrittenButNotDurable(FsError::Io(
                "a published transcript rewrite is uncertain; reopen the session".into(),
            )));
        }
        Ok(())
    }

    /// Hold the stable session-state transcript lock while a caller resolves
    /// a cwd/project target, performs relocation, and appends through the
    /// supplied transaction guard. The closure is synchronous by design; a
    /// host should run it on a blocking worker and must not cancel it after
    /// filesystem IO begins.
    pub fn with_transaction<T, F>(&self, operation: F) -> Result<T, TranscriptWriterError>
    where
        F: FnOnce(&DurableTranscriptTransaction<'_>) -> Result<T, TranscriptWriterError>,
    {
        let transaction = self.begin_transaction()?;
        operation(&transaction)
    }

    /// Append a JSON object exactly once by `delivery_id`.
    pub fn append_json_once(
        &self,
        transcript_relative: &Path,
        delivery_id: &str,
        payload: Utf16JsonProjection,
    ) -> Result<TranscriptAppendOutcome, TranscriptWriterError> {
        self.append_json_once_at(
            &self.root,
            &self.identity,
            transcript_relative,
            delivery_id,
            payload,
        )
    }

    /// Append under a separate approved transcript root while retaining this
    /// writer's stable session-state lock.  The caller must pass the identity
    /// of the exact transcript parent opened during target resolution; this is
    /// what lets `/cd` relocation and outbox delivery share one lock without
    /// forcing the existing cwd-dependent transcript layout under session
    /// state.
    pub fn append_json_once_at(
        &self,
        transcript_root: &Path,
        transcript_identity: &RootIdentity,
        transcript_relative: &Path,
        delivery_id: &str,
        payload: Utf16JsonProjection,
    ) -> Result<TranscriptAppendOutcome, TranscriptWriterError> {
        self.begin_transaction()?.append_json_once_at(
            transcript_root,
            transcript_identity,
            transcript_relative,
            delivery_id,
            payload,
        )
    }

    /// Remove one native transcript UUID under the same cross-process
    /// transaction used by durable appends and relocation. The replacement is
    /// atomic, so a crash cannot leave the original file truncated between a
    /// tombstone and suffix copy.
    pub fn remove_message_by_uuid_at(
        &self,
        transcript_root: &Path,
        transcript_identity: &RootIdentity,
        transcript_relative: &Path,
        message_uuid: &str,
    ) -> Result<bool, TranscriptWriterError> {
        self.begin_transaction()?.remove_message_by_uuid_at(
            transcript_root,
            transcript_identity,
            transcript_relative,
            message_uuid,
        )
    }

    fn append_json_once_at_locked(
        &self,
        transcript_root: &Path,
        transcript_identity: &RootIdentity,
        transcript_relative: &Path,
        delivery_id: &str,
        payload: Utf16JsonProjection,
        stamp_delivery_id: bool,
        identity_registration: Option<(&IdentityLogStore, &Path)>,
    ) -> Result<(TranscriptAppendOutcome, bool), TranscriptWriterError> {
        if payload.object_entries().is_none() {
            return Err(TranscriptWriterError::PayloadNotObject);
        }
        if !stamp_delivery_id && payload.contains_property("deliveryId") {
            return Err(TranscriptWriterError::NativeDeliveryIdField);
        }
        let message_uuid = payload
            .string_at("/uuid")
            .filter(|uuid| !uuid.is_empty())
            .ok_or(TranscriptWriterError::MissingMessageUuid)?
            .to_string();
        let (outcome, last_uuid) = self.append_json_once_at_locked_with_policy(
            transcript_root,
            transcript_identity,
            transcript_relative,
            delivery_id,
            payload,
            stamp_delivery_id,
            identity_registration,
            DuplicatePayloadPolicy::Exact,
        )?;
        Ok((outcome, last_uuid.as_deref() == Some(message_uuid.as_str())))
    }

    fn append_pending_prompt_json_once_at_locked(
        &self,
        transcript_root: &Path,
        transcript_identity: &RootIdentity,
        transcript_relative: &Path,
        message_uuid: &str,
        payload: Utf16JsonProjection,
        identity_registration: Option<(&IdentityLogStore, &Path)>,
    ) -> Result<(TranscriptAppendOutcome, Option<String>), TranscriptWriterError> {
        self.append_json_once_at_locked_with_policy(
            transcript_root,
            transcript_identity,
            transcript_relative,
            message_uuid,
            payload,
            false,
            identity_registration,
            DuplicatePayloadPolicy::PendingPrompt,
        )
    }

    fn append_json_once_at_locked_with_policy(
        &self,
        transcript_root: &Path,
        transcript_identity: &RootIdentity,
        transcript_relative: &Path,
        delivery_id: &str,
        mut payload: Utf16JsonProjection,
        stamp_delivery_id: bool,
        identity_registration: Option<(&IdentityLogStore, &Path)>,
        comparison_policy: DuplicatePayloadPolicy,
    ) -> Result<(TranscriptAppendOutcome, Option<String>), TranscriptWriterError> {
        if payload.object_entries().is_none() {
            return Err(TranscriptWriterError::PayloadNotObject);
        }
        if !stamp_delivery_id && payload.contains_property("deliveryId") {
            return Err(TranscriptWriterError::NativeDeliveryIdField);
        }
        let message_uuid = payload
            .string_at("/uuid")
            .filter(|uuid| !uuid.is_empty())
            .ok_or(TranscriptWriterError::MissingMessageUuid)?
            .to_string();
        if stamp_delivery_id {
            payload
                .set_field("deliveryId", Utf16JsonProjection::from(delivery_id))
                .map_err(crate::jsonl::exact_json::projection_error)?;
        }
        // Keep the admitted file handle for both the scan and the append. A
        // later pathname replacement cannot redirect the write to a sibling.
        let mut admitted_file =
            self.open_authoritative_target(transcript_root, transcript_relative)?;
        let scan_file = match admitted_file.as_ref() {
            Some(file) => Ok(file
                .try_clone()
                .map_err(|error| FsError::Io(error.to_string()))?),
            None => open_read_file_pinned(
                transcript_root,
                transcript_relative,
                Some(transcript_identity),
            ),
        };
        let (file_present, missing_final_delimiter, existing_match, last_uuid) = match scan_file {
            Ok(file) => {
                let (missing_delimiter, matching, last_uuid) = self.scan_existing(
                    file,
                    delivery_id,
                    &message_uuid,
                    &payload,
                    comparison_policy,
                )?;
                (true, missing_delimiter, matching, last_uuid)
            }
            Err(FsError::NotFound(_)) => (false, false, None, None),
            Err(error) => return Err(error.into()),
        };
        #[cfg(test)]
        Self::wait_authority_test_gate(&self.authority_after_scan);
        if let Some(outcome) = existing_match {
            if outcome
                .as_ref()
                .is_ok_and(|value| *value == TranscriptAppendOutcome::AlreadyPresent)
            {
                // A complete line can be visible after a process died between
                // write and fsync. Never promote that observation to a durable
                // duplicate acknowledgement until both file and directory
                // have crossed the same persistence boundary as a new append.
                if let Some(file) = admitted_file.as_mut() {
                    file.sync_all()
                        .map_err(|error| FsError::Io(error.to_string()))?;
                    sync_parent_pinned(
                        transcript_root,
                        transcript_relative,
                        Some(transcript_identity),
                    )?;
                } else {
                    self.sync_existing_match(
                        transcript_root,
                        transcript_identity,
                        transcript_relative,
                    )?;
                }
                self.confirm_authoritative_target_after_write(
                    transcript_root,
                    transcript_relative,
                )?;
            }
            return outcome.map(|outcome| (outcome, last_uuid));
        }
        // Parentage is part of the immutable transcript payload. Resolve it
        // only after the UUID duplicate check, while the same transaction is
        // held, so a retry can keep the original parent even after later
        // messages were appended.
        let exact_boundary = payload.string_at("/type") == Some("system")
            && payload.string_at("/subtype") == Some("compact_boundary")
            && payload
                .at_pointer("/compactMetadata/nativeExactPreserved")
                .and_then(|value| value.as_bool())
                == Some(true);
        let needs_parent = !exact_boundary
            && payload
                .at_pointer("/parentUuid")
                .is_none_or(|parent| parent.is_null());
        if needs_parent {
            let parent = last_uuid.map_or_else(
                || Utf16JsonProjection::parse("null").expect("null is valid JSON"),
                Utf16JsonProjection::from,
            );
            payload
                .set_field("parentUuid", parent)
                .map_err(crate::jsonl::exact_json::projection_error)?;
        }
        let mut line = payload
            .to_json_string()
            .map_err(crate::jsonl::exact_json::projection_error)?
            .into_bytes();
        line.push(b'\n');
        if line.len() > self.max_record_bytes {
            return Err(TranscriptWriterError::ScanTooLarge {
                limit: self.max_record_bytes,
            });
        }
        if missing_final_delimiter {
            line.insert(0, b'\n');
        }
        self.verify_authoritative_target(transcript_root, transcript_relative)?;
        if let Some((store, identity_path)) = identity_registration {
            message_identity::append_row_identity_at(
                store,
                identity_path,
                transcript_root,
                transcript_identity,
                &message_uuid,
            )?;
        }
        let mut file = match admitted_file {
            Some(file) => file,
            None => open_append_file_pinned(
                transcript_root,
                transcript_relative,
                Some(transcript_identity),
            )?,
        };
        let exact_previous_len = if exact_boundary {
            Some(
                file.metadata()
                    .map_err(|error| FsError::Io(error.to_string()))?
                    .len(),
            )
        } else {
            None
        };
        self.append_complete_line(&mut file, &line)?;
        #[cfg(test)]
        if self
            .fail_next_append_sync
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            if let Some(previous_len) = exact_previous_len {
                return Err(self.rollback_undurable_append(
                    &mut file,
                    previous_len,
                    "synthetic written transcript sync failure".into(),
                ));
            }
            return Err(TranscriptWriterError::WrittenButNotDurable(FsError::Io(
                "synthetic written transcript sync failure".into(),
            )));
        }
        #[cfg(test)]
        self.syncs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Err(error) = file.sync_all() {
            if let Some(previous_len) = exact_previous_len {
                return Err(self.rollback_undurable_append(
                    &mut file,
                    previous_len,
                    error.to_string(),
                ));
            }
            return Err(TranscriptWriterError::WrittenButNotDurable(FsError::Io(
                error.to_string(),
            )));
        }
        if !file_present {
            #[cfg(test)]
            self.syncs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            sync_parent_pinned(
                transcript_root,
                transcript_relative,
                Some(transcript_identity),
            )
            .map_err(|error| match exact_previous_len {
                Some(previous_len) => {
                    self.rollback_undurable_append(&mut file, previous_len, error.to_string())
                }
                None => TranscriptWriterError::WrittenButNotDurable(error),
            })?;
        }
        #[cfg(test)]
        Self::wait_authority_test_gate(&self.authority_after_write);
        self.confirm_authoritative_target_after_write(transcript_root, transcript_relative)?;
        Ok((TranscriptAppendOutcome::Appended, Some(message_uuid)))
    }

    fn sync_existing_match(
        &self,
        transcript_root: &Path,
        transcript_identity: &RootIdentity,
        transcript_relative: &Path,
    ) -> Result<(), TranscriptWriterError> {
        #[cfg(test)]
        if self
            .fail_next_existing_sync
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(FsError::Io("synthetic existing transcript sync failure".into()).into());
        }
        let file = open_append_file_pinned(
            transcript_root,
            transcript_relative,
            Some(transcript_identity),
        )?;
        file.sync_all()
            .map_err(|error| FsError::Io(error.to_string()))?;
        sync_parent_pinned(
            transcript_root,
            transcript_relative,
            Some(transcript_identity),
        )?;
        Ok(())
    }

    #[cfg(test)]
    fn fail_next_existing_sync_for_test(&self) {
        self.fail_next_existing_sync
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn sync_count_for_test(&self) -> usize {
        self.syncs.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn duplicate_scan_count_for_test(&self) -> usize {
        self.duplicate_scans
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    fn scan_existing(
        &self,
        file: std::fs::File,
        delivery_id: &str,
        message_uuid: &str,
        payload: &Utf16JsonProjection,
        comparison_policy: DuplicatePayloadPolicy,
    ) -> Result<
        (
            bool,
            Option<Result<TranscriptAppendOutcome, TranscriptWriterError>>,
            Option<String>,
        ),
        TranscriptWriterError,
    > {
        #[cfg(test)]
        self.duplicate_scans
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let expected = payload.clone();
        let expected_delivery_id = expected.string_at("/deliveryId").map(str::to_owned);
        let expected = TranscriptExpectation {
            projection: expected,
            delivery_id: expected_delivery_id,
        };
        let mut reader = BufReader::with_capacity(16 * 1024, file);
        let mut line = Vec::new();
        let mut line_offset = 0_u64;
        let mut matching = None;
        let mut content_bytes = 0_u64;
        let mut last_uuid = None;

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
            content_bytes = content_bytes.checked_add(content_len as u64).ok_or(
                TranscriptWriterError::ScanTooLarge {
                    limit: self.max_record_bytes,
                },
            )?;
            if content_bytes + u64::from(newline.is_some()) <= self.max_record_bytes as u64 {
                line.extend_from_slice(&available[..content_len]);
            } else {
                line.clear();
            }
            reader.consume(take);
            if newline.is_some() {
                let physical_len =
                    content_bytes
                        .checked_add(1)
                        .ok_or(TranscriptWriterError::ScanTooLarge {
                            limit: self.max_record_bytes,
                        })?;
                let uuid = if physical_len > self.max_record_bytes as u64 {
                    self.inspect_large_line(
                        &mut reader,
                        line_offset,
                        physical_len,
                        delivery_id,
                        message_uuid,
                        &mut matching,
                    )?
                } else {
                    self.inspect_line(
                        &line,
                        line_offset,
                        delivery_id,
                        message_uuid,
                        &expected,
                        comparison_policy,
                        &mut matching,
                    )?
                };
                if uuid.is_some() {
                    last_uuid = uuid;
                }
                line_offset = line_offset.checked_add(physical_len).ok_or(
                    TranscriptWriterError::ScanTooLarge {
                        limit: self.max_record_bytes,
                    },
                )?;
                line.clear();
                content_bytes = 0;
            }
        }

        let missing_final_delimiter = content_bytes != 0;
        if missing_final_delimiter {
            let uuid = if content_bytes > self.max_record_bytes as u64 {
                self.inspect_large_line(
                    &mut reader,
                    line_offset,
                    content_bytes,
                    delivery_id,
                    message_uuid,
                    &mut matching,
                )?
            } else {
                self.inspect_line(
                    &line,
                    line_offset,
                    delivery_id,
                    message_uuid,
                    &expected,
                    comparison_policy,
                    &mut matching,
                )?
            };
            if uuid.is_some() {
                last_uuid = uuid;
            }
        }
        Ok((missing_final_delimiter, matching, last_uuid))
    }

    fn inspect_line(
        &self,
        line: &[u8],
        offset: u64,
        delivery_id: &str,
        message_uuid: &str,
        expected: &TranscriptExpectation,
        comparison_policy: DuplicatePayloadPolicy,
        matching: &mut Option<Result<TranscriptAppendOutcome, TranscriptWriterError>>,
    ) -> Result<Option<String>, TranscriptWriterError> {
        if line.is_empty() {
            return Ok(None);
        }
        let actual = std::str::from_utf8(line)
            .ok()
            .and_then(|line| Utf16JsonProjection::parse(line).ok())
            .ok_or(TranscriptWriterError::CorruptLine { offset })?;
        let stored_uuid = actual.string_at("/uuid");
        let last_uuid = stored_uuid
            .filter(|uuid| !uuid.is_empty())
            .map(str::to_owned);
        let stored_delivery_id = actual.string_at("/deliveryId");
        let matches_uuid = stored_uuid == Some(message_uuid);
        let matches_delivery = stored_delivery_id == Some(delivery_id);
        let identities_agree = matches_uuid
            && match expected.delivery_id.as_deref() {
                Some(expected_id) => stored_delivery_id == Some(expected_id),
                None => actual.at_pointer("/deliveryId").is_none(),
            };
        if matches_uuid || matches_delivery {
            let mut comparable_actual = payload_without_delivery_id(actual)?;
            let mut comparable_expected = payload_without_delivery_id(expected.projection.clone())?;
            // A caller may use null as the parent placeholder for a new
            // Fusion delivery. Existing duplicate rows carry their resolved
            // parent; compare all immutable fields while ignoring only this
            // derived field during the duplicate probe.
            if comparable_expected
                .at_pointer("/parentUuid")
                .is_none_or(|parent| parent.is_null())
            {
                comparable_actual
                    .remove_field("parentUuid")
                    .map_err(crate::jsonl::exact_json::projection_error)?;
                comparable_expected
                    .remove_field("parentUuid")
                    .map_err(crate::jsonl::exact_json::projection_error)?;
            }
            if comparison_policy == DuplicatePayloadPolicy::PendingPrompt {
                comparable_actual
                    .remove_field("timestamp")
                    .map_err(crate::jsonl::exact_json::projection_error)?;
                comparable_expected
                    .remove_field("timestamp")
                    .map_err(crate::jsonl::exact_json::projection_error)?;
            }
            let duplicate_payload_matches = match comparison_policy {
                DuplicatePayloadPolicy::Exact => comparable_actual == comparable_expected,
                DuplicatePayloadPolicy::PendingPrompt => {
                    // JSONL already contains the result of Native JSON
                    // serialization. Compare pending retries at that same
                    // boundary: values such as -0 and 1e309 are observable as
                    // 0 and null respectively after JSON.stringify, while
                    // source property order and UTF-16 code units remain
                    // significant. Do not use projection semantic equality,
                    // which would normalize object order differently.
                    crate::jsonl::exact_json::stringify_projection(&comparable_actual)?
                        == crate::jsonl::exact_json::stringify_projection(&comparable_expected)?
                }
            };
            // Native rows identify their prepared UUID alone. Fusion rows
            // carry the current delivery identity as well. A retry must use
            // the same format and immutable payload as its original append.
            let outcome = if identities_agree && duplicate_payload_matches {
                Ok(TranscriptAppendOutcome::AlreadyPresent)
            } else {
                Err(TranscriptWriterError::DeliveryConflict {
                    delivery_id: delivery_id.to_string(),
                })
            };
            if matching.is_none() {
                *matching = Some(outcome);
            } else if matching.as_ref().is_some_and(Result::is_ok) && outcome.is_err() {
                *matching = Some(outcome);
            }
        }
        Ok(last_uuid)
    }

    fn inspect_large_line(
        &self,
        reader: &mut BufReader<std::fs::File>,
        offset: u64,
        length: u64,
        delivery_id: &str,
        message_uuid: &str,
        matching: &mut Option<Result<TranscriptAppendOutcome, TranscriptWriterError>>,
    ) -> Result<Option<String>, TranscriptWriterError> {
        reader
            .seek(SeekFrom::Start(offset))
            .map_err(|error| FsError::Io(error.to_string()))?;
        let budget = Cell::new(Some(self.max_record_bytes));
        let bounded = MetadataReader {
            reader: Read::take(reader, length),
            budget: &budget,
            validation: StreamingStringValidation::default(),
        };
        let mut deserializer = serde_json::Deserializer::from_reader(bounded);
        let identity = serde::de::DeserializeSeed::deserialize(
            IdentitySeed {
                budget: &budget,
                limit: self.max_record_bytes,
            },
            &mut deserializer,
        )
        .map_err(|_| TranscriptWriterError::CorruptLine { offset })?;
        deserializer
            .end()
            .map_err(|_| TranscriptWriterError::CorruptLine { offset })?;
        if identity.uuid.as_deref() == Some(message_uuid)
            || identity.delivery.as_deref() == Some(delivery_id)
        {
            // A bounded new Fusion record cannot equal an oversized ordinary
            // record. Never skip a colliding identity just because its body is
            // large; failing closed preserves effectively-once publication.
            *matching = Some(Err(TranscriptWriterError::DeliveryConflict {
                delivery_id: delivery_id.into(),
            }));
        }
        Ok(identity.uuid.filter(|uuid| !uuid.is_empty()))
    }
}

impl DurableTranscriptTransaction<'_> {
    pub(crate) fn transfer_rewritten_target(
        &self,
        root: &Path,
        relative: &Path,
        expected_digest: Option<[u8; 32]>,
        expected_file_identity: Option<RootIdentity>,
    ) -> Result<(), TranscriptWriterError> {
        self.writer.ensure_rewrite_is_resolved()?;
        #[cfg(test)]
        self.writer.wait_authority_rewrite_test_gate();
        let Some(expected_digest) = expected_digest else {
            return Ok(());
        };
        let previous = self
            .writer
            .transcript_authority()
            .ok_or_else(|| FsError::Io("transcript authority disappeared during rewrite".into()))?;
        if previous.path() != root.join(relative) {
            return Err(FsError::Io("rewritten transcript target changed".into()).into());
        }
        let mut file = open_read_file_pinned(
            &previous.root,
            &previous.relative,
            Some(&previous.root_identity),
        )?;
        lingxi_core::host::rooted_fs::validate_open_file_single_link(&file)?;
        let file_identity = lingxi_core::host::rooted_fs::opened_file_identity(&file)?;
        if Some(file_identity) != expected_file_identity {
            return Err(FsError::Io(
                "rewritten transcript inode changed before authority transfer".into(),
            )
            .into());
        }
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = file
                .read(&mut buffer)
                .map_err(|error| FsError::Io(error.to_string()))?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
        }
        if <[u8; 32]>::from(hasher.finalize()) != expected_digest {
            return Err(FsError::Io(
                "rewritten transcript changed before authority transfer".into(),
            )
            .into());
        }
        let next = TranscriptFileAuthority::from_capture(
            previous.root,
            previous.relative,
            previous.root_identity,
            file_identity,
        )?;
        self.writer
            .bind_transcript_authority(&root.join(relative), Some(next))?;
        Ok(())
    }

    /// Append one source JSON value while this transaction owns the session
    /// lock. This is intentionally not append-once: relocation markers are
    /// ordinary transcript metadata and their caller serializes them with the
    /// shared in-process writer mutex.
    pub fn append_raw_json_at(
        &self,
        transcript_root: &Path,
        transcript_identity: &RootIdentity,
        transcript_relative: &Path,
        payload: Utf16JsonProjection,
    ) -> Result<(), TranscriptWriterError> {
        self.append_raw_json_at_with_file(
            transcript_root,
            transcript_identity,
            transcript_relative,
            payload,
            None,
        )
    }

    pub(crate) fn append_raw_json_at_with_file(
        &self,
        transcript_root: &Path,
        transcript_identity: &RootIdentity,
        transcript_relative: &Path,
        payload: Utf16JsonProjection,
        admitted_file: Option<std::fs::File>,
    ) -> Result<(), TranscriptWriterError> {
        self.append_raw_json_at_with_file_and_sync(
            transcript_root,
            transcript_identity,
            transcript_relative,
            payload,
            admitted_file,
            false,
        )
    }

    pub(crate) fn append_raw_json_at_with_file_and_sync(
        &self,
        transcript_root: &Path,
        transcript_identity: &RootIdentity,
        transcript_relative: &Path,
        payload: Utf16JsonProjection,
        admitted_file: Option<std::fs::File>,
        force_sync: bool,
    ) -> Result<(), TranscriptWriterError> {
        self.writer.ensure_rewrite_is_resolved()?;
        let mut line = payload
            .to_json_string()
            .map_err(crate::jsonl::exact_json::projection_error)?
            .into_bytes();
        line.push(b'\n');
        let file_present = admitted_file.is_some()
            || lingxi_core::host::rooted_fs::checked_join(transcript_root, transcript_relative)
                .map(|path| path.exists())
                .unwrap_or(false);
        let admitted_file = match admitted_file {
            Some(file) => Some(file),
            None => self
                .writer
                .open_authoritative_target(transcript_root, transcript_relative)?,
        };
        let mut file = match admitted_file {
            Some(file) => file,
            None => open_append_file_pinned(
                transcript_root,
                transcript_relative,
                Some(transcript_identity),
            )?,
        };
        self.writer
            .verify_authoritative_target(transcript_root, transcript_relative)?;
        if self.writer.transcript_authority().is_some() {
            require_complete_transcript_tail(&mut file)?;
        }
        // The fixed session mode must survive before its creation is published.
        let fixed_mode = payload.string_at("/type") == Some("fixed-session-mode");
        let mobile_anchor = payload.string_at("/type") == Some("custom-title")
            && payload.number_at("/mobileEmptySession") == Some(1.0);
        let no_authority = self.writer.transcript_authority().is_none();
        let fresh_mobile_anchor = no_authority && mobile_anchor;
        // A normal Desktop session starts with this immutable header. Retain
        // the inode opened for its first write as the live writer's authority.
        let fresh_fixed_header =
            no_authority && fixed_mode && payload.string_at("/sessionId").is_some();
        let pending_authority = if fresh_mobile_anchor || fresh_fixed_header {
            let owner = payload
                .string_at("/sessionId")
                .ok_or_else(|| FsError::Io("session metadata has no session identity".into()))?;
            let owner_id = uuid::Uuid::parse_str(owner)
                .map_err(|_| FsError::Io("session metadata has an invalid identity".into()))?;
            if transcript_relative
                .file_stem()
                .and_then(|part| part.to_str())
                .and_then(|part| uuid::Uuid::parse_str(part).ok())
                != Some(owner_id)
            {
                return Err(FsError::Io(
                    "session metadata has a different transcript identity".into(),
                )
                .into());
            }
            lingxi_core::host::rooted_fs::validate_open_file_single_link(&file)?;
            let file_identity = lingxi_core::host::rooted_fs::opened_file_identity(&file)?;
            let authority = TranscriptFileAuthority::from_capture(
                transcript_root.to_path_buf(),
                transcript_relative.to_path_buf(),
                *transcript_identity,
                file_identity,
            )?;
            let source = authority.read_checked()?;
            if fresh_fixed_header {
                if !source.is_empty() {
                    return Err(FsError::Io("new session transcript is not empty".into()).into());
                }
                let header =
                    std::str::from_utf8(&line).map_err(|error| FsError::Io(error.to_string()))?;
                crate::JsonlWriter::fixed_session_profile_from_source(header, owner)
                    .map_err(FsError::Io)?;
            } else {
                if !source.ends_with(b"\n") {
                    return Err(
                        FsError::Io("transcript has an incomplete final record".into()).into(),
                    );
                }
                let source =
                    std::str::from_utf8(&source).map_err(|error| FsError::Io(error.to_string()))?;
                crate::JsonlWriter::fixed_session_profile_from_source(source, owner)
                    .map_err(FsError::Io)?;
            }
            Some(authority)
        } else {
            None
        };
        // A cwd transition must be durable before a host publishes the new
        // working directory; otherwise a cold resume can restore the old cwd.
        let relocation = payload.string_at("/type") == Some("relocated");
        let previous_len = if fixed_mode || mobile_anchor || relocation || force_sync {
            Some(
                file.metadata()
                    .map_err(|error| FsError::Io(error.to_string()))?
                    .len(),
            )
        } else {
            None
        };
        self.writer.append_complete_line(&mut file, &line)?;
        if let Some(previous_len) = previous_len {
            #[cfg(test)]
            let injected_error = self
                .writer
                .fail_next_append_sync
                .swap(false, std::sync::atomic::Ordering::SeqCst)
                .then(|| std::io::Error::other("synthetic session mode sync failure"));
            #[cfg(not(test))]
            let injected_error: Option<std::io::Error> = None;
            let sync = if let Some(error) = injected_error {
                Err(error)
            } else {
                #[cfg(test)]
                self.writer
                    .syncs
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                file.sync_all()
            };
            if let Err(error) = sync {
                // The record was not acknowledged. While still holding the
                // transaction, attempt to remove its complete line.
                return Err(self.writer.rollback_undurable_append(
                    &mut file,
                    previous_len,
                    error.to_string(),
                ));
            }
        }
        // A failed first header attempt can leave an empty created file. The
        // successful retry must still sync its directory before publishing
        // the new owner's authority.
        if !file_present || fresh_fixed_header {
            #[cfg(test)]
            self.writer
                .syncs
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Err(error) = sync_parent_pinned(
                transcript_root,
                transcript_relative,
                Some(transcript_identity),
            ) {
                if let Some(previous_len) = previous_len {
                    return Err(self.writer.rollback_undurable_append(
                        &mut file,
                        previous_len,
                        error.to_string(),
                    ));
                }
                return Err(error.into());
            }
        }
        if let Some(authority) = pending_authority {
            // Publish the identity obtained from the original opened file only
            // after its marker is durable. A replaced path is an uncertain
            // write, never an opportunity to recapture the replacement.
            self.writer
                .bind_transcript_authority(
                    &transcript_root.join(transcript_relative),
                    Some(authority),
                )
                .map_err(TranscriptWriterError::WrittenButNotDurable)?;
        }
        self.writer
            .confirm_authoritative_target_after_write(transcript_root, transcript_relative)?;
        Ok(())
    }

    /// Remove a Native tombstone UUID while the durable transcript lock is
    /// held. The 64 KiB tail path keeps the normal target search bounded; if
    /// the target is elsewhere, full inspection is limited to 50 MiB. Both
    /// paths publish with an atomic replacement, so an interrupted copy never
    /// leaves the original inode truncated. Surviving `parentUuid` fields are
    /// left byte-for-byte unchanged.
    pub fn remove_message_by_uuid_at(
        &self,
        transcript_root: &Path,
        transcript_identity: &RootIdentity,
        transcript_relative: &Path,
        message_uuid: &str,
    ) -> Result<bool, TranscriptWriterError> {
        self.writer.ensure_rewrite_is_resolved()?;
        let result = self.remove_message_by_uuid_at_inner(
            transcript_root,
            transcript_identity,
            transcript_relative,
            message_uuid,
        );
        if matches!(&result, Err(TranscriptWriterError::WrittenButNotDurable(_))) {
            // Keep the fence inside the transaction even for callers using
            // DurableTranscriptTransaction directly, before its lock drops.
            self.writer.mark_rewrite_uncertain();
        }
        result
    }

    fn remove_message_by_uuid_at_inner(
        &self,
        transcript_root: &Path,
        transcript_identity: &RootIdentity,
        transcript_relative: &Path,
        message_uuid: &str,
    ) -> Result<bool, TranscriptWriterError> {
        let admitted = self
            .writer
            .open_authoritative_target(transcript_root, transcript_relative)?;
        let guarded = admitted.is_some();
        let mut file = match admitted.map(Ok).unwrap_or_else(|| {
            open_read_file_pinned(
                transcript_root,
                transcript_relative,
                Some(transcript_identity),
            )
        }) {
            Ok(file) => file,
            Err(FsError::NotFound(_)) => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        let file_len = file
            .metadata()
            .map_err(|error| FsError::Io(error.to_string()))?
            .len();
        if file_len == 0 {
            return Ok(false);
        }
        let original_authority = self.writer.transcript_authority();
        let original_revision = original_authority
            .as_ref()
            .map(TranscriptFileAuthority::digest_checked)
            .transpose()?;

        let tail_len = file_len.min(TOMBSTONE_TAIL_BYTES);
        let tail_start = file_len - tail_len;
        let tail_len_usize = usize::try_from(tail_len)
            .map_err(|_| FsError::Io("transcript tail exceeds addressable memory".into()))?;
        let mut tail = vec![0; tail_len_usize];
        file.seek(SeekFrom::Start(tail_start))
            .map_err(|error| FsError::Io(error.to_string()))?;
        file.read_exact(&mut tail)
            .map_err(|error| FsError::Io(error.to_string()))?;
        let mut offset = 0usize;
        let mut tail_match = None;
        for line in tail.split_inclusive(|byte| *byte == b'\n') {
            let line_start = offset;
            offset += line.len();
            // The first fragment can begin halfway through a record. It is
            // eligible only when the bounded scan started at the file head.
            if line_start == 0 && tail_start != 0 {
                continue;
            }
            if transcript_line_has_uuid(line, message_uuid) {
                tail_match = Some(line_start..offset);
                break;
            }
        }

        if tail_match.is_none() && file_len > TOMBSTONE_REWRITE_LIMIT_BYTES {
            tracing::warn!(
                bytes = file_len,
                message_uuid,
                "skipping transcript tombstone removal because the target is outside the tail window of a large session file"
            );
            return Ok(false);
        }

        if let Some(range) = tail_match {
            let line_start = tail_start
                .checked_add(range.start as u64)
                .ok_or_else(|| FsError::Io("transcript tail offset overflow".into()))?;
            let line_end = tail_start
                .checked_add(range.end as u64)
                .ok_or_else(|| FsError::Io("transcript tail offset overflow".into()))?;
            let expected_digest = if guarded {
                let mut hasher = Sha256::new();
                for (start, length) in [(0, line_start), (line_end, file_len - line_end)] {
                    file.seek(SeekFrom::Start(start))
                        .map_err(|error| FsError::Io(error.to_string()))?;
                    let mut remaining = length;
                    let mut buffer = [0u8; 64 * 1024];
                    while remaining > 0 {
                        let chunk = remaining.min(buffer.len() as u64) as usize;
                        let count = file
                            .read(&mut buffer[..chunk])
                            .map_err(|error| FsError::Io(error.to_string()))?;
                        if count == 0 {
                            return Err(
                                FsError::Io("transcript changed during rewrite".into()).into()
                            );
                        }
                        hasher.update(&buffer[..count]);
                        remaining -= count as u64;
                    }
                }
                let _current = self
                    .writer
                    .open_authoritative_target(transcript_root, transcript_relative)?;
                Some(hasher.finalize().into())
            } else {
                None
            };
            let mut published_identity = None;
            atomic_write_stream_pinned_checked(
                transcript_root,
                transcript_relative,
                AtomicWriteOptions {
                    overwrite: true,
                    create_parents: false,
                    dir_mode: 0o700,
                    file_mode: 0o600,
                },
                transcript_identity,
                |temporary| {
                    published_identity = Some(lingxi_core::host::rooted_fs::opened_file_identity(
                        temporary,
                    )?);
                    file.seek(SeekFrom::Start(0))
                        .map_err(|error| FsError::Io(error.to_string()))?;
                    let copied_prefix = {
                        let mut prefix = (&mut file).take(line_start);
                        std::io::copy(&mut prefix, temporary)
                            .map_err(|error| FsError::Io(error.to_string()))?
                    };
                    if copied_prefix != line_start {
                        return Err(FsError::Io(
                            "transcript changed while staging tombstone prefix".into(),
                        ));
                    }
                    file.seek(SeekFrom::Start(line_end))
                        .map_err(|error| FsError::Io(error.to_string()))?;
                    let copied_suffix = std::io::copy(&mut file, temporary)
                        .map_err(|error| FsError::Io(error.to_string()))?;
                    if copied_suffix != file_len.saturating_sub(line_end) {
                        return Err(FsError::Io(
                            "transcript changed while staging tombstone suffix".into(),
                        ));
                    }
                    Ok(())
                },
                || {
                    #[cfg(test)]
                    self.writer.wait_authority_publish_test_gate();
                    if let (Some(authority), Some(revision)) =
                        (&original_authority, &original_revision)
                    {
                        authority.verify_revision(revision)?;
                    }
                    Ok(())
                },
            )?;
            self.transfer_rewritten_target(
                transcript_root,
                transcript_relative,
                expected_digest,
                published_identity,
            )
            .map_err(|error| {
                TranscriptWriterError::WrittenButNotDurable(FsError::Io(error.to_string()))
            })?;
            return Ok(true);
        }

        let capacity = usize::try_from(file_len)
            .map_err(|_| FsError::Io("transcript file exceeds addressable memory".into()))?;
        let mut body = Vec::new();
        body.try_reserve_exact(capacity).map_err(|error| {
            FsError::Io(format!(
                "could not buffer transcript for tombstone: {error}"
            ))
        })?;
        file.seek(SeekFrom::Start(0))
            .map_err(|error| FsError::Io(error.to_string()))?;
        file.read_to_end(&mut body)
            .map_err(|error| FsError::Io(error.to_string()))?;

        let mut line_start = 0usize;
        let mut match_range = None;
        for line in body.split_inclusive(|byte| *byte == b'\n') {
            let line_end = line_start + line.len();
            if transcript_line_has_uuid(line, message_uuid) {
                match_range = Some((line_start, line_end));
                break;
            }
            line_start = line_end;
        }
        let Some((line_start, line_end)) = match_range else {
            return Ok(false);
        };
        body.copy_within(line_end.., line_start);
        body.truncate(body.len() - (line_end - line_start));
        let expected_digest = guarded.then(|| <[u8; 32]>::from(Sha256::digest(&body)));
        if guarded {
            let _current = self
                .writer
                .open_authoritative_target(transcript_root, transcript_relative)?;
        }
        drop(file);

        let mut published_identity = None;
        atomic_write_stream_pinned_checked(
            transcript_root,
            transcript_relative,
            AtomicWriteOptions {
                overwrite: true,
                create_parents: false,
                dir_mode: 0o700,
                file_mode: 0o600,
            },
            transcript_identity,
            |temporary| {
                published_identity = Some(lingxi_core::host::rooted_fs::opened_file_identity(
                    temporary,
                )?);
                temporary
                    .write_all(&body)
                    .map_err(|error| FsError::Io(error.to_string()))
            },
            || {
                #[cfg(test)]
                self.writer.wait_authority_publish_test_gate();
                if let (Some(authority), Some(revision)) = (&original_authority, &original_revision)
                {
                    authority.verify_revision(revision)?;
                }
                Ok(())
            },
        )?;
        self.transfer_rewritten_target(
            transcript_root,
            transcript_relative,
            expected_digest,
            published_identity,
        )
        .map_err(|error| {
            TranscriptWriterError::WrittenButNotDurable(FsError::Io(error.to_string()))
        })?;
        Ok(true)
    }

    /// Append under the writer's own pinned root without reacquiring the lock.
    pub fn append_json_once(
        &self,
        transcript_relative: &Path,
        delivery_id: &str,
        payload: Utf16JsonProjection,
    ) -> Result<TranscriptAppendOutcome, TranscriptWriterError> {
        self.append_json_once_at(
            &self.writer.root,
            &self.writer.identity,
            transcript_relative,
            delivery_id,
            payload,
        )
    }

    /// Append under an approved transcript root while retaining the one
    /// session-state transaction lock acquired by this guard.
    pub fn append_json_once_at(
        &self,
        transcript_root: &Path,
        transcript_identity: &RootIdentity,
        transcript_relative: &Path,
        delivery_id: &str,
        payload: Utf16JsonProjection,
    ) -> Result<TranscriptAppendOutcome, TranscriptWriterError> {
        self.append_json_once_at_with_tip(
            transcript_root,
            transcript_identity,
            transcript_relative,
            delivery_id,
            payload,
        )
        .map(|(outcome, _)| outcome)
    }

    /// Return whether this delivery is the durable UUID tip observed under the
    /// same transaction. Duplicate lookup and tip observation share one scan.
    pub fn append_json_once_at_with_tip(
        &self,
        transcript_root: &Path,
        transcript_identity: &RootIdentity,
        transcript_relative: &Path,
        delivery_id: &str,
        payload: Utf16JsonProjection,
    ) -> Result<(TranscriptAppendOutcome, bool), TranscriptWriterError> {
        self.writer.ensure_rewrite_is_resolved()?;
        self.writer.append_json_once_at_locked(
            transcript_root,
            transcript_identity,
            transcript_relative,
            delivery_id,
            payload,
            true,
            None,
        )
    }

    /// Append a delivery row and register its outer UUID in the Host identity
    /// sidecar under the same transaction. A Fusion delivery includes its
    /// private delivery id in the transcript row.
    pub(crate) fn append_json_once_at_with_tip_identity(
        &self,
        transcript_root: &Path,
        transcript_identity: &RootIdentity,
        transcript_relative: &Path,
        delivery_id: &str,
        payload: Utf16JsonProjection,
        identity_store: &IdentityLogStore,
        transcript_path: &Path,
    ) -> Result<(TranscriptAppendOutcome, bool), TranscriptWriterError> {
        self.writer.ensure_rewrite_is_resolved()?;
        self.writer.append_json_once_at_locked(
            transcript_root,
            transcript_identity,
            transcript_relative,
            delivery_id,
            payload,
            true,
            Some((identity_store, transcript_path)),
        )
    }

    /// Append one prepared Native UUID row and register that same identity in
    /// the Host sidecar under the transaction. The delivery key remains
    /// private and is not added to the JSONL payload.
    pub(crate) fn append_native_json_once_at_with_tip_identity(
        &self,
        transcript_root: &Path,
        transcript_identity: &RootIdentity,
        transcript_relative: &Path,
        delivery_id: &str,
        payload: Utf16JsonProjection,
        identity_store: &IdentityLogStore,
        transcript_path: &Path,
    ) -> Result<(TranscriptAppendOutcome, bool), TranscriptWriterError> {
        self.writer.ensure_rewrite_is_resolved()?;
        self.writer.append_json_once_at_locked(
            transcript_root,
            transcript_identity,
            transcript_relative,
            delivery_id,
            payload,
            false,
            Some((identity_store, transcript_path)),
        )
    }

    /// Append a pending user-prompt row once by its stable UUID. Duplicate
    /// comparison preserves every source field except the regenerated
    /// timestamp and transaction-derived parent UUID.
    pub(crate) fn append_pending_prompt_json_once_at_with_tip_identity(
        &self,
        transcript_root: &Path,
        transcript_identity: &RootIdentity,
        transcript_relative: &Path,
        message_uuid: &str,
        payload: Utf16JsonProjection,
        identity_store: &IdentityLogStore,
        transcript_path: &Path,
    ) -> Result<(TranscriptAppendOutcome, Option<String>), TranscriptWriterError> {
        self.writer.ensure_rewrite_is_resolved()?;
        if payload.string_at("/uuid") != Some(message_uuid) {
            return Err(TranscriptWriterError::DeliveryConflict {
                delivery_id: message_uuid.to_string(),
            });
        }
        self.writer.append_pending_prompt_json_once_at_locked(
            transcript_root,
            transcript_identity,
            transcript_relative,
            message_uuid,
            payload,
            Some((identity_store, transcript_path)),
        )
    }
}

fn transcript_line_has_uuid(line: &[u8], expected_uuid: &str) -> bool {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let Ok(line) = std::str::from_utf8(line) else {
        return false;
    };
    Utf16JsonProjection::parse(line)
        .ok()
        .and_then(|projection| {
            projection
                .string_at("/uuid")
                .map(|uuid| uuid == expected_uuid)
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fresh_fixed_header_binds_only_after_successful_durable_write() {
        use crate::jsonl::writer::JsonlWriter;
        use lingxi_core::host::{session_context::ConversationMode, FileSystem};
        use lingxi_core::types::SessionId;
        use std::sync::atomic::Ordering;

        for fail_sync in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let id = SessionId::new();
            let path = root.path().join(format!("{}.jsonl", id.as_uuid()));
            let durable = Arc::new(DurableTranscriptWriter::open(root.path()).unwrap());
            let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(
                root.path().to_path_buf(),
            ));
            let writer = JsonlWriter::new(path.clone(), fs).with_durable_lock(durable.clone());
            writer
                .activate_session_target(id, path.clone(), root.path().into())
                .unwrap();
            if fail_sync {
                durable.fail_next_append_sync.store(true, Ordering::SeqCst);
            } else {
                durable.fail_next_append_write.store(true, Ordering::SeqCst);
            }
            assert!(writer
                .initialize_fixed_session_mode(&id.as_uuid().to_string(), ConversationMode::Code, 0)
                .await
                .is_err());
            assert!(writer.active_transcript_authority().is_none());
            assert!(std::fs::read(&path).unwrap_or_default().is_empty());
            let before_retry = durable.sync_count_for_test();
            writer
                .initialize_fixed_session_mode(&id.as_uuid().to_string(), ConversationMode::Code, 0)
                .await
                .unwrap();
            assert_eq!(
                durable.sync_count_for_test() - before_retry,
                2,
                "a successful first header syncs both the file and its directory"
            );
            let authority = writer
                .active_transcript_authority()
                .expect("durable header binds authority");
            writer.verify_active_transcript_source(&authority).unwrap();
            let bytes = std::fs::read(&path).unwrap();
            std::fs::rename(&path, path.with_extension("old")).unwrap();
            std::fs::write(&path, &bytes).unwrap();
            assert!(writer.verify_active_transcript_source(&authority).is_err());
        }
    }

    #[tokio::test]
    async fn fresh_and_repeated_mobile_anchors_require_durable_success() {
        use crate::jsonl::writer::JsonlWriter;
        use lingxi_core::host::FileSystem;
        use lingxi_core::types::SessionId;
        use std::sync::atomic::Ordering;

        for fail_sync in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let id = SessionId::from_uuid(uuid::Uuid::new_v4());
            let path = root.path().join(format!("{}.jsonl", id.as_uuid()));
            let header = format!(
                "{}\n",
                json!({"type":"fixed-session-mode", "sessionId":id.as_uuid(), "sessionMode":"chat", "instructionEpoch":0})
            );
            std::fs::write(&path, &header).unwrap();
            let durable = Arc::new(DurableTranscriptWriter::open(root.path()).unwrap());
            let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(
                root.path().to_path_buf(),
            ));
            let writer = JsonlWriter::new(path.clone(), fs).with_durable_lock(durable.clone());
            writer
                .activate_session_target(id, path.clone(), root.path().into())
                .unwrap();
            if fail_sync {
                durable.fail_next_append_sync.store(true, Ordering::SeqCst);
            } else {
                durable.fail_next_append_write.store(true, Ordering::SeqCst);
            }
            assert!(writer
                .append_mobile_empty_session(&id.as_uuid().to_string(), "Draft", "/workspace")
                .await
                .is_err());
            assert!(writer.active_transcript_authority().is_none());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), header);
            writer
                .append_mobile_empty_session(&id.as_uuid().to_string(), "Draft", "/workspace")
                .await
                .unwrap();
            let authority = writer.active_transcript_authority().unwrap();
            writer.verify_active_transcript_source(&authority).unwrap();
            let before = std::fs::read(&path).unwrap();
            durable.fail_next_append_sync.store(true, Ordering::SeqCst);
            assert!(writer
                .append_mobile_empty_session(&id.as_uuid().to_string(), "Again", "/workspace")
                .await
                .is_err());
            assert_eq!(std::fs::read(&path).unwrap(), before);
            writer.verify_active_transcript_source(&authority).unwrap();
        }
    }

    #[tokio::test]
    async fn same_source_cwd_marker_write_and_sync_failures_do_not_publish_cwd() {
        use crate::jsonl::writer::JsonlWriter;
        use lingxi_core::host::FileSystem;
        use lingxi_core::types::SessionId;
        use std::sync::atomic::Ordering;

        for fail_sync in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let id = SessionId::from_uuid(uuid::Uuid::new_v4());
            let path = root.path().join(format!("{}.jsonl", id.as_uuid()));
            std::fs::write(&path, b"{\"type\":\"fixture\"}\n").unwrap();
            let before = std::fs::read(&path).unwrap();
            let durable = Arc::new(DurableTranscriptWriter::open(root.path()).unwrap());
            let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(
                root.path().to_path_buf(),
            ));
            let writer = JsonlWriter::new(path.clone(), fs).with_durable_lock(durable.clone());
            writer
                .activate_session_target(id, path.clone(), "/old".into())
                .unwrap();
            if fail_sync {
                durable.fail_next_append_sync.store(true, Ordering::SeqCst);
            } else {
                durable.fail_next_append_write.store(true, Ordering::SeqCst);
            }
            assert!(writer
                .retarget_with_relocation(path.clone(), &id.as_uuid().to_string(), "/new")
                .await
                .is_err());
            assert_eq!(writer.active_path(), path);
            assert_eq!(
                writer.session_target_cwd(id).as_deref(),
                Some(Path::new("/old"))
            );
            assert_eq!(std::fs::read(&path).unwrap(), before);
            writer
                .retarget_with_relocation(path.clone(), &id.as_uuid().to_string(), "/new")
                .await
                .unwrap();
            assert_eq!(
                writer.session_target_cwd(id).as_deref(),
                Some(Path::new("/new"))
            );
            assert!(std::fs::read_to_string(path)
                .unwrap()
                .contains("\"relocatedCwd\":\"/new\""));
        }
    }

    #[tokio::test]
    async fn exact_boundary_write_or_sync_failure_keeps_uncommitted_stage_out_of_cold_chain() {
        use crate::jsonl::{loader::build_conversation_chain, reader::JsonlReader};
        use platform_posix::fs::PosixFileSystem;
        use std::sync::Arc;
        use uuid::Uuid;

        for fail_sync in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let writer = DurableTranscriptWriter::open(dir.path()).unwrap();
            let path = Path::new("history.jsonl");
            let owner = Uuid::new_v4().to_string();
            let old = Uuid::new_v4().to_string();
            let summary = Uuid::new_v4().to_string();
            let boundary = Uuid::new_v4().to_string();
            let after = Uuid::new_v4().to_string();
            let base = |id: &str, parent: Option<&str>, sidechain: bool, timestamp: &str| {
                json!({"type":"user", "uuid":id, "parentUuid":parent, "sessionId":owner,
                    "timestamp":timestamp, "cwd":"/", "version":"test", "isSidechain":sidechain,
                    "message":{"role":"user", "content":id}})
            };
            writer
                .append_json_once(
                    path,
                    &old,
                    source_payload(base(&old, None, false, "2025-01-01T00:00:00Z")),
                )
                .unwrap();
            let mut stage = base(&summary, Some(&boundary), true, "2025-01-01T00:00:01Z");
            stage["nativeCompactStage"] = json!(boundary);
            stage["isCompactSummary"] = json!(true);
            writer
                .append_json_once(path, &summary, source_payload(stage))
                .unwrap();
            let before_boundary = std::fs::read(dir.path().join(path)).unwrap();
            let boundary_row = json!({"type":"system", "subtype":"compact_boundary", "uuid":boundary,
                "parentUuid":null, "sessionId":owner, "timestamp":"2025-01-01T00:00:02Z", "cwd":"/", "version":"test",
                "isSidechain":false, "compactMetadata":{"nativeExactPreserved":true,
                    "preservedSegment":{"headUuid":old,"tailUuid":old,"anchorUuid":summary},
                    "preservedMessages":{"anchorUuid":summary,"uuids":[old],"allUuids":[old]}}});
            if fail_sync {
                writer
                    .fail_next_append_sync
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            } else {
                writer
                    .fail_next_append_write
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            assert!(writer
                .append_json_once(path, &boundary, source_payload(boundary_row))
                .is_err());
            assert_eq!(
                std::fs::read(dir.path().join(path)).unwrap(),
                before_boundary
            );

            let read = || {
                JsonlReader::new(
                    dir.path().join(path),
                    Arc::new(PosixFileSystem::new(dir.path().to_path_buf())),
                )
            };
            let staged_only = read().read_routed().await.unwrap();
            assert_eq!(
                build_conversation_chain(&staged_only, &owner)
                    .0
                    .iter()
                    .map(|row| row.uuid.as_str())
                    .collect::<Vec<_>>(),
                vec![old.as_str()]
            );

            writer
                .append_json_once(
                    path,
                    &after,
                    source_payload(base(&after, Some(&old), false, "2025-01-01T00:00:03Z")),
                )
                .unwrap();
            drop(writer);
            let cold = read().read_routed().await.unwrap();
            assert_eq!(
                build_conversation_chain(&cold, &owner)
                    .0
                    .iter()
                    .map(|row| row.uuid.as_str())
                    .collect::<Vec<_>>(),
                vec![old.as_str(), after.as_str()]
            );
        }
    }

    #[test]
    fn disk_full_partial_append_preserves_committed_history_and_allows_retry() {
        for once in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let writer = DurableTranscriptWriter::open(dir.path()).unwrap();
            let path = Path::new("history.jsonl");
            writer
                .append_json_once(
                    path,
                    "saved",
                    source_payload(serde_json::json!({"uuid":"saved", "message":"history"})),
                )
                .unwrap();
            let before = std::fs::read(dir.path().join(path)).unwrap();
            let pending = source_payload(
                serde_json::json!({"uuid":"pending", "message":"retain this complete result"}),
            );
            writer
                .fail_next_append_write
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let append = || {
                if once {
                    writer
                        .append_json_once(path, "pending", pending.clone())
                        .map(|_| ())
                } else {
                    writer.with_transaction(|tx| {
                        tx.append_raw_json_at(dir.path(), &writer.identity, path, pending.clone())
                    })
                }
            };
            assert!(append().is_err());
            assert_eq!(std::fs::read(dir.path().join(path)).unwrap(), before);
            append().unwrap();
            let after = std::fs::read(dir.path().join(path)).unwrap();
            assert!(after.starts_with(&before));
            let rows = std::str::from_utf8(&after)
                .unwrap()
                .lines()
                .collect::<Vec<_>>();
            assert_eq!(rows.len(), 2);
            for row in rows {
                serde_json::from_str::<serde_json::Value>(row).unwrap();
            }
        }
    }
    use serde_json::json;

    fn source_payload(value: Value) -> Utf16JsonProjection {
        Utf16JsonProjection::from_serde_value(&value).unwrap()
    }

    #[test]
    fn native_source_written_failure_requires_durable_cold_retry_and_same_units() {
        let dir = tempfile::tempdir().unwrap();
        let writer = DurableTranscriptWriter::open(dir.path()).unwrap();
        let identity = root_identity(dir.path()).unwrap();
        let path = Path::new("native.jsonl");
        let transcript_path = dir.path().join(path);
        let identity_store = IdentityLogStore::default();
        message_identity::bootstrap_at(&identity_store, &transcript_path, dir.path(), &identity)
            .unwrap();
        let payload = Utf16JsonProjection::parse(
            r#"{"uuid":"native-peer","parentUuid":null,"origin":{"kind":"peer","body":"\ud83d"},"message":{"content":[{"type":"text","text":"\ud83d"}]}}"#,
        )
        .unwrap();
        writer
            .fail_next_append_sync
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(
            writer.with_transaction(|tx| tx.append_native_json_once_at_with_tip_identity(
                dir.path(),
                &identity,
                path,
                "private-delivery",
                payload.clone(),
                &identity_store,
                &transcript_path,
            )),
            Err(TranscriptWriterError::WrittenButNotDurable(_))
        ));
        let raw = std::fs::read_to_string(dir.path().join(path)).unwrap();
        assert_eq!(raw.matches("\\ud83d").count(), 2);
        assert!(!raw.contains("deliveryId"));
        drop(writer);
        let cold = DurableTranscriptWriter::open(dir.path()).unwrap();
        let cold_identity_store = IdentityLogStore::default();
        message_identity::bootstrap_at(
            &cold_identity_store,
            &transcript_path,
            dir.path(),
            &identity,
        )
        .unwrap();
        cold.fail_next_existing_sync_for_test();
        assert!(cold
            .with_transaction(|tx| tx.append_native_json_once_at_with_tip_identity(
                dir.path(),
                &identity,
                path,
                "private-delivery",
                payload.clone(),
                &cold_identity_store,
                &transcript_path,
            ))
            .is_err());
        assert_eq!(
            cold.with_transaction(|tx| tx.append_native_json_once_at_with_tip_identity(
                dir.path(),
                &identity,
                path,
                "private-delivery",
                payload.clone(),
                &cold_identity_store,
                &transcript_path,
            ))
            .unwrap(),
            (TranscriptAppendOutcome::AlreadyPresent, true)
        );
        let different = Utf16JsonProjection::parse(
            r#"{"uuid":"native-peer","parentUuid":null,"origin":{"kind":"peer","body":"\udc00"},"message":{"content":[{"type":"text","text":"\ud83d"}]}}"#,
        )
        .unwrap();
        assert!(matches!(
            cold.with_transaction(|tx| tx.append_native_json_once_at_with_tip_identity(
                dir.path(),
                &identity,
                path,
                "private-delivery",
                different,
                &cold_identity_store,
                &transcript_path,
            )),
            Err(TranscriptWriterError::DeliveryConflict { .. })
        ));
        assert_eq!(std::fs::read_to_string(dir.path().join(path)).unwrap(), raw);
    }

    #[test]
    fn fixed_mode_header_is_synced_and_sync_failure_is_not_committed() {
        let dir = tempfile::tempdir().unwrap();
        let writer = DurableTranscriptWriter::open(dir.path()).unwrap();
        let path = Path::new("mode.jsonl");
        let identity = root_identity(dir.path()).unwrap();
        let append = |payload: Value| {
            writer.with_transaction(|transaction| {
                transaction.append_raw_json_at(dir.path(), &identity, path, source_payload(payload))
            })
        };

        append(json!({"type":"tag", "tag":"ordinary"})).unwrap();
        assert_eq!(
            writer.sync_count_for_test(),
            1,
            "ordinary metadata only syncs the new directory entry"
        );
        append(json!({"type":"fixed-session-mode", "sessionMode":"chat", "instructionEpoch":1}))
            .unwrap();
        assert_eq!(
            writer.sync_count_for_test(),
            2,
            "a fixed mode header fsyncs its file"
        );
        let before = std::fs::read(dir.path().join(path)).unwrap();
        writer
            .fail_next_append_sync
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(
            append(
                json!({"type":"fixed-session-mode", "sessionMode":"code", "instructionEpoch":2})
            ),
            Err(TranscriptWriterError::WrittenButNotDurable(_))
        ));
        assert_eq!(
            std::fs::read(dir.path().join(path)).unwrap(),
            before,
            "an unacknowledged header cannot be replayed on resume"
        );
        append(json!({"type":"fixed-session-mode", "sessionMode":"code", "instructionEpoch":2}))
            .unwrap();
        assert_eq!(writer.sync_count_for_test(), 4);

        let fresh = Path::new("fresh-mode.jsonl");
        writer
            .with_transaction(|transaction| {
                transaction.append_raw_json_at(
                    dir.path(),
                    &identity,
                    fresh,
                    source_payload(
                        json!({"type":"fixed-session-mode", "sessionMode":"chat", "instructionEpoch":1}),
                    ),
                )
            })
            .unwrap();
        assert_eq!(
            writer.sync_count_for_test(),
            6,
            "a fresh mode header syncs both file and directory"
        );
    }

    /// Ordinary rows travel through the durable transaction for ORDERING, not
    /// for durability, and must not pay an fsync each. Only creating the file
    /// is worth a parent sync; a Fusion delivery, whose receipt claims the row
    /// is on disk, still pays for one.
    #[test]
    fn ordinary_rows_do_not_fsync_but_a_delivery_still_does() {
        let dir = tempfile::tempdir().unwrap();
        let writer = DurableTranscriptWriter::open(dir.path()).unwrap();
        let path = Path::new("transcript.jsonl");
        let identity = lingxi_core::host::rooted_fs::root_identity(dir.path()).unwrap();
        writer
            .with_transaction(|transaction| {
                transaction.append_raw_json_at(
                    dir.path(),
                    &identity,
                    path,
                    source_payload(json!({"n": 1})),
                )
            })
            .unwrap();
        // One parent sync for creating the file, and nothing else.
        assert_eq!(writer.sync_count_for_test(), 1);

        for n in 2..=5 {
            writer
                .with_transaction(|transaction| {
                    transaction.append_raw_json_at(
                        dir.path(),
                        &identity,
                        path,
                        source_payload(json!({"n": n})),
                    )
                })
                .unwrap();
        }
        assert_eq!(
            writer.sync_count_for_test(),
            1,
            "four more ordinary rows must not add a single fsync"
        );

        assert_eq!(
            writer
                .append_json_once(path, "delivery", source_payload(json!({"uuid": "fusion"})))
                .unwrap(),
            TranscriptAppendOutcome::Appended
        );
        assert_eq!(
            writer.sync_count_for_test(),
            2,
            "a delivery whose receipt claims durability pays for it"
        );
    }

    #[test]
    fn durable_tombstone_removes_one_row_atomically_without_reparenting_children() {
        let state = tempfile::tempdir().unwrap();
        let transcript = tempfile::tempdir().unwrap();
        let path = Path::new("transcript.jsonl");
        let transcript_path = transcript.path().join(path);
        let root = "{\"type\":\"user\",\"uuid\":\"root\",\"parentUuid\":null}\n";
        let removed = "{\"type\":\"assistant\",\"uuid\":\"removed\",\"parentUuid\":\"root\"}\n";
        let child = "{\"type\":\"assistant\",\"uuid\":\"child\",\"parentUuid\":\"removed\"}\n";
        let source = format!("{root}{removed}{child}");
        std::fs::write(&transcript_path, &source).unwrap();
        let identity = root_identity(transcript.path()).unwrap();
        let writer = DurableTranscriptWriter::open(state.path()).unwrap();

        assert!(writer
            .remove_message_by_uuid_at(transcript.path(), &identity, path, "removed")
            .unwrap());
        assert_eq!(
            std::fs::read_to_string(&transcript_path).unwrap(),
            format!("{root}{child}")
        );
        let persisted_child: Value = serde_json::from_str(
            std::fs::read_to_string(&transcript_path)
                .unwrap()
                .lines()
                .nth(1)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(persisted_child["parentUuid"], "removed");
    }

    #[test]
    fn durable_tombstone_holds_the_shared_transcript_transaction() {
        let state = tempfile::tempdir().unwrap();
        let transcript = tempfile::tempdir().unwrap();
        let path = Path::new("transcript.jsonl");
        let transcript_path = transcript.path().join(path);
        std::fs::write(
            &transcript_path,
            "{\"type\":\"assistant\",\"uuid\":\"target\",\"parentUuid\":null}\n",
        )
        .unwrap();
        let identity = root_identity(transcript.path()).unwrap();
        let writer = DurableTranscriptWriter::open(state.path()).unwrap();
        let transaction = writer.begin_transaction().unwrap();
        let worker_writer = writer.clone();
        let transcript_root = transcript.path().to_path_buf();
        let identity_for_worker = identity;
        let path_for_worker = path.to_path_buf();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let result = worker_writer.remove_message_by_uuid_at(
                &transcript_root,
                &identity_for_worker,
                &path_for_worker,
                "target",
            );
            finished_tx.send(result).unwrap();
        });

        started_rx.recv().unwrap();
        assert!(finished_rx
            .recv_timeout(std::time::Duration::from_millis(30))
            .is_err());
        drop(transaction);
        assert!(finished_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
            .unwrap());
        worker.join().unwrap();
        assert_eq!(std::fs::metadata(transcript_path).unwrap().len(), 0);
    }

    #[test]
    fn uncertain_tombstone_fences_the_existing_transaction_and_future_writers() {
        let state = tempfile::tempdir().unwrap();
        let transcript = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(transcript.path()).unwrap();
        let relative = PathBuf::from("transcript.jsonl");
        let path = root.join(&relative);
        std::fs::write(
            &path,
            "{\"type\":\"user\",\"uuid\":\"keep\"}\n{\"type\":\"assistant\",\"uuid\":\"remove\"}\n",
        )
        .unwrap();
        let (authority, _) =
            TranscriptFileAuthority::capture(root.clone(), relative.clone()).unwrap();
        let identity = root_identity(&root).unwrap();
        let writer = DurableTranscriptWriter::open(state.path()).unwrap();
        writer
            .bind_transcript_authority(&path, Some(authority))
            .unwrap();
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        writer.install_authority_rewrite_test_gate((reached_tx, release_rx));
        let worker = std::thread::spawn({
            let writer = writer.clone();
            let root = root.clone();
            let relative = relative.clone();
            move || {
                let transaction = writer.begin_transaction().unwrap();
                let rewrite =
                    transaction.remove_message_by_uuid_at(&root, &identity, &relative, "remove");
                let raw = transaction.append_raw_json_at(
                    &root,
                    &identity,
                    &relative,
                    source_payload(serde_json::json!({"type":"agent-name","agentName":"later"})),
                );
                let once = transaction.append_json_once_at(
                    &root,
                    &identity,
                    &relative,
                    "delivery",
                    source_payload(serde_json::json!({"type":"user","uuid":"later"})),
                );
                (rewrite, raw, once)
            }
        });
        reached_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let published = std::fs::read(&path).unwrap();
        let saved = path.with_extension("saved");
        std::fs::rename(&path, &saved).unwrap();
        std::fs::write(&path, &published).unwrap();
        release_tx.send(()).unwrap();
        let (rewrite, raw, once) = worker.join().unwrap();
        assert!(matches!(
            rewrite,
            Err(TranscriptWriterError::WrittenButNotDurable(_))
        ));
        assert!(matches!(
            raw,
            Err(TranscriptWriterError::WrittenButNotDurable(_))
        ));
        assert!(matches!(
            once,
            Err(TranscriptWriterError::WrittenButNotDurable(_))
        ));
        assert!(matches!(
            writer.begin_transaction(),
            Err(TranscriptWriterError::WrittenButNotDurable(_))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), published);
    }

    #[test]
    fn durable_tombstone_rejects_a_replaced_transcript_root_without_touching_it() {
        let state = tempfile::tempdir().unwrap();
        let transcript = tempfile::tempdir().unwrap();
        let wrong_root = tempfile::tempdir().unwrap();
        let path = Path::new("transcript.jsonl");
        let transcript_path = transcript.path().join(path);
        let source = "{\"type\":\"assistant\",\"uuid\":\"target\",\"parentUuid\":null}\n";
        std::fs::write(&transcript_path, source).unwrap();
        let wrong_identity = root_identity(wrong_root.path()).unwrap();
        let writer = DurableTranscriptWriter::open(state.path()).unwrap();

        assert!(writer
            .remove_message_by_uuid_at(transcript.path(), &wrong_identity, path, "target")
            .is_err());
        assert_eq!(std::fs::read_to_string(transcript_path).unwrap(), source);
    }

    #[test]
    fn retry_after_written_row_sync_failure_reports_whether_duplicate_is_still_tip() {
        let dir = tempfile::tempdir().unwrap();
        let writer = DurableTranscriptWriter::open(dir.path()).unwrap();
        let path = Path::new("transcript.jsonl");
        let payload = source_payload(json!({"uuid":"fusion", "text":"answer"}));
        writer
            .fail_next_append_sync
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(
            writer.append_json_once(path, "delivery", payload.clone()),
            Err(TranscriptWriterError::WrittenButNotDurable(FsError::Io(message)))
                if message.contains("written transcript sync failure")
        ));
        assert_eq!(
            std::fs::read_to_string(dir.path().join(path))
                .unwrap()
                .lines()
                .count(),
            1
        );
        let retry = || {
            writer
                .begin_transaction()
                .unwrap()
                .append_json_once_at_with_tip(
                    dir.path(),
                    &writer.identity,
                    path,
                    "delivery",
                    payload.clone(),
                )
                .unwrap()
        };
        assert_eq!(retry(), (TranscriptAppendOutcome::AlreadyPresent, true));
        writer
            .append_json_once(
                path,
                "later",
                source_payload(json!({"uuid":"later", "text":"next"})),
            )
            .unwrap();
        assert_eq!(retry(), (TranscriptAppendOutcome::AlreadyPresent, false));
    }

    #[test]
    fn large_ordinary_image_preserves_parent_and_fusion_idempotency() {
        let dir = tempfile::tempdir().unwrap();
        let writer = DurableTranscriptWriter::open(dir.path())
            .unwrap()
            .with_max_scan_bytes(512);
        let path = Path::new("transcript.jsonl");
        writer.begin_transaction().unwrap().append_raw_json_at(
            dir.path(), &writer.identity, path,
            source_payload(json!({"uuid":"image", "message":{"content":[{"type":"image","data":"A".repeat(64 * 1024)}]}})),
        ).unwrap();
        let payload = source_payload(json!({"uuid":"fusion", "text":"answer", "parentUuid":null}));
        assert_eq!(
            writer
                .append_json_once(path, "delivery", payload.clone())
                .unwrap(),
            TranscriptAppendOutcome::Appended
        );
        assert_eq!(
            writer.append_json_once(path, "delivery", payload).unwrap(),
            TranscriptAppendOutcome::AlreadyPresent
        );
        let lines = std::fs::read_to_string(dir.path().join(path)).unwrap();
        let rows: Vec<Value> = lines
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1]["parentUuid"], "image");
    }

    #[test]
    fn oversized_history_still_rejects_corruption_and_identity_collisions() {
        let dir = tempfile::tempdir().unwrap();
        let writer = DurableTranscriptWriter::open(dir.path())
            .unwrap()
            .with_max_scan_bytes(128);
        let path = Path::new("transcript.jsonl");
        let original =
            serde_json::to_vec(&json!({"uuid":"fusion", "text":"x".repeat(4096)})).unwrap();
        std::fs::write(dir.path().join(path), &original).unwrap();
        assert!(matches!(
            writer.append_json_once(
                path,
                "delivery",
                source_payload(json!({"uuid":"fusion","text":"answer"})),
            ),
            Err(TranscriptWriterError::DeliveryConflict { .. })
        ));
        let mut broken = original;
        broken.pop();
        std::fs::write(dir.path().join(path), &broken).unwrap();
        assert!(matches!(
            writer.append_json_once(
                path,
                "delivery",
                source_payload(json!({"uuid":"other","text":"answer"})),
            ),
            Err(TranscriptWriterError::CorruptLine { .. })
        ));
    }

    #[test]
    fn oversized_streaming_scan_bounds_metadata_and_validates_ignored_strings() {
        let dir = tempfile::tempdir().unwrap();
        let writer = DurableTranscriptWriter::open(dir.path())
            .unwrap()
            .with_max_scan_bytes(256);
        let path = Path::new("transcript.jsonl");
        let prefix = format!("{{\"uuid\":\"image\",\"body\":\"{}", "a".repeat(1024));
        let mut cases = vec![
            format!("{prefix}\\uQQQQ\"}}").into_bytes(),
            format!("{prefix}\\uD80\"}}").into_bytes(),
            format!("{prefix}\\q\"}}").into_bytes(),
            format!("{{\"uuid\":\"{}\",\"body\":0}}", "a".repeat(1024)).into_bytes(),
            format!("{{\"{}\":0}}", "k".repeat(1024)).into_bytes(),
            format!("{{\"body\":{}0{}}}", "[".repeat(256), "]".repeat(256)).into_bytes(),
        ];
        let mut invalid_utf8 = prefix.as_bytes().to_vec();
        invalid_utf8.extend_from_slice(&[0xff, b'"', b'}']);
        cases.push(invalid_utf8);
        for bytes in cases {
            std::fs::write(dir.path().join(path), &bytes).unwrap();
            assert!(matches!(
                writer.append_json_once(path, "d", source_payload(json!({"uuid":"f"}))),
                Err(TranscriptWriterError::CorruptLine { .. })
            ));
            assert_eq!(std::fs::read(dir.path().join(path)).unwrap(), bytes);
        }
        // JSON strings admit escaped unmatched UTF-16 as well as scalar text.
        for suffix in ["你好\\uD83D\\uDE00", "\\uD800", "\\uDC00", "\\uD800\\u0041"] {
            std::fs::write(dir.path().join(path), format!("{prefix}{suffix}\"}}")).unwrap();
            assert_eq!(
                writer
                    .append_json_once(path, "d", source_payload(json!({"uuid":"f"})))
                    .unwrap(),
                TranscriptAppendOutcome::Appended
            );
        }
    }

    #[test]
    fn stable_delivery_id_is_append_once() {
        let dir = tempfile::tempdir().unwrap();
        let writer = DurableTranscriptWriter::open(dir.path()).unwrap();
        let path = Path::new("transcript.jsonl");
        assert_eq!(
            writer
                .append_json_once(
                    path,
                    "delivery-1",
                    source_payload(json!({"uuid":"message-1","text":"ok"})),
                )
                .unwrap(),
            TranscriptAppendOutcome::Appended
        );
        assert_eq!(
            writer
                .append_json_once(
                    path,
                    "delivery-1",
                    source_payload(json!({"uuid":"message-1","text":"ok"})),
                )
                .unwrap(),
            TranscriptAppendOutcome::AlreadyPresent
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join(path))
                .unwrap()
                .lines()
                .count(),
            1
        );
    }

    #[test]
    fn identical_match_is_not_acknowledged_when_its_durability_sync_fails() {
        let dir = tempfile::tempdir().unwrap();
        let writer = DurableTranscriptWriter::open(dir.path()).unwrap();
        let path = Path::new("transcript.jsonl");
        let payload = source_payload(json!({"uuid":"message-1","text":"ok"}));
        assert_eq!(
            writer
                .append_json_once(path, "delivery-1", payload.clone())
                .unwrap(),
            TranscriptAppendOutcome::Appended
        );

        writer.fail_next_existing_sync_for_test();
        assert!(matches!(
            writer.append_json_once(path, "delivery-1", payload.clone()),
            Err(TranscriptWriterError::Fs(FsError::Io(message)))
                if message.contains("existing transcript sync failure")
        ));

        assert_eq!(
            writer
                .append_json_once(path, "delivery-1", payload)
                .unwrap(),
            TranscriptAppendOutcome::AlreadyPresent
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join(path))
                .unwrap()
                .lines()
                .count(),
            1
        );
    }

    #[test]
    fn conflicting_delivery_id_fails_without_second_line() {
        let dir = tempfile::tempdir().unwrap();
        let writer = DurableTranscriptWriter::open(dir.path()).unwrap();
        let path = Path::new("transcript.jsonl");
        writer
            .append_json_once(
                path,
                "delivery-1",
                source_payload(json!({"uuid":"message-1","text":"ok"})),
            )
            .unwrap();
        assert!(matches!(
            writer.append_json_once(
                path,
                "delivery-2",
                source_payload(json!({"uuid":"message-1","text":"different"})),
            ),
            Err(TranscriptWriterError::DeliveryConflict { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(dir.path().join(path))
                .unwrap()
                .lines()
                .count(),
            1
        );
    }

    #[test]
    fn delivery_id_cannot_be_reused_for_a_different_message_uuid() {
        let dir = tempfile::tempdir().unwrap();
        let writer = DurableTranscriptWriter::open(dir.path()).unwrap();
        let path = Path::new("transcript.jsonl");
        writer
            .append_json_once(
                path,
                "delivery-1",
                source_payload(json!({"uuid":"message-1","text":"ok"})),
            )
            .unwrap();

        assert!(matches!(
            writer.append_json_once(
                path,
                "delivery-1",
                source_payload(json!({"uuid":"message-2","text":"ok"})),
            ),
            Err(TranscriptWriterError::DeliveryConflict { delivery_id })
                if delivery_id == "delivery-1"
        ));
        assert_eq!(
            std::fs::read_to_string(dir.path().join(path))
                .unwrap()
                .lines()
                .count(),
            1
        );
    }

    #[test]
    fn message_uuid_cannot_be_reused_for_a_different_delivery_id() {
        let dir = tempfile::tempdir().unwrap();
        let writer = DurableTranscriptWriter::open(dir.path()).unwrap();
        let path = Path::new("transcript.jsonl");
        writer
            .append_json_once(
                path,
                "delivery-1",
                source_payload(json!({"uuid":"message-1","text":"ok"})),
            )
            .unwrap();

        assert!(matches!(
            writer.append_json_once(
                path,
                "delivery-2",
                source_payload(json!({"uuid":"message-1","text":"ok"})),
            ),
            Err(TranscriptWriterError::DeliveryConflict { delivery_id })
                if delivery_id == "delivery-2"
        ));
        assert_eq!(
            std::fs::read_to_string(dir.path().join(path))
                .unwrap()
                .lines()
                .count(),
            1
        );
    }

    #[test]
    fn append_once_keeps_native_uuid_and_fusion_delivery_identities_distinct() {
        let dir = tempfile::tempdir().unwrap();
        let writer = DurableTranscriptWriter::open(dir.path()).unwrap();
        let path = Path::new("transcript.jsonl");
        let mut existing = serde_json::to_vec(&json!({"uuid":"message-1","text":"ok"})).unwrap();
        existing.push(b'\n');
        std::fs::write(dir.path().join(path), existing).unwrap();
        let identity = root_identity(dir.path()).unwrap();
        let transcript_path = dir.path().join(path);
        let identity_store = IdentityLogStore::default();
        message_identity::bootstrap_at(&identity_store, &transcript_path, dir.path(), &identity)
            .unwrap();

        let payload = source_payload(json!({"uuid":"message-1","text":"ok"}));
        assert!(matches!(
            writer.append_json_once(path, "delivery-1", payload.clone()),
            Err(TranscriptWriterError::DeliveryConflict { .. })
        ));
        // A current native UUID-only append still retries its own row.
        assert_eq!(
            writer
                .with_transaction(|tx| tx.append_native_json_once_at_with_tip_identity(
                    dir.path(),
                    &identity,
                    path,
                    "private-delivery",
                    payload.clone(),
                    &identity_store,
                    &transcript_path,
                ))
                .unwrap(),
            (TranscriptAppendOutcome::AlreadyPresent, true)
        );

        let mut invalid_native = payload.clone();
        invalid_native
            .set_field("deliveryId", Utf16JsonProjection::parse("null").unwrap())
            .unwrap();
        assert!(matches!(
            writer.with_transaction(|tx| tx.append_native_json_once_at_with_tip_identity(
                dir.path(),
                &identity,
                path,
                "private-delivery",
                invalid_native.clone(),
                &identity_store,
                &transcript_path,
            )),
            Err(TranscriptWriterError::NativeDeliveryIdField)
        ));
        let invalid_line = invalid_native.to_json_string().unwrap();
        std::fs::write(dir.path().join(path), format!("{invalid_line}\n")).unwrap();
        assert!(matches!(
            writer.with_transaction(|tx| tx.append_native_json_once_at_with_tip_identity(
                dir.path(),
                &identity,
                path,
                "private-delivery",
                payload.clone(),
                &identity_store,
                &transcript_path,
            )),
            Err(TranscriptWriterError::DeliveryConflict { .. })
        ));

        let fusion_path = Path::new("fusion.jsonl");
        let fusion_transcript_path = dir.path().join(fusion_path);
        writer
            .append_json_once(fusion_path, "delivery-1", payload.clone())
            .unwrap();
        assert!(matches!(
            writer.with_transaction(|tx| tx.append_native_json_once_at_with_tip_identity(
                dir.path(),
                &identity,
                fusion_path,
                "private-delivery",
                payload,
                &identity_store,
                &fusion_transcript_path,
            )),
            Err(TranscriptWriterError::DeliveryConflict { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(dir.path().join(path))
                .unwrap()
                .lines()
                .count(),
            1
        );
    }

    #[test]
    fn one_transaction_resolves_target_and_appends_without_recursive_locking() {
        let state_dir = tempfile::tempdir().unwrap();
        let transcript_dir = tempfile::tempdir().unwrap();
        let writer = DurableTranscriptWriter::open(state_dir.path()).unwrap();
        let path = Path::new("transcript.jsonl");

        let first = writer
            .with_transaction(|transaction| {
                let transcript_identity = root_identity(transcript_dir.path())?;
                transaction.append_json_once_at(
                    transcript_dir.path(),
                    &transcript_identity,
                    path,
                    "delivery-1",
                    source_payload(json!({"uuid":"message-1","text":"ok"})),
                )
            })
            .unwrap();
        assert_eq!(first, TranscriptAppendOutcome::Appended);

        let duplicate = writer
            .with_transaction(|transaction| {
                let transcript_identity = root_identity(transcript_dir.path())?;
                transaction.append_json_once_at(
                    transcript_dir.path(),
                    &transcript_identity,
                    path,
                    "delivery-1",
                    source_payload(json!({"uuid":"message-1","text":"ok"})),
                )
            })
            .unwrap();
        assert_eq!(duplicate, TranscriptAppendOutcome::AlreadyPresent);
        assert_eq!(
            std::fs::read_to_string(transcript_dir.path().join(path))
                .unwrap()
                .lines()
                .count(),
            1
        );
    }

    #[test]
    fn parent_uuid_is_resolved_under_the_transaction_and_reused_on_retry() {
        let state_dir = tempfile::tempdir().unwrap();
        let transcript_dir = tempfile::tempdir().unwrap();
        let writer = DurableTranscriptWriter::open(state_dir.path()).unwrap();
        let identity = root_identity(transcript_dir.path()).unwrap();
        let path = Path::new("transcript.jsonl");

        writer
            .with_transaction(|transaction| {
                transaction.append_json_once_at(
                    transcript_dir.path(),
                    &identity,
                    path,
                    "delivery-1",
                    source_payload(json!({"uuid":"message-1", "parentUuid": null, "type":"user"})),
                )?;
                transaction.append_json_once_at(
                    transcript_dir.path(),
                    &identity,
                    path,
                    "delivery-2",
                    source_payload(json!({"uuid":"message-2", "parentUuid": null, "type":"user"})),
                )
            })
            .unwrap();

        let rows = std::fs::read_to_string(transcript_dir.path().join(path))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(rows[0]["parentUuid"], Value::Null);
        assert_eq!(rows[1]["parentUuid"], Value::String("message-1".into()));

        assert_eq!(
            writer
                .append_json_once_at(
                    transcript_dir.path(),
                    &identity,
                    path,
                    "delivery-2",
                    source_payload(json!({"uuid":"message-2", "parentUuid": null, "type":"user"})),
                )
                .unwrap(),
            TranscriptAppendOutcome::AlreadyPresent
        );
        assert_eq!(
            std::fs::read_to_string(transcript_dir.path().join(path))
                .unwrap()
                .lines()
                .count(),
            2
        );
    }

    #[test]
    fn long_transcript_is_not_rejected_when_each_record_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let writer = DurableTranscriptWriter::open(dir.path())
            .unwrap()
            .with_max_scan_bytes(128);
        let path = Path::new("transcript.jsonl");
        let mut existing = std::fs::File::create(dir.path().join(path)).unwrap();
        for index in 0..200_u32 {
            serde_json::to_writer(
                &mut existing,
                &json!({"uuid": format!("old-{index}"), "text":"ok"}),
            )
            .unwrap();
            existing.write_all(b"\n").unwrap();
        }
        existing.sync_all().unwrap();
        assert!(std::fs::metadata(dir.path().join(path)).unwrap().len() > 128);

        assert_eq!(
            writer
                .append_json_once(
                    path,
                    "delivery-new",
                    source_payload(json!({"uuid":"message-new","text":"ok"})),
                )
                .unwrap(),
            TranscriptAppendOutcome::Appended
        );
    }

    #[cfg(unix)]
    #[test]
    fn transcript_read_errors_never_bypass_idempotency_scan() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let writer = DurableTranscriptWriter::open(dir.path()).unwrap();
        let path = Path::new("transcript.jsonl");
        symlink("missing-target", dir.path().join(path)).unwrap();

        assert!(matches!(
            writer.append_json_once(
                path,
                "delivery-1",
                source_payload(json!({"uuid":"message-1","text":"ok"})),
            ),
            Err(TranscriptWriterError::Fs(_))
        ));
    }
}
