//! Effectively-once transcript delivery under a session-state lock.
//!
//! `JsonlWriter` remains the compatibility writer used by older embedders.
//! This writer is the durable path for terminal/outbox records: it uses the
//! pinned session-state directory as the lock root, checks the stable delivery
//! id under that same lock, appends through the exact no-follow handle, and
//! fsyncs before acknowledging success.

use crate::jsonl::journal::{SESSION_STATE_DIR_MODE, SESSION_STATE_FILE_MODE};
use lingxi_core::host::rooted_fs::{
    lock_exclusive_pinned, open_append_file_pinned, open_read_file_pinned, root_identity,
    sync_parent_pinned, RootIdentity,
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
/// does not validate their UTF-8/surrogate pairs and uses a depth-sized scratch
/// stack. Guard precisely those properties; serde still owns JSON grammar.
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
        value: u16,
        left: u8,
        low: bool,
    },
    LowSlash,
    LowU,
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
                b'u' => StringEscape::Unicode {
                    value: 0,
                    left: 4,
                    low: false,
                },
                b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => StringEscape::None,
                _ => return false,
            },
            StringEscape::Unicode { value, left, low } => {
                let Some(digit) = char::from(byte).to_digit(16) else {
                    return false;
                };
                let value = (value << 4) | digit as u16;
                if left > 1 {
                    StringEscape::Unicode {
                        value,
                        left: left - 1,
                        low,
                    }
                } else if low {
                    if !(0xdc00..=0xdfff).contains(&value) {
                        return false;
                    }
                    StringEscape::None
                } else if (0xd800..=0xdbff).contains(&value) {
                    StringEscape::LowSlash
                } else if (0xdc00..=0xdfff).contains(&value) {
                    return false;
                } else {
                    StringEscape::None
                }
            }
            StringEscape::LowSlash if byte == b'\\' => StringEscape::LowU,
            StringEscape::LowU if byte == b'u' => StringEscape::Unicode {
                value: 0,
                left: 4,
                low: true,
            },
            StringEscape::LowSlash | StringEscape::LowU => return false,
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

    fn append_json_once_at_locked(
        &self,
        transcript_root: &Path,
        transcript_identity: &RootIdentity,
        transcript_relative: &Path,
        delivery_id: &str,
        mut payload: Value,
    ) -> Result<(TranscriptAppendOutcome, bool), TranscriptWriterError> {
        if !matches!(payload, Value::Object(_)) {
            return Err(TranscriptWriterError::PayloadNotObject);
        }
        let message_uuid = payload
            .get("uuid")
            .and_then(Value::as_str)
            .filter(|uuid| !uuid.is_empty())
            .ok_or(TranscriptWriterError::MissingMessageUuid)?
            .to_string();
        if let Value::Object(object) = &mut payload {
            object.insert(
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
                    let (missing_delimiter, matching, last_uuid) =
                        self.scan_existing(file, delivery_id, &message_uuid, &payload)?;
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
        let mut line =
            serde_json::to_vec(&payload).map_err(|error| FsError::Io(error.to_string()))?;
        line.push(b'\n');
        if line.len() > self.max_record_bytes {
            return Err(TranscriptWriterError::ScanTooLarge {
                limit: self.max_record_bytes,
            });
        }
        if missing_final_delimiter {
            line.insert(0, b'\n');
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
        let expected = payload_without_delivery_id(payload.clone());
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
        expected: &Value,
        matching: &mut Option<Result<TranscriptAppendOutcome, TranscriptWriterError>>,
    ) -> Result<Option<String>, TranscriptWriterError> {
        if line.is_empty() {
            return Ok(None);
        }
        let value: Value = serde_json::from_slice(line)
            .map_err(|_| TranscriptWriterError::CorruptLine { offset })?;
        let stored_uuid = value.get("uuid").and_then(Value::as_str);
        let last_uuid = stored_uuid
            .filter(|uuid| !uuid.is_empty())
            .map(str::to_owned);
        let stored_delivery_id = value.get("deliveryId").and_then(Value::as_str);
        let matches_uuid = stored_uuid == Some(message_uuid);
        let matches_delivery = stored_delivery_id == Some(delivery_id);
        let identities_agree =
            matches_uuid && stored_delivery_id.is_none_or(|stored| stored == delivery_id);
        if matches_uuid || matches_delivery {
            let mut actual = payload_without_delivery_id(value);
            let mut comparable_expected = expected.clone();
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
            }
            // Older transcript rows did not carry `deliveryId`; accepting an
            // identical UUID-only row makes the upgrade idempotent. Once a
            // delivery id is present, both stable identities must agree: a
            // collision in either namespace is a hard conflict.
            let outcome = if identities_agree && actual == comparable_expected {
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
        let mut line =
            serde_json::to_vec(&payload).map_err(|error| FsError::Io(error.to_string()))?;
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
        )
    }
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
            format!("{prefix}\\uD800\"}}").into_bytes(),
            format!("{prefix}\\uDC00\"}}").into_bytes(),
            format!("{prefix}\\uD800\\u0041\"}}").into_bytes(),
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
        // Real multibyte Unicode and an escaped supplementary codepoint both
        // remain valid while a large unrelated image body is skipped.
        let valid = format!("{prefix}你好\\uD83D\\uDE00\"}}");
        std::fs::write(dir.path().join(path), valid).unwrap();
        assert_eq!(
            writer
                .append_json_once(path, "d", json!({"uuid":"f"}))
                .unwrap(),
            TranscriptAppendOutcome::Appended
        );
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
    fn identical_legacy_uuid_without_delivery_id_is_upgrade_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let writer = DurableTranscriptWriter::open(dir.path()).unwrap();
        let path = Path::new("transcript.jsonl");
        let mut existing = serde_json::to_vec(&json!({"uuid":"message-1","text":"ok"})).unwrap();
        existing.push(b'\n');
        std::fs::write(dir.path().join(path), existing).unwrap();

        assert_eq!(
            writer
                .append_json_once(path, "delivery-1", json!({"uuid":"message-1","text":"ok"}))
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
