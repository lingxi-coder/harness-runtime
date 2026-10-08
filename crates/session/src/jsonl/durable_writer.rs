//! Effectively-once transcript delivery under a session-state lock.
//!
//! `JsonlWriter` owns the session transcript targets. This writer supplies
//! their durable path for terminal/outbox records: it uses the
//! pinned session-state directory as the lock root, checks the stable delivery
//! id under that same lock, appends through the exact no-follow handle, and
//! fsyncs before acknowledging success.

use crate::jsonl::exact_json::{
    ExactJsonError, ExactJsonValue, Utf16Overrides,
};
use crate::jsonl::journal::{SESSION_STATE_DIR_MODE, SESSION_STATE_FILE_MODE};
use crate::jsonl::message_identity::{self, IdentityLogStore};
use lingxi_core::host::rooted_fs::{
    atomic_write_pinned, atomic_write_stream_pinned, lock_exclusive_pinned,
    open_append_file_pinned, open_read_file_pinned, root_identity, sync_parent_pinned,
    AtomicWriteOptions, RootIdentity,
};
use lingxi_core::host::FsError;
use serde_json::{Map, Value};
use std::cell::Cell;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Stable lock filename shared by ordinary transcript appends, `/cd`
/// relocation, and durable outbox delivery.
pub const TRANSCRIPT_LOCK_FILE_NAME: &str = "transcript.lock";
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

/// Durable transcript errors.  A conflict is never repaired by appending a
/// second interpretation of the same delivery id.
#[derive(Debug, Error)]
pub enum TranscriptWriterError {
    /// Exact JavaScript JSON string encoding failed.
    #[error(transparent)]
    ExactJson(#[from] ExactJsonError),
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

/// Root-pinned transcript append owner.
#[derive(Debug, Clone)]
pub struct DurableTranscriptWriter {
    root: PathBuf,
    identity: RootIdentity,
    max_record_bytes: usize,
    #[cfg(test)]
    fail_next_existing_sync: std::sync::Arc<std::sync::atomic::AtomicBool>,
    #[cfg(test)]
    fail_next_append_sync: std::sync::Arc<std::sync::atomic::AtomicBool>,
    #[cfg(test)]
    duplicate_scans: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Counts `fsync` calls made by this writer, so a regression can assert
    /// which appends pay for durability and which do not.
    #[cfg(test)]
    syncs: std::sync::Arc<std::sync::atomic::AtomicUsize>,
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
    exact: ExactJsonValue,
    keys: Vec<lingxi_core::types::utf16_json::Utf16JsonKey>,
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
            max_record_bytes: DEFAULT_MAX_TRANSCRIPT_SCAN_BYTES,
            #[cfg(test)]
            fail_next_existing_sync: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(test)]
            fail_next_append_sync: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(test)]
            syncs: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            duplicate_scans: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
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
        Ok(DurableTranscriptTransaction {
            writer: self,
            _lock: lock,
        })
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
        payload: Value,
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
        payload: Value,
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
        mut payload: Value,
        utf16_overrides: &Utf16Overrides,
        stamp_delivery_id: bool,
        identity_registration: Option<(&IdentityLogStore, &Path)>,
        projection: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>,
    ) -> Result<(TranscriptAppendOutcome, bool), TranscriptWriterError> {
        if !matches!(payload, Value::Object(_)) {
            return Err(TranscriptWriterError::PayloadNotObject);
        }
        if !stamp_delivery_id && payload.get("deliveryId").is_some() {
            return Err(TranscriptWriterError::NativeDeliveryIdField);
        }
        let message_uuid = payload
            .get("uuid")
            .and_then(Value::as_str)
            .filter(|uuid| !uuid.is_empty())
            .ok_or(TranscriptWriterError::MissingMessageUuid)?
            .to_string();
        if stamp_delivery_id {
            payload
                .as_object_mut()
                .expect("object payload checked")
                .insert(
                    "deliveryId".to_string(),
                    Value::String(delivery_id.to_string()),
                );
        }
        let (file_present, missing_final_delimiter, existing_match, last_uuid) =
            match open_read_file_pinned(
                transcript_root,
                transcript_relative,
                Some(transcript_identity),
            ) {
                Ok(file) => {
                    let (missing_delimiter, matching, last_uuid) = self.scan_existing(
                        file,
                        delivery_id,
                        &message_uuid,
                        &payload,
                        utf16_overrides,
                        projection,
                    )?;
                    (true, missing_delimiter, matching, last_uuid)
                }
                Err(FsError::NotFound(_)) => (false, false, None, None),
                Err(error) => return Err(error.into()),
            };
        if let Some(outcome) = existing_match {
            if outcome
                .as_ref()
                .is_ok_and(|value| *value == TranscriptAppendOutcome::AlreadyPresent)
            {
                // A complete line can be visible after a process died between
                // write and fsync. Never promote that observation to a durable
                // duplicate acknowledgement until both file and directory
                // have crossed the same persistence boundary as a new append.
                self.sync_existing_match(
                    transcript_root,
                    transcript_identity,
                    transcript_relative,
                )?;
            }
            return outcome
                .map(|outcome| (outcome, last_uuid.as_deref() == Some(message_uuid.as_str())));
        }
        // Parentage is part of the immutable transcript payload. Resolve it
        // only after the UUID duplicate check, while the same transaction is
        // held, so a retry can keep the original parent even after later
        // messages were appended.
        let needs_parent = payload.get("parentUuid").is_none_or(Value::is_null);
        if needs_parent {
            if let Value::Object(object) = &mut payload {
                object.insert(
                    "parentUuid".to_string(),
                    last_uuid.map_or(Value::Null, Value::String),
                );
            }
        }
        let mut line = crate::jsonl::exact_json::native_projection_bytes(&payload, utf16_overrides, projection)?;
        line.push(b'\n');
        if line.len() > self.max_record_bytes {
            return Err(TranscriptWriterError::ScanTooLarge {
                limit: self.max_record_bytes,
            });
        }
        if missing_final_delimiter {
            line.insert(0, b'\n');
        }
        if let Some((store, identity_path)) = identity_registration {
            message_identity::append_row_identity_at(
                store,
                identity_path,
                transcript_root,
                transcript_identity,
                &message_uuid,
            )?;
        }
        let mut file = open_append_file_pinned(
            transcript_root,
            transcript_relative,
            Some(transcript_identity),
        )?;
        file.write_all(&line)
            .map_err(|error| FsError::Io(error.to_string()))?;
        #[cfg(test)]
        if self
            .fail_next_append_sync
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(TranscriptWriterError::WrittenButNotDurable(FsError::Io(
                "synthetic written transcript sync failure".into(),
            )));
        }
        #[cfg(test)]
        self.syncs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        file.sync_all().map_err(|error| {
            TranscriptWriterError::WrittenButNotDurable(FsError::Io(error.to_string()))
        })?;
        if !file_present {
            #[cfg(test)]
            self.syncs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            sync_parent_pinned(
                transcript_root,
                transcript_relative,
                Some(transcript_identity),
            )
            .map_err(TranscriptWriterError::WrittenButNotDurable)?;
        }
        Ok((TranscriptAppendOutcome::Appended, true))
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
        payload: &Value,
        utf16_overrides: &Utf16Overrides,
        projection: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>,
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
        let encoded = crate::jsonl::exact_json::native_projection_bytes(payload, utf16_overrides, projection)?;
        let projected = lingxi_core::types::utf16_json::Utf16JsonProjection::parse(std::str::from_utf8(&encoded).expect("exact JSON encoder emits UTF-8"))
            .map_err(|error| ExactJsonError::InvalidOverride(error.to_string()))?;
        let mut expected = ExactJsonValue { value: projected.value.clone(), utf16_overrides: projected.string_overrides() };
        let expected_delivery_id = expected
            .value
            .get("deliveryId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        expected.value = payload_without_delivery_id(expected.value);
        expected.utf16_overrides.remove("/deliveryId");
        let expected = TranscriptExpectation {
            exact: expected,
            keys: projected.keys,
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
        matching: &mut Option<Result<TranscriptAppendOutcome, TranscriptWriterError>>,
    ) -> Result<Option<String>, TranscriptWriterError> {
        if line.is_empty() {
            return Ok(None);
        }
        let projected = std::str::from_utf8(line)
            .ok()
            .and_then(|line| lingxi_core::types::utf16_json::Utf16JsonProjection::parse(line).ok())
            .ok_or(TranscriptWriterError::CorruptLine { offset })?;
        let mut exact = ExactJsonValue { value: projected.value.clone(), utf16_overrides: projected.string_overrides() };
        let value = exact.value;
        let stored_uuid = value.get("uuid").and_then(Value::as_str);
        let last_uuid = stored_uuid
            .filter(|uuid| !uuid.is_empty())
            .map(str::to_owned);
        let stored_delivery_id = value.get("deliveryId").and_then(Value::as_str);
        let matches_uuid = stored_uuid == Some(message_uuid);
        let matches_delivery = stored_delivery_id == Some(delivery_id);
        let identities_agree = matches_uuid
            && match expected.delivery_id.as_deref() {
                Some(expected_id) => stored_delivery_id == Some(expected_id),
                None => value.get("deliveryId").is_none(),
            };
        if matches_uuid || matches_delivery {
            let mut actual = payload_without_delivery_id(value);
            let mut comparable_expected = expected.exact.value.clone();
            let mut expected_units = expected.exact.utf16_overrides.clone();
            exact.utf16_overrides.remove("/deliveryId");
            // A caller may use null as the parent placeholder for a new
            // Fusion delivery. Existing duplicate rows carry their resolved
            // parent; compare all immutable fields while ignoring only this
            // derived field during the duplicate probe.
            if comparable_expected
                .get("parentUuid")
                .is_none_or(Value::is_null)
            {
                if let Value::Object(object) = &mut actual {
                    object.remove("parentUuid");
                }
                if let Value::Object(object) = &mut comparable_expected {
                    object.remove("parentUuid");
                }
                exact.utf16_overrides.remove("/parentUuid");
                expected_units.remove("/parentUuid");
            }
            // Native rows identify their prepared UUID alone. Fusion rows
            // carry the current delivery identity as well. A retry must use
            // the same format and immutable payload as its original append.
            let outcome = if identities_agree
                && actual == comparable_expected
                && exact.utf16_overrides == expected_units
                && projected.keys == expected.keys
            {
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

    /// Append a pre-serialized JSON object exactly once.  This is useful for
    /// callers that already have a stable wire DTO and want to avoid a second
    /// serialization pass; the method still canonicalizes the delivery id.
    pub fn append_object_once(
        &self,
        transcript_relative: &Path,
        delivery_id: &str,
        payload: Map<String, Value>,
    ) -> Result<TranscriptAppendOutcome, TranscriptWriterError> {
        self.append_json_once(transcript_relative, delivery_id, Value::Object(payload))
    }
}

impl DurableTranscriptTransaction<'_> {
    /// Append one already-serialized JSON object while this transaction owns
    /// the session lock. This is intentionally not append-once: relocation
    /// markers are ordinary transcript metadata and their caller serializes
    /// them with the shared in-process writer mutex.
    pub fn append_raw_json_at(
        &self,
        transcript_root: &Path,
        transcript_identity: &RootIdentity,
        transcript_relative: &Path,
        payload: Value,
    ) -> Result<(), TranscriptWriterError> {
        self.append_raw_json_at_exact(
            transcript_root,
            transcript_identity,
            transcript_relative,
            payload,
            &Utf16Overrides::new(),
            None,
        )
    }

    /// Append one ordinary native row with recovered exact string leaves.
    /// It retains the ordinary path's ordering and durability policy.
    pub fn append_raw_json_at_exact(
        &self,
        transcript_root: &Path,
        transcript_identity: &RootIdentity,
        transcript_relative: &Path,
        payload: Value,
        utf16_overrides: &Utf16Overrides,
        projection: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>,
    ) -> Result<(), TranscriptWriterError> {
        let mut line = crate::jsonl::exact_json::native_projection_bytes(&payload, utf16_overrides, projection)?;
        line.push(b'\n');
        // Ordinary transcript rows do not buy durability, and never did: the
        // non-durable writer this path replaced only flushed. They travel
        // through the transaction for ORDERING -- so a `/cd` relocation cannot
        // split an ordinary append from a Fusion delivery -- not for fsync.
        // Paying `F_FULLFSYNC` per row costs 10-100ms each on macOS and turns
        // an ordinary turn into several of them.
        //
        // The directory entry is a different matter: creating the file is
        // worth one parent sync, so a crash cannot lose the transcript itself.
        let file_present =
            lingxi_core::host::rooted_fs::checked_join(transcript_root, transcript_relative)
                .map(|path| path.exists())
                .unwrap_or(false);
        let mut file = open_append_file_pinned(
            transcript_root,
            transcript_relative,
            Some(transcript_identity),
        )?;
        file.write_all(&line)
            .map_err(|error| FsError::Io(error.to_string()))?;
        if !file_present {
            #[cfg(test)]
            self.writer
                .syncs
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            sync_parent_pinned(
                transcript_root,
                transcript_relative,
                Some(transcript_identity),
            )?;
        }
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
        let mut file = match open_read_file_pinned(
            transcript_root,
            transcript_relative,
            Some(transcript_identity),
        ) {
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
            atomic_write_stream_pinned(
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
            )?;
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
        drop(file);

        atomic_write_pinned(
            transcript_root,
            transcript_relative,
            &body,
            AtomicWriteOptions {
                overwrite: true,
                create_parents: false,
                dir_mode: 0o700,
                file_mode: 0o600,
            },
            transcript_identity,
        )?;
        Ok(true)
    }

    /// Append under the writer's own pinned root without reacquiring the lock.
    pub fn append_json_once(
        &self,
        transcript_relative: &Path,
        delivery_id: &str,
        payload: Value,
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
        payload: Value,
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
    /// same transaction. In particular, a recovered duplicate may still be
    /// the tip after its earlier write succeeded but fsync failed. No second
    /// scan or unlocked tip lookup is needed.
    pub fn append_json_once_at_with_tip(
        &self,
        transcript_root: &Path,
        transcript_identity: &RootIdentity,
        transcript_relative: &Path,
        delivery_id: &str,
        payload: Value,
    ) -> Result<(TranscriptAppendOutcome, bool), TranscriptWriterError> {
        self.writer.append_json_once_at_locked(
            transcript_root,
            transcript_identity,
            transcript_relative,
            delivery_id,
            payload,
            &Utf16Overrides::new(),
            true,
            None,
            None,
        )
    }

    /// Append one durable delivery and register its outer UUID in the Host
    /// identity sidecar under the same transaction, only after duplicate
    /// detection has established that a new row will be written.
    pub(crate) fn append_json_once_at_with_tip_identity(
        &self,
        transcript_root: &Path,
        transcript_identity: &RootIdentity,
        transcript_relative: &Path,
        delivery_id: &str,
        payload: Value,
        identity_store: &IdentityLogStore,
        transcript_path: &Path,
    ) -> Result<(TranscriptAppendOutcome, bool), TranscriptWriterError> {
        self.writer.append_json_once_at_locked(
            transcript_root,
            transcript_identity,
            transcript_relative,
            delivery_id,
            payload,
            &Utf16Overrides::new(),
            true,
            Some((identity_store, transcript_path)),
            None,
        )
    }

    /// Append native exact JavaScript strings once by their prepared UUID.
    /// The delivery key stays private; no `deliveryId` field is emitted.
    pub fn append_json_once_at_with_tip_exact(
        &self,
        transcript_root: &Path,
        transcript_identity: &RootIdentity,
        transcript_relative: &Path,
        delivery_id: &str,
        payload: Value,
        utf16_overrides: &Utf16Overrides,
    ) -> Result<(TranscriptAppendOutcome, bool), TranscriptWriterError> {
        self.writer.append_json_once_at_locked(
            transcript_root,
            transcript_identity,
            transcript_relative,
            delivery_id,
            payload,
            utf16_overrides,
            false,
            None,
            None,
        )
    }

    /// The ordinary Host session writer variant. It records the outer UUID in
    /// its Host-only identity sidecar under this already-held transaction,
    /// after duplicate detection and before the transcript append.
    pub(crate) fn append_json_once_at_with_tip_exact_identity(
        &self,
        transcript_root: &Path,
        transcript_identity: &RootIdentity,
        transcript_relative: &Path,
        delivery_id: &str,
        payload: Value,
        utf16_overrides: &Utf16Overrides,
        identity_store: &IdentityLogStore,
        transcript_path: &Path,
        projection: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>,
    ) -> Result<(TranscriptAppendOutcome, bool), TranscriptWriterError> {
        self.writer.append_json_once_at_locked(
            transcript_root,
            transcript_identity,
            transcript_relative,
            delivery_id,
            payload,
            utf16_overrides,
            false,
            Some((identity_store, transcript_path)),
            projection,
        )
    }
}

fn transcript_line_has_uuid(line: &[u8], expected_uuid: &str) -> bool {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let Ok(line) = std::str::from_utf8(line) else {
        return false;
    };
    lingxi_core::types::utf16_json::Utf16JsonProjection::parse(line)
        .ok()
        .and_then(|exact| {
            exact
                .value
                .get("uuid")
                .and_then(Value::as_str)
                .map(|uuid| uuid == expected_uuid)
        })
        .unwrap_or(false)
}

fn payload_without_delivery_id(mut payload: Value) -> Value {
    if let Value::Object(object) = &mut payload {
        object.remove("deliveryId");
    }
    payload
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_exact_written_failure_requires_durable_cold_retry_and_same_units() {
        let dir = tempfile::tempdir().unwrap();
        let writer = DurableTranscriptWriter::open(dir.path()).unwrap();
        let identity = root_identity(dir.path()).unwrap();
        let path = Path::new("native.jsonl");
        let payload = json!({"uuid":"native-peer", "parentUuid":null, "origin":{"kind":"peer","body":"\u{fffd}"}, "message":{"content":[{"type":"text","text":"\u{fffd}"}]}});
        let overrides = Utf16Overrides::from([
            ("/origin/body".into(), vec![0xd83d]),
            ("/message/content/0/text".into(), vec![0xd83d]),
        ]);
        writer
            .fail_next_append_sync
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(
            writer.with_transaction(|tx| tx.append_json_once_at_with_tip_exact(
                dir.path(),
                &identity,
                path,
                "private-delivery",
                payload.clone(),
                &overrides
            )),
            Err(TranscriptWriterError::WrittenButNotDurable(_))
        ));
        let raw = std::fs::read_to_string(dir.path().join(path)).unwrap();
        assert_eq!(raw.matches("\\ud83d").count(), 2);
        assert!(!raw.contains("deliveryId"));
        drop(writer);
        let cold = DurableTranscriptWriter::open(dir.path()).unwrap();
        cold.fail_next_existing_sync_for_test();
        assert!(cold
            .with_transaction(|tx| tx.append_json_once_at_with_tip_exact(
                dir.path(),
                &identity,
                path,
                "private-delivery",
                payload.clone(),
                &overrides
            ))
            .is_err());
        assert_eq!(
            cold.with_transaction(|tx| tx.append_json_once_at_with_tip_exact(
                dir.path(),
                &identity,
                path,
                "private-delivery",
                payload.clone(),
                &overrides
            ))
            .unwrap(),
            (TranscriptAppendOutcome::AlreadyPresent, true)
        );
        let different = Utf16Overrides::from([
            ("/origin/body".into(), vec![0xdc00]),
            ("/message/content/0/text".into(), vec![0xd83d]),
        ]);
        assert!(matches!(
            cold.with_transaction(|tx| tx.append_json_once_at_with_tip_exact(
                dir.path(),
                &identity,
                path,
                "private-delivery",
                payload,
                &different
            )),
            Err(TranscriptWriterError::DeliveryConflict { .. })
        ));
        assert_eq!(std::fs::read_to_string(dir.path().join(path)).unwrap(), raw);
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
                transaction.append_raw_json_at(dir.path(), &identity, path, json!({"n": 1}))
            })
            .unwrap();
        // One parent sync for creating the file, and nothing else.
        assert_eq!(writer.sync_count_for_test(), 1);

        for n in 2..=5 {
            writer
                .with_transaction(|transaction| {
                    transaction.append_raw_json_at(dir.path(), &identity, path, json!({"n": n}))
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
                .append_json_once(path, "delivery", json!({"uuid": "fusion"}))
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
        let payload = json!({"uuid":"fusion", "text":"answer"});
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
            .append_json_once(path, "later", json!({"uuid":"later", "text":"next"}))
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
            json!({"uuid":"image", "message":{"content":[{"type":"image","data":"A".repeat(64 * 1024)}]}}),
        ).unwrap();
        let payload = json!({"uuid":"fusion", "text":"answer", "parentUuid":null});
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
            writer.append_json_once(path, "delivery", json!({"uuid":"fusion","text":"answer"})),
            Err(TranscriptWriterError::DeliveryConflict { .. })
        ));
        let mut broken = original;
        broken.pop();
        std::fs::write(dir.path().join(path), &broken).unwrap();
        assert!(matches!(
            writer.append_json_once(path, "delivery", json!({"uuid":"other","text":"answer"})),
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
                writer.append_json_once(path, "d", json!({"uuid":"f"})),
                Err(TranscriptWriterError::CorruptLine { .. })
            ));
            assert_eq!(std::fs::read(dir.path().join(path)).unwrap(), bytes);
        }
        // JSON strings admit escaped unmatched UTF-16 as well as scalar text.
        for suffix in ["你好\\uD83D\\uDE00", "\\uD800", "\\uDC00", "\\uD800\\u0041"] {
            std::fs::write(dir.path().join(path), format!("{prefix}{suffix}\"}}")).unwrap();
            assert_eq!(
                writer
                    .append_json_once(path, "d", json!({"uuid":"f"}))
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
                .append_json_once(path, "delivery-1", json!({"uuid":"message-1","text":"ok"}),)
                .unwrap(),
            TranscriptAppendOutcome::Appended
        );
        assert_eq!(
            writer
                .append_json_once(path, "delivery-1", json!({"uuid":"message-1","text":"ok"}),)
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
        let payload = json!({"uuid":"message-1","text":"ok"});
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
            .append_json_once(path, "delivery-1", json!({"uuid":"message-1","text":"ok"}))
            .unwrap();
        assert!(matches!(
            writer.append_json_once(
                path,
                "delivery-2",
                json!({"uuid":"message-1","text":"different"}),
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
            .append_json_once(path, "delivery-1", json!({"uuid":"message-1","text":"ok"}))
            .unwrap();

        assert!(matches!(
            writer.append_json_once(
                path,
                "delivery-1",
                json!({"uuid":"message-2","text":"ok"}),
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
            .append_json_once(path, "delivery-1", json!({"uuid":"message-1","text":"ok"}))
            .unwrap();

        assert!(matches!(
            writer.append_json_once(
                path,
                "delivery-2",
                json!({"uuid":"message-1","text":"ok"}),
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

        let payload = json!({"uuid":"message-1","text":"ok"});
        assert!(matches!(
            writer.append_json_once(path, "delivery-1", payload.clone()),
            Err(TranscriptWriterError::DeliveryConflict { .. })
        ));
        // A current native UUID-only append still retries its own row.
        let identity = root_identity(dir.path()).unwrap();
        assert_eq!(
            writer
                .with_transaction(|tx| tx.append_json_once_at_with_tip_exact(
                    dir.path(),
                    &identity,
                    path,
                    "private-delivery",
                    payload.clone(),
                    &Utf16Overrides::new(),
                ))
                .unwrap(),
            (TranscriptAppendOutcome::AlreadyPresent, true)
        );

        let mut invalid_native = payload.clone();
        invalid_native["deliveryId"] = Value::Null;
        assert!(matches!(
            writer.with_transaction(|tx| tx.append_json_once_at_with_tip_exact(
                dir.path(),
                &identity,
                path,
                "private-delivery",
                invalid_native.clone(),
                &Utf16Overrides::new(),
            )),
            Err(TranscriptWriterError::NativeDeliveryIdField)
        ));
        std::fs::write(dir.path().join(path), format!("{invalid_native}\n")).unwrap();
        assert!(matches!(
            writer.with_transaction(|tx| tx.append_json_once_at_with_tip_exact(
                dir.path(),
                &identity,
                path,
                "private-delivery",
                payload.clone(),
                &Utf16Overrides::new(),
            )),
            Err(TranscriptWriterError::DeliveryConflict { .. })
        ));

        let fusion_path = Path::new("fusion.jsonl");
        writer
            .append_json_once(fusion_path, "delivery-1", payload.clone())
            .unwrap();
        assert!(matches!(
            writer.with_transaction(|tx| tx.append_json_once_at_with_tip_exact(
                dir.path(),
                &identity,
                fusion_path,
                "private-delivery",
                payload,
                &Utf16Overrides::new(),
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
                    json!({"uuid":"message-1","text":"ok"}),
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
                    json!({"uuid":"message-1","text":"ok"}),
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
                    json!({"uuid":"message-1", "parentUuid": null, "type":"user"}),
                )?;
                transaction.append_json_once_at(
                    transcript_dir.path(),
                    &identity,
                    path,
                    "delivery-2",
                    json!({"uuid":"message-2", "parentUuid": null, "type":"user"}),
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
                    json!({"uuid":"message-2", "parentUuid": null, "type":"user"}),
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
                    json!({"uuid":"message-new","text":"ok"}),
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
            writer.append_json_once(path, "delivery-1", json!({"uuid":"message-1","text":"ok"}),),
            Err(TranscriptWriterError::Fs(_))
        ));
    }
}
