//! Append-only JSONL writer — 1:1 port of
//! `claude-code/src/utils/sessionStorage.ts:2572-2584` (`appendEntryToFile`).
//!
//! Lock: serialize via `serde_json::to_string` (no whitespace, no indent),
//! terminate every line with a single `\n`, file mode `0o600`, dir mode `0o700`.

use crate::jsonl::durable_writer::{
    DurableTranscriptWriter, TranscriptAppendOutcome, TranscriptWriterError,
};
use crate::jsonl::exact_json::{
    native_message_bytes, native_message_projection, parse_exact_json, ExactJsonError,
};
use crate::jsonl::message_identity::{self, IdentityLogStore, SessionMessageIdentitySnapshot};
use crate::jsonl::re_append::{
    plan_re_append, read_tail, SessionMetadataState, METADATA_REAPPEND_BACKSTOP_BYTES,
};
use crate::jsonl::schema::{session_kind, JsonlMessage, SESSION_KIND_KEY};
use crate::jsonl::transcript_compact::{
    local_gc_enabled, next_backstop, perform_compact_transcript, CompactOutcome, CompactStats,
    COMPACT_BACKSTOP_BYTES,
};
use lingxi_core::host::{FileSystem, FlockGuard, FsError};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::SessionId;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::Mutex;

#[path = "persistent_chat.rs"]
mod persistent_chat;
pub use persistent_chat::{CapturedChatTranscript, ChatTranscriptCheckpoint};

/// Native `removeMessageByUuid` scans this much of the file tail before it
/// falls back to a full rewrite.
const TOMBSTONE_TAIL_BYTES: u64 = 64 * 1024;
/// Native skips a full-file tombstone rewrite above 50 MiB when the UUID is
/// outside the tail window.
const TOMBSTONE_REWRITE_LIMIT_BYTES: u64 = 50 * 1024 * 1024;

/// Failure modes for [`JsonlWriter`] operations.
#[derive(Debug, Error)]
pub enum WriterError {
    /// Exact native string encoding failed.
    #[error(transparent)]
    ExactJson(#[from] ExactJsonError),
    /// Underlying filesystem error.
    #[error(transparent)]
    Fs(#[from] FsError),
    /// `serde_json::to_string` failed (e.g. malformed `Value`).
    #[error("serialize failure: {0}")]
    Serialize(#[from] serde_json::Error),
    /// The production composition root's durable transcript transaction.
    #[error(transparent)]
    Durable(#[from] TranscriptWriterError),
}

fn queue_journal_error(error: WriterError) -> TranscriptWriterError {
    match error {
        WriterError::ExactJson(error) => TranscriptWriterError::ExactJson(error),
        WriterError::Fs(error) => TranscriptWriterError::Fs(error),
        WriterError::Serialize(error) => TranscriptWriterError::ExactJson(error.into()),
        WriterError::Durable(error) => error,
    }
}

fn initialize_identity_for_fresh_metadata(
    store: &IdentityLogStore,
    path: &Path,
    root: &Path,
    identity: &lingxi_core::host::rooted_fs::RootIdentity,
) -> Result<(), FsError> {
    let relative = path
        .file_name()
        .map(PathBuf::from)
        .ok_or_else(|| FsError::Io("transcript has no filename".into()))?;
    match lingxi_core::host::rooted_fs::open_read_file_pinned(root, &relative, Some(identity)) {
        Ok(file)
            if file
                .metadata()
                .map_err(|error| FsError::Io(error.to_string()))?
                .len()
                > 0 =>
        {
            // Metadata can be appended to Native files, but it must not import
            // their message identities outside the explicit resume boundary.
            return Ok(());
        }
        Ok(_) | Err(FsError::NotFound(_)) => {}
        Err(error) => return Err(error),
    }
    message_identity::initialize_empty_host_log_at(store, path, root, identity)
}

/// SC-07 — stamp `sessionKind` on a chain entry that does not carry one.
///
/// The oracle sets it inside the persistence layer, on every entry
/// `insertMessageChain` writes (@296794533: `…, sessionKind:a3e(), userType,
/// …`), not at the message factories — which is why this lives here and not in
/// the callers that build [`JsonlMessage`]. `a3e()` is process-global
/// ([`session_kind`]), so one env read per line is the whole derivation.
///
/// Returns `None` — "nothing to change, serialize the caller's value" — in the
/// overwhelmingly common case: no session kind set, or the entry already
/// carries one (a resumed foreign line round-tripping through `extra`, which
/// must keep the value the ORIGINAL writer stamped rather than adopt this
/// process's). Only a genuine `bg` / `daemon` / `daemon-worker` process pays
/// the clone.
fn stamp_session_kind(msg: &JsonlMessage) -> Option<JsonlMessage> {
    let kind = session_kind()?;
    if msg.extra.contains_property(SESSION_KIND_KEY) {
        return None;
    }
    let mut stamped = msg.clone();
    let kind_projection =
        Utf16JsonProjection::root_string(kind.clone(), kind.encode_utf16().collect())
            .expect("a Rust string is valid UTF-16 JSON text");
    stamped
        .extra
        .set_field(SESSION_KIND_KEY, kind_projection)
        .expect("JsonlMessage extra is an object projection");
    Some(stamped)
}

#[derive(Clone)]
struct DurableTranscriptTarget {
    path: PathBuf,
    writer: Arc<DurableTranscriptWriter>,
    cwd: PathBuf,
}

/// Append-only writer for one session's `<uuid>.jsonl`.
///
/// Holds an exclusive in-process lock so concurrent `append` calls serialize
/// (cross-process locking is delegated to the `FileSystem` flock impl when
/// the orchestrator wants it; the spec only mandates in-process for M5-07).
pub struct JsonlWriter {
    /// Dedicated AppAgent transcript authority; physical I/O is pinned to
    /// this writer and its private Chat directory for its entire lifetime.
    persistent_chat: Option<Arc<std::sync::Mutex<persistent_chat::PersistentChatWriter>>>,
    queued_operations: std::sync::OnceLock<(SessionId, QueueOperationBuffer, PathBuf)>,
    path: PathBuf,
    active_path: Arc<std::sync::RwLock<PathBuf>>,
    fs: Arc<dyn FileSystem>,
    /// Optional session-state transaction used by production composition.
    /// Writers without a session-state root use `FileSystem` directly.
    durable_lock: Arc<std::sync::RwLock<Option<Arc<DurableTranscriptWriter>>>>,
    /// One coherent `(path, writer, cwd)` snapshot for ordinary appends. A hot
    /// session switch publishes this tuple synchronously after the destination
    /// cost authority is active, so no append can combine A's path with B's
    /// session-state lock.
    active_durable_target: Arc<std::sync::RwLock<Option<DurableTranscriptTarget>>>,
    /// Session-pinned transcript targets used by late background recorders.
    /// The ordinary writer follows the active target; a Fusion recorder keeps
    /// its originating session id and resolves this map under the same writer
    /// mutex, so A→B cannot redirect a late A append into B's transcript.
    session_targets: Arc<std::sync::RwLock<HashMap<SessionId, DurableTranscriptTarget>>>,
    lock: Mutex<()>,
    /// Host-only outer-UUID index ledger cache. The durable transcript lock
    /// or rooted sidecar flock serializes each ledger mutation with its row.
    identity_store: IdentityLogStore,
    /// Bytes appended to the active transcript since the last metadata
    /// re-append — the oracle's `bytesSinceMetadataReAppend` (increment site
    /// 2.1.220 @237850612: `bytesSinceMetadataReAppend += Buffer.byteLength(t,"utf8")`,
    /// gated on the write target being the CURRENT session file, which is
    /// always true for this writer).
    ///
    /// Once it reaches [`METADATA_REAPPEND_BACKSTOP_BYTES`] the metadata set
    /// must be re-appended so it stays inside the 64 KiB tail window every
    /// session-index reader scans. See [`Self::metadata_re_append_due`].
    bytes_since_metadata_re_append: AtomicUsize,
    /// The metadata this writer will re-append when the backstop fires.
    ///
    /// The writer OWNS this rather than taking it per call: it is the single
    /// funnel every metadata record already goes through
    /// ([`Self::append_custom_title`] and friends), and it already owns the
    /// file, the lock and the byte counter. Any other owner would have to be
    /// threaded from the composition root through the resume path to reach the
    /// same place.
    metadata_states: std::sync::RwLock<HashMap<String, Arc<Mutex<SessionMetadataState>>>>,
    /// Bytes appended to the active transcript since the last SUCCESSFUL
    /// transcript rewrite — the oracle's `bytesSinceCompact`
    /// (increment site @296775839, trigger @296777239).
    ///
    /// Separate counter from [`Self::bytes_since_metadata_re_append`] on
    /// purpose: they fire three orders of magnitude apart (32 KiB vs. 20 MiB)
    /// and the rewrite resets both, but a re-append resets only its own.
    bytes_since_compact: AtomicU64,
    /// `backstopThresholdBytes` — starts at
    /// [`COMPACT_BACKSTOP_BYTES`], doubles (capped) after a rewrite that
    /// reclaimed under 10 %, and is reset to the base every time a compact
    /// boundary is written (@296794903).
    compact_backstop_bytes: AtomicU64,
}

/// Producer-owned operations staged before a session writer is available.
/// The writer drains this same FIFO under its normal append lease.
pub type QueueOperationBuffer =
    Arc<std::sync::Mutex<VecDeque<lingxi_core::types::utf16_json::Utf16JsonProjection>>>;

struct QueueOperationDrain {
    source: QueueOperationBuffer,
    records: VecDeque<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    current: Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
}
impl Drop for QueueOperationDrain {
    fn drop(&mut self) {
        if let Some(current) = self.current.take() {
            self.records.push_front(current);
        }
        if self.records.is_empty() {
            return;
        }
        let mut pending = self
            .source
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.records.append(&mut pending);
        *pending = std::mem::take(&mut self.records);
    }
}

/// `eI(e)` — the compact-boundary predicate, applied to an outgoing chain entry.
///
/// Upstream arms the transcript rewrite from inside `insertMessageChain`
/// (@296794903: `if(y&&!t&&this.sessionFile&&l===zt()) this.backstopThresholdBytes=Uyr,
/// this.requestCompact(this.sessionFile,a)`), i.e. at the moment the boundary
/// line is persisted — not from the compaction engine. Same seam here.
fn writes_compact_boundary(msg: &JsonlMessage) -> bool {
    msg.message_type == "system" && msg.extra.string_at("/subtype") == Some("compact_boundary")
}

/// UUID identity ignores hexadecimal case; physical transcript paths retain it.
fn canonical_metadata_session_id(session_id: &str) -> String {
    uuid::Uuid::parse_str(session_id)
        .map(|id| id.to_string())
        .unwrap_or_else(|_| session_id.to_owned())
}

/// Read the newest custom-title record when the bounded tail no longer
/// contains one. This protects the in-process metadata state from a title
/// appended by another host (for example Electron's metadata-only rename)
/// before a large transcript write crosses the backstop in one operation.
fn latest_custom_title(path: &Path, session_id: &str) -> Option<Option<Utf16JsonProjection>> {
    let session_id = canonical_metadata_session_id(session_id);
    let contents = std::fs::read_to_string(path).ok()?;
    contents.lines().rev().find_map(|line| {
        let value = Utf16JsonProjection::parse(line).ok()?;
        if value.string_at("/type") != Some("custom-title")
            || !value
                .string_at("/sessionId")
                .is_some_and(|recorded| canonical_metadata_session_id(recorded) == session_id)
        {
            return None;
        }
        let title = value.subprojection("/customTitle").ok()?;
        title.string_units("")?;
        Some(Some(title))
    })
}

/// Linux/macOS `EXDEV` and Windows `ERROR_NOT_SAME_DEVICE` are intentionally
/// handled without a libc dependency: this leaf crate builds on both native
/// and mobile targets, while the fallback is only reached after `rename`
/// reports one of these platform error numbers.
fn is_cross_device(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(18) | Some(17))
}

/// Move one transcript without replacing an occupied destination. A same-file
/// rename is atomic on the normal path; a cross-device move copies the bytes,
/// then removes the source only after the copy succeeds. The destination is
/// removed again if source cleanup fails, preserving the source as the
/// recoverable copy.
fn move_file_with_cross_device_fallback(from: &Path, to: &Path) -> std::io::Result<()> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(error) if is_cross_device(&error) => {
            if let Err(copy_error) = std::fs::copy(from, to) {
                let _ = std::fs::remove_file(to);
                return Err(copy_error);
            }
            if let Err(remove_error) = std::fs::remove_file(from) {
                let _ = std::fs::remove_file(to);
                return Err(remove_error);
            }
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// Quarantine an occupied destination with a non-JSONL suffix. Keeping the
/// suffix off the session filename prevents the indexer from presenting stale
/// bytes as the active session while retaining the file for manual recovery.
fn move_to_superseded_path(path: &Path) -> std::io::Result<PathBuf> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| std::io::Error::other("session transcript path is not UTF-8"))?;
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    for attempt in 0..1000_u32 {
        let suffix = if attempt == 0 {
            format!(".superseded-{millis}")
        } else {
            format!(".superseded-{millis}-{attempt}")
        };
        let candidate = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(format!("{file_name}{suffix}"));
        if std::fs::symlink_metadata(&candidate).is_err() {
            std::fs::rename(path, &candidate)?;
            return Ok(candidate);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a superseded transcript name",
    ))
}

struct IdentitySidecarRelocation {
    source: PathBuf,
    destination: PathBuf,
    destination_backup: Option<PathBuf>,
    moved_source: bool,
}

impl IdentitySidecarRelocation {
    fn rollback(self) {
        if self.moved_source {
            if let Err(error) =
                move_file_with_cross_device_fallback(&self.destination, &self.source)
            {
                tracing::warn!(
                    source = %self.source.display(),
                    destination = %self.destination.display(),
                    %error,
                    "failed to restore Host identity sidecar after transcript relocation error"
                );
            }
        }
        if let Some(backup) = self.destination_backup {
            if let Err(error) = move_file_with_cross_device_fallback(&backup, &self.destination) {
                tracing::warn!(
                    backup = %backup.display(),
                    destination = %self.destination.display(),
                    %error,
                    "failed to restore displaced Host identity sidecar"
                );
            }
        }
    }
}

fn identity_sidecar_path(transcript_path: &Path) -> std::io::Result<PathBuf> {
    let parent = transcript_path
        .parent()
        .ok_or_else(|| std::io::Error::other("transcript has no parent"))?;
    let relative = message_identity::log_path(transcript_path)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    Ok(parent.join(relative))
}

fn inspect_identity_sidecar(path: &Path) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(true),
        Ok(_) => Err(std::io::Error::other(format!(
            "Host identity sidecar is not a regular file: {}",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn relocate_identity_sidecars(
    source_transcript: &Path,
    destination_transcript: &Path,
) -> std::io::Result<IdentitySidecarRelocation> {
    let source = identity_sidecar_path(source_transcript)?;
    let destination = identity_sidecar_path(destination_transcript)?;
    let source_exists = inspect_identity_sidecar(&source)?;
    let destination_exists = inspect_identity_sidecar(&destination)?;
    let destination_backup = if destination_exists {
        Some(move_to_superseded_path(&destination)?)
    } else {
        None
    };
    if source_exists {
        if let Err(error) = move_file_with_cross_device_fallback(&source, &destination) {
            if let Some(backup) = destination_backup.as_ref() {
                if let Err(restore_error) =
                    move_file_with_cross_device_fallback(backup, &destination)
                {
                    tracing::warn!(
                        backup = %backup.display(),
                        destination = %destination.display(),
                        %restore_error,
                        "failed to restore displaced Host identity sidecar"
                    );
                }
            }
            return Err(error);
        }
    }
    Ok(IdentitySidecarRelocation {
        source,
        destination,
        destination_backup,
        moved_source: source_exists,
    })
}

/// Find the lexical root shared by both transcript parent directories.
/// `/cd` paths normally share `<config-home>/projects`; keeping this generic
/// preserves the writer's direct relocation tests and embedded callers.
fn relocation_root(from: &Path, to: &Path) -> std::io::Result<PathBuf> {
    let from = from
        .parent()
        .ok_or_else(|| std::io::Error::other("transcript source has no parent"))?;
    let to = to
        .parent()
        .ok_or_else(|| std::io::Error::other("transcript destination has no parent"))?;
    let mut root = PathBuf::new();
    for (left, right) in from.components().zip(to.components()) {
        if left != right {
            break;
        }
        root.push(left.as_os_str());
    }
    if root.as_os_str().is_empty() {
        return Err(std::io::Error::other(
            "transcript paths have no shared relocation root",
        ));
    }
    Ok(root)
}

/// Reject a symlink or non-directory anywhere from `root` through `parent`.
/// `symlink_metadata` inspects each directory entry itself instead of
/// following it. Callers run this both before destination quarantine and again
/// immediately before the move, bounding the pathname-swap window without
/// changing the established rename/EXDEV behavior.
fn validate_real_parent_chain(root: &Path, parent: &Path) -> std::io::Result<()> {
    let relative = parent
        .strip_prefix(root)
        .map_err(|_| std::io::Error::other("transcript parent is outside the relocation root"))?;
    let root_metadata = std::fs::symlink_metadata(root)?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(std::io::Error::other(format!(
            "transcript relocation root is not a real directory: {}",
            root.display()
        )));
    }

    let mut current = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(segment) = component else {
            return Err(std::io::Error::other(
                "transcript parent contains a non-normal path component",
            ));
        };
        current.push(segment);
        let metadata = std::fs::symlink_metadata(&current)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(std::io::Error::other(format!(
                "transcript parent is not a real directory: {}",
                current.display()
            )));
        }
    }
    Ok(())
}

fn validate_relocation_parents(
    root: &Path,
    from: &Path,
    to: &Path,
    source_exists: bool,
) -> std::io::Result<()> {
    if source_exists {
        validate_real_parent_chain(
            root,
            from.parent()
                .ok_or_else(|| std::io::Error::other("transcript source has no parent"))?,
        )?;
    }
    validate_real_parent_chain(
        root,
        to.parent()
            .ok_or_else(|| std::io::Error::other("transcript destination has no parent"))?,
    )
}

impl JsonlWriter {
    /// Open (or create on first append) `path`.
    ///
    /// No I/O is performed until `append` is called — keeps construction cheap
    /// for the orchestrator's `Option<Arc<JsonlWriter>>` wiring.
    #[must_use]
    pub fn new(path: PathBuf, fs: Arc<dyn FileSystem>) -> Self {
        Self {
            persistent_chat: None,
            queued_operations: std::sync::OnceLock::new(),
            active_path: Arc::new(std::sync::RwLock::new(path.clone())),
            path,
            fs,
            durable_lock: Arc::new(std::sync::RwLock::new(None)),
            active_durable_target: Arc::new(std::sync::RwLock::new(None)),
            session_targets: Arc::new(std::sync::RwLock::new(HashMap::new())),
            lock: Mutex::new(()),
            identity_store: Arc::new(std::sync::Mutex::new(HashMap::new())),
            bytes_since_metadata_re_append: AtomicUsize::new(0),
            metadata_states: std::sync::RwLock::new(HashMap::new()),
            bytes_since_compact: AtomicU64::new(0),
            compact_backstop_bytes: AtomicU64::new(COMPACT_BACKSTOP_BYTES),
        }
    }

    /// Share the coordinator's durable transcript transaction with ordinary
    /// appends and Fusion outbox delivery. Writers without this session-state
    /// transaction append through their configured `FileSystem`.
    #[must_use]
    pub fn with_durable_lock(self, durable_lock: Arc<DurableTranscriptWriter>) -> Self {
        if self
            .reject_persistent_chat_mutation("shared durable authority")
            .is_err()
        {
            return self;
        }
        *self
            .durable_lock
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(durable_lock);
        self
    }

    pub fn bind_queue_operations(&self, session: SessionId, operations: QueueOperationBuffer) {
        if self
            .reject_persistent_chat_mutation("queue binding")
            .is_err()
        {
            return;
        }
        let owner = self
            .queued_operations
            .get_or_init(|| (session, operations.clone(), self.active_path()));
        assert!(
            owner.0 == session && Arc::ptr_eq(&owner.1, &operations),
            "queue journal already belongs to another source"
        );
    }

    pub async fn flush_queue_operations(&self) -> Result<(), WriterError> {
        {
            let _guard = self.lock.lock().await;
            self.flush_queue_operations_locked().await?;
        }
        self.maybe_re_append_metadata().await;
        Ok(())
    }

    async fn flush_queue_operations_locked(&self) -> Result<(), WriterError> {
        let Some((session, source, initial_path)) = self.queued_operations.get() else {
            return Ok(());
        };
        let mut operations = QueueOperationDrain {
            records: std::mem::take(
                &mut *source
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            ),
            source: source.clone(),
            current: None,
        };
        let target = self
            .session_targets
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session)
            .cloned();
        while let Some(operation) = operations.records.pop_front() {
            operations.current = Some(operation.clone());
            let operation_bytes = if target.is_some() {
                Some(
                    operation
                        .to_json_string()
                        .map_err(|error| {
                            WriterError::ExactJson(ExactJsonError::InvalidOverride(
                                error.to_string(),
                            ))
                        })?
                        .len()
                        + 1,
                )
            } else {
                None
            };
            let result = if let Some(target) = &target {
                self.append_json_durable_locked(
                    target.path.clone(),
                    target.writer.clone(),
                    operation,
                    None,
                )
                .await
                .map_err(WriterError::Durable)
            } else {
                match operation.to_json_string() {
                    Ok(mut line) => {
                        if let Some(parent) = initial_path.parent() {
                            tokio::fs::create_dir_all(parent)
                                .await
                                .map_err(|error| FsError::Io(error.to_string()))?;
                        }
                        let _identity_guard =
                            self.append_identity_non_durable(initial_path, None).await?;
                        line.push('\n');
                        self.append_payload_to_path(initial_path, &line, true).await
                    }
                    Err(error) => Err(WriterError::ExactJson(ExactJsonError::InvalidOverride(
                        error.to_string(),
                    ))),
                }
            };
            if let Err(error) = result {
                return Err(error);
            }
            if let Some(bytes) = operation_bytes {
                self.bytes_since_metadata_re_append
                    .fetch_add(bytes, Ordering::Relaxed);
                self.bytes_since_compact
                    .fetch_add(bytes as u64, Ordering::Relaxed);
            }
            operations.current = None;
        }
        Ok(())
    }

    /// Replace the active session's durable lock after a validated hot
    /// session switch. The writer remains the same shared in-process object;
    /// only the pinned session-state root changes.
    pub fn set_durable_lock(&self, durable_lock: Arc<DurableTranscriptWriter>) {
        if self
            .reject_persistent_chat_mutation("shared durable authority")
            .is_err()
        {
            return;
        }
        *self
            .durable_lock
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(durable_lock);
    }

    /// Pin the current transcript path and durable authority for one session.
    /// Composition roots call this after boot/hot-switch hydration and after
    /// a cwd relocation. No I/O occurs; the next append resolves this target
    /// while holding the writer mutex.
    pub fn bind_session_target(&self, session_id: SessionId, path: PathBuf) {
        let _ = self.activate_session_target(session_id, path, PathBuf::new());
    }

    /// Publish a hot-session transcript target without an await point. The
    /// caller invokes this immediately after the durable cost authority has
    /// switched; cancellation can therefore occur only before both changes or
    /// after both are visible.
    pub fn activate_session_target(
        &self,
        session_id: SessionId,
        path: PathBuf,
        cwd: PathBuf,
    ) -> Result<(), WriterError> {
        let durable_lock = self.durable_lock().ok_or_else(|| {
            WriterError::Fs(FsError::Io(
                "durable transcript lock is not configured".into(),
            ))
        })?;
        self.activate_session_target_with_durable_lock(session_id, path, cwd, durable_lock);
        Ok(())
    }

    /// Atomically publish an explicitly prepared session target and its exact
    /// durable authority. Hot-switch preparation captures this lock without
    /// mutating the active writer; the owned commit consumes it with no await
    /// or fallible lookup between transcript, cost, and conversation identity.
    pub fn activate_session_target_with_durable_lock(
        &self,
        session_id: SessionId,
        path: PathBuf,
        cwd: PathBuf,
        durable_lock: Arc<DurableTranscriptWriter>,
    ) {
        if self
            .reject_persistent_chat_mutation("session target change")
            .is_err()
        {
            return;
        }
        *self
            .durable_lock
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(durable_lock.clone());
        let target = DurableTranscriptTarget {
            path: path.clone(),
            writer: durable_lock,
            cwd,
        };
        self.session_targets
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id, target.clone());
        *self
            .active_durable_target
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(target);
        *self
            .active_path
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = path;
    }

    /// Cwd metadata captured alongside one session's durable transcript target.
    #[must_use]
    pub fn session_target_cwd(&self, session_id: SessionId) -> Option<PathBuf> {
        self.session_targets
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&session_id)
            .map(|target| target.cwd.clone())
    }

    /// Transcript path captured with one session's durable authority.
    #[must_use]
    pub fn session_target_path(&self, session_id: SessionId) -> Option<PathBuf> {
        self.session_targets
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&session_id)
            .map(|target| target.path.clone())
    }

    fn durable_lock(&self) -> Option<Arc<DurableTranscriptWriter>> {
        self.durable_lock
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Whether production attached a session-state transcript authority.
    #[must_use]
    pub fn durable_transcript_enabled(&self) -> bool {
        self.durable_lock().is_some()
    }

    fn active_durable_target(&self) -> Option<DurableTranscriptTarget> {
        self.active_durable_target
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Returns the on-disk path this writer targets.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn durable_writer_for_path(&self, path: &Path) -> Option<Arc<DurableTranscriptWriter>> {
        if let Some(target) = self.active_durable_target() {
            if target.path == path {
                return Some(target.writer);
            }
        }
        self.session_targets
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .find(|target| target.path == path)
            .map(|target| target.writer.clone())
    }

    async fn identity_sidecar_guard(
        &self,
        transcript_path: &Path,
    ) -> Result<Box<dyn FlockGuard>, WriterError> {
        if let Some(guard) = self.persistent_chat_identity_guard(transcript_path)? {
            return Ok(guard);
        }
        let root = transcript_path
            .parent()
            .ok_or_else(|| FsError::Io("transcript has no parent directory".into()))?;
        let sidecar_lock = message_identity::lock_path(transcript_path)?;
        Ok(self.fs.flock_exclusive_rooted(root, &sidecar_lock).await?)
    }

    async fn append_identity_non_durable(
        &self,
        transcript_path: &Path,
        uuid: Option<&str>,
    ) -> Result<Box<dyn FlockGuard>, WriterError> {
        self.prepare_identity_non_durable(transcript_path, uuid, false)
            .await
    }

    async fn prepare_metadata_identity_non_durable(
        &self,
        transcript_path: &Path,
    ) -> Result<Box<dyn FlockGuard>, WriterError> {
        self.prepare_identity_non_durable(transcript_path, None, true)
            .await
    }

    async fn prepare_identity_non_durable(
        &self,
        transcript_path: &Path,
        uuid: Option<&str>,
        metadata_only: bool,
    ) -> Result<Box<dyn FlockGuard>, WriterError> {
        let root = transcript_path
            .parent()
            .ok_or_else(|| FsError::Io("transcript has no parent directory".into()))?
            .to_path_buf();
        if !root.exists() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(&root)
                    .map_err(|error| FsError::Io(error.to_string()))?;
            }
            #[cfg(not(unix))]
            std::fs::create_dir_all(&root).map_err(|error| FsError::Io(error.to_string()))?;
        }
        let guard = self.identity_sidecar_guard(transcript_path).await?;
        let identity = lingxi_core::host::rooted_fs::root_identity(&root)?;
        let store = self.identity_store.clone();
        let transcript_path = transcript_path.to_path_buf();
        let uuid = uuid.map(str::to_owned);
        let strict_owner = self.persistent_chat.clone();
        #[cfg(test)]
        let test_gate = self.take_persistent_chat_identity_test_gate();
        tokio::task::spawn_blocking(move || {
            // The guard must outlive actual sidecar I/O, even if the awaiting
            // task is cancelled. A strict guard also retains its real owner.
            let _strict_io = persistent_chat::identity_io_guard(&strict_owner, &transcript_path)?;
            #[cfg(test)]
            if let Some(gate) = test_gate {
                gate.wait();
            }
            let result = if let Some(uuid) = uuid {
                message_identity::append_row_identity_at(
                    &store,
                    &transcript_path,
                    &root,
                    &identity,
                    &uuid,
                )
                .map(|_| ())
            } else if metadata_only {
                initialize_identity_for_fresh_metadata(&store, &transcript_path, &root, &identity)
            } else {
                message_identity::initialize_empty_host_log_at(
                    &store,
                    &transcript_path,
                    &root,
                    &identity,
                )
            };
            result?;
            Ok::<_, FsError>(guard)
        })
        .await
        .map_err(|error| FsError::Io(error.to_string()))?
        .map_err(WriterError::from)
    }

    /// Read Host-managed outer-row identities for one transcript. A missing
    /// sidecar is valid for a Native transcript that has not been imported by
    /// this Host yet; call [`Self::bootstrap_session_message_identity_snapshot`]
    /// at that explicit import boundary to create stable indices.
    pub async fn read_session_message_identity_snapshot(
        &self,
        transcript_path: &Path,
    ) -> Result<SessionMessageIdentitySnapshot, WriterError> {
        let _guard = self.lock.lock().await;
        let Some(root) = transcript_path.parent() else {
            return Err(FsError::Io("transcript has no parent directory".into()).into());
        };
        if !root.exists() {
            return Ok(SessionMessageIdentitySnapshot::default());
        }
        if let Some(durable_writer) = self.durable_writer_for_path(transcript_path) {
            let path = transcript_path.to_path_buf();
            return tokio::task::spawn_blocking(move || {
                durable_writer.with_transaction(|_| {
                    let root = path.parent().ok_or_else(|| {
                        TranscriptWriterError::Fs(FsError::Io(
                            "transcript has no parent directory".into(),
                        ))
                    })?;
                    let identity = lingxi_core::host::rooted_fs::root_identity(root)?;
                    message_identity::read_snapshot_at(&path, root, &identity)
                        .map_err(TranscriptWriterError::from)
                })
            })
            .await
            .map_err(|error| FsError::Io(error.to_string()))?
            .map_err(WriterError::from);
        }

        let _identity_guard = self.identity_sidecar_guard(transcript_path).await?;
        let path = transcript_path.to_path_buf();
        let strict_owner = self.persistent_chat.clone();
        tokio::task::spawn_blocking(move || {
            let _identity_guard = _identity_guard;
            let _strict_io = persistent_chat::identity_io_guard(&strict_owner, &path)?;
            let root = path
                .parent()
                .ok_or_else(|| FsError::Io("transcript has no parent directory".into()))?;
            let identity = lingxi_core::host::rooted_fs::root_identity(root)?;
            message_identity::read_snapshot_at(&path, root, &identity)
        })
        .await
        .map_err(|error| FsError::Io(error.to_string()))?
        .map_err(WriterError::from)
    }

    /// Explicitly import the current Native JSONL rows into the Host-only
    /// identity ledger. Existing sidecar state is preserved; indices for rows
    /// Native deleted before this first import are unknowable and are not
    /// synthesized.
    pub async fn bootstrap_session_message_identity_snapshot(
        &self,
        transcript_path: &Path,
    ) -> Result<SessionMessageIdentitySnapshot, WriterError> {
        self.bootstrap_session_message_identity_snapshot_with_authority(
            transcript_path,
            None,
        )
        .await
    }

    /// Import a resume target before publishing it as the active session.
    /// The supplied authority belongs to the prepared destination; an older
    /// active session must not lend its transaction to the destination import.
    pub async fn bootstrap_prepared_session_message_identity_snapshot(
        &self,
        transcript_path: &Path,
        durable_writer: Arc<DurableTranscriptWriter>,
    ) -> Result<SessionMessageIdentitySnapshot, WriterError> {
        self.reject_persistent_chat_mutation("shared durable identity authority")?;
        self.bootstrap_session_message_identity_snapshot_with_authority(
            transcript_path,
            Some(durable_writer),
        )
        .await
    }

    async fn bootstrap_session_message_identity_snapshot_with_authority(
        &self,
        transcript_path: &Path,
        durable_writer: Option<Arc<DurableTranscriptWriter>>,
    ) -> Result<SessionMessageIdentitySnapshot, WriterError> {
        let _guard = self.lock.lock().await;
        let root = transcript_path
            .parent()
            .ok_or_else(|| FsError::Io("transcript has no parent directory".into()))?;
        if !root.exists() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(root)
                    .map_err(|error| FsError::Io(error.to_string()))?;
            }
            #[cfg(not(unix))]
            std::fs::create_dir_all(root).map_err(|error| FsError::Io(error.to_string()))?;
        }
        if let Some(durable_writer) =
            durable_writer.or_else(|| self.durable_writer_for_path(transcript_path))
        {
            let path = transcript_path.to_path_buf();
            let store = self.identity_store.clone();
            return tokio::task::spawn_blocking(move || {
                durable_writer.with_transaction(|_| {
                    let root = path.parent().ok_or_else(|| {
                        TranscriptWriterError::Fs(FsError::Io(
                            "transcript has no parent directory".into(),
                        ))
                    })?;
                    let identity = lingxi_core::host::rooted_fs::root_identity(root)?;
                    message_identity::bootstrap_at(&store, &path, root, &identity)
                        .map_err(TranscriptWriterError::from)
                })
            })
            .await
            .map_err(|error| FsError::Io(error.to_string()))?
            .map_err(WriterError::from);
        }

        let _identity_guard = self.identity_sidecar_guard(transcript_path).await?;
        let path = transcript_path.to_path_buf();
        let store = self.identity_store.clone();
        let strict_owner = self.persistent_chat.clone();
        tokio::task::spawn_blocking(move || {
            let _identity_guard = _identity_guard;
            let _strict_io = persistent_chat::identity_io_guard(&strict_owner, &path)?;
            let root = path
                .parent()
                .ok_or_else(|| FsError::Io("transcript has no parent directory".into()))?;
            let identity = lingxi_core::host::rooted_fs::root_identity(root)?;
            message_identity::bootstrap_at(&store, &path, root, &identity)
        })
        .await
        .map_err(|error| FsError::Io(error.to_string()))?
        .map_err(WriterError::from)
    }

    /// Returns the path currently receiving appends.
    ///
    /// Most runtimes keep the initial path for the writer's entire lifetime.
    /// Mobile keeps one orchestrator alive across `NewSession` and
    /// `ResumeSession`, so it retargets the writer after the session transition.
    #[must_use]
    pub fn active_path(&self) -> PathBuf {
        self.active_path
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Clone the filesystem capability used by this writer. Session-targeted
    /// transcript appenders use the same host filesystem for lookup and write.
    #[must_use]
    pub fn filesystem_handle(&self) -> Arc<dyn FileSystem> {
        self.fs.clone()
    }

    /// Capture only each metadata kind's latest exact source record. This is
    /// prepared from the same transcript read that supplied resumed history.
    pub fn capture_session_metadata_source(
        content: &str,
        session_id: &str,
    ) -> Vec<Utf16JsonProjection> {
        let session_id = canonical_metadata_session_id(session_id);
        const KINDS: &[&str] = &[
            "custom-title",
            "ai-title",
            "tag",
            "relocated",
            "last-prompt",
            "agent-name",
            "agent-color",
            "agent-setting",
            "mode",
            "permission-mode",
            "session-mode",
            "isolation-latch",
            "atis-latch",
            "worktree-state",
            "pr-link",
            "bridge-session",
        ];
        let mut latest = std::collections::BTreeMap::new();
        for line in content.lines() {
            let Ok(row) = Utf16JsonProjection::parse(line) else {
                continue;
            };
            if !row
                .string_at("/sessionId")
                .is_some_and(|recorded| canonical_metadata_session_id(recorded) == session_id)
            {
                continue;
            }
            let Some(kind) = row.string_at("/type").filter(|kind| KINDS.contains(kind)) else {
                continue;
            };
            if kind == "relocated"
                && !row
                    .string_at("/relocatedCwd")
                    .is_some_and(|cwd| !cwd.is_empty())
            {
                continue;
            }
            if kind == "atis-latch"
                && !row
                    .string_at("/atis")
                    .is_some_and(crate::jsonl::reader::is_valid_atis_latch)
            {
                continue;
            }
            latest.insert(kind.to_owned(), row);
        }
        latest.into_values().collect()
    }

    fn metadata_from_source(rows: &[Utf16JsonProjection]) -> SessionMetadataState {
        let mut state = SessionMetadataState::default();
        for row in rows {
            let string = |field: &str| row.string_at(&format!("/{field}")).map(str::to_owned);
            let value = || {
                row.to_json_string()
                    .ok()
                    .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            };
            match row.string_at("/type") {
                Some("custom-title") => {
                    state.title = row
                        .subprojection("/customTitle")
                        .ok()
                        .filter(|title| title.string_units("").is_some())
                }
                Some("ai-title") => state.ai_title = string("aiTitle"),
                Some("tag") => state.tag = string("tag"),
                Some("relocated") => state.relocated_cwd = string("relocatedCwd"),
                Some("last-prompt") => {
                    state.last_prompt = string("lastPrompt");
                    state.leaf_uuid = string("leafUuid");
                }
                Some("agent-name") => state.agent_name = string("agentName"),
                Some("agent-color") => state.agent_color = string("agentColor"),
                Some("agent-setting") => state.agent_setting = string("agentSetting"),
                Some("mode") => state.mode = string("mode"),
                Some("permission-mode") => state.permission_mode = string("permissionMode"),
                Some("session-mode") => state.session_mode = string("sessionMode"),
                Some("isolation-latch") => state.isolation_latch = string("side"),
                Some("atis-latch") => {
                    state.atis = string("atis")
                        .filter(|value| crate::jsonl::reader::is_valid_atis_latch(value))
                }
                Some("worktree-state") => {
                    state.worktree = value().and_then(|value| value.get("worktreeSession").cloned())
                }
                Some("pr-link") => {
                    state.pr_number = value().and_then(|value| {
                        value.get("prNumber").and_then(serde_json::Value::as_u64)
                    });
                    state.pr_url = string("prUrl");
                    state.pr_repository = string("prRepository");
                }
                Some("bridge-session") => {
                    state.bridge_id = string("bridgeSessionId");
                    state.bridge_seq = value().and_then(|value| {
                        value
                            .get("lastSequenceNum")
                            .and_then(serde_json::Value::as_u64)
                    });
                    state.bridge_grouping_id = string("sessionGroupingId");
                    state.bridge_dialog_kinds = value()
                        .and_then(|value| {
                            value
                                .get("declaredDialogKinds")
                                .and_then(serde_json::Value::as_array)
                                .cloned()
                        })
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(|value| value.as_str().map(str::to_owned))
                        .collect();
                }
                _ => {}
            }
        }
        state
    }

    /// Install a captured metadata mirror at the synchronous session commit.
    /// Other in-flight metadata owners retain their old, session-pinned Arc.
    pub fn install_session_metadata_source(&self, session_id: &str, rows: &[Utf16JsonProjection]) {
        self.metadata_states
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                canonical_metadata_session_id(session_id),
                Arc::new(Mutex::new(Self::metadata_from_source(rows))),
            );
    }

    async fn session_metadata_state(
        &self,
        session_id: &str,
        path: &Path,
    ) -> Arc<Mutex<SessionMetadataState>> {
        let session_id = canonical_metadata_session_id(session_id);
        if let Some(state) = self
            .metadata_states
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&session_id)
            .cloned()
        {
            return state;
        }
        // Direct library writers may not have a prepared replay snapshot.
        // Hydrate their first mirror from the complete target so old titles
        // outside the 64 KiB tail remain available to the backstop.
        let content = if tokio::fs::symlink_metadata(path)
            .await
            .is_ok_and(|metadata| metadata.file_type().is_file())
        {
            self.fs
                .read_file(&path.to_string_lossy(), None, None)
                .await
                .map(|file| file.content)
                .unwrap_or_default()
        } else {
            String::new()
        };
        let rows = Self::capture_session_metadata_source(&content, &session_id);
        self.metadata_states
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(session_id.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(Self::metadata_from_source(&rows))))
            .clone()
    }

    fn capture_active_target(&self) -> (PathBuf, Option<Arc<DurableTranscriptWriter>>) {
        self.active_durable_target().map_or_else(
            || (self.active_path(), None),
            |target| (target.path, Some(target.writer)),
        )
    }

    fn capture_metadata_target(
        &self,
        session_id: &str,
    ) -> Result<(PathBuf, Option<Arc<DurableTranscriptWriter>>), WriterError> {
        let session_id = canonical_metadata_session_id(session_id);
        let session_id = session_id.as_str();
        let active = self.capture_active_target();
        if Self::metadata_session_id_from_path(&active.0).as_deref() == Some(session_id) {
            return Ok(active);
        }
        let target = SessionId::parse_prefixed(session_id).and_then(|id| {
            self.session_targets
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&id)
                .cloned()
        });
        target
            .map(|target| (target.path, Some(target.writer)))
            .ok_or_else(|| {
                FsError::Io("metadata session has no bound transcript target".into()).into()
            })
    }

    /// Resolve again while holding the append mutex. A metadata operation may
    /// have waited for this session's slot while `/cd` moved its transcript.
    /// A different active UUID still leaves the captured owner's file intact.
    fn admitted_metadata_target(
        &self,
        session_id: &str,
        captured: (PathBuf, Option<Arc<DurableTranscriptWriter>>),
    ) -> (PathBuf, Option<Arc<DurableTranscriptWriter>>) {
        self.capture_metadata_target(session_id).unwrap_or(captured)
    }

    /// Atomically switch subsequent appends to another session transcript.
    ///
    /// The append mutex makes the boundary explicit: an append already in
    /// progress finishes on the previous file before this method returns.
    pub async fn retarget(&self, path: PathBuf) {
        if self.persistent_chat.is_some() {
            if self.active_path() != path {
                let _ = self.reject_persistent_chat_mutation("transcript retarget");
            }
            return;
        }
        let _g = self.lock.lock().await;
        *self
            .active_path
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = path;
    }

    /// Record that the live session moved to `relocated_cwd`, then retarget
    /// subsequent appends to `path`.
    ///
    /// A directory change must keep one session in one transcript.  When the
    /// project path changes, the existing file is rehomed before the marker is
    /// appended; otherwise a resume from the new cwd would select a fresh
    /// partial file and silently lose the pre-`/cd` history.  If the sanitized
    /// project path is unchanged, the marker is appended in place because it
    /// is the only durable, lossless signal that lets the session index
    /// distinguish the new cwd from the first message's cwd.
    ///
    /// The metadata mirror is updated as part of the same state transition, so
    /// a later metadata backstop keeps the relocation marker near the tail.
    /// When the project path changes, byte backstop counters are reset because
    /// they belong to the old transcript rather than the newly targeted file.
    pub async fn retarget_with_relocation(
        &self,
        path: PathBuf,
        session_id: &str,
        relocated_cwd: &str,
    ) -> Result<(), WriterError> {
        self.reject_persistent_chat_mutation("transcript relocation")?;
        if let Some(durable_target) = self.active_durable_target() {
            return self
                .retarget_with_relocation_durable(
                    path,
                    session_id.to_string(),
                    relocated_cwd.to_string(),
                    durable_target.writer,
                )
                .await;
        }
        self.retarget_without_durable_transaction(path, session_id, relocated_cwd)
            .await
    }

    /// Relocate through the same rooted session transaction used by durable
    /// ordinary/outbox appends.  The blocking closure owns the transaction
    /// lock across source inspection, destination quarantine, move, target
    /// resolution and marker fsync; a concurrent outbox therefore cannot
    /// capture the old path between relocation's metadata and append locks.
    async fn retarget_with_relocation_durable(
        &self,
        path: PathBuf,
        session_id: String,
        relocated_cwd: String,
        durable_lock: Arc<DurableTranscriptWriter>,
    ) -> Result<(), WriterError> {
        let target_session_id = SessionId::parse_prefixed(&session_id);
        let line = serde_json::to_string(&serde_json::json!({
            "type": "relocated",
            "relocatedCwd": relocated_cwd,
            "sessionId": session_id,
        }))?;
        let mut payload = String::with_capacity(line.len() + 1);
        payload.push_str(&line);
        payload.push('\n');

        let metadata_slot = self
            .session_metadata_state(&session_id, &self.active_path())
            .await;
        let mut metadata_state = metadata_slot.lock().await;
        let _writer_guard = self.lock.lock().await;
        let old_path = self.active_path();
        let same_path = old_path == path;
        let shared_root = if same_path {
            None
        } else {
            Some(relocation_root(&old_path, &path).map_err(|error| {
                WriterError::Fs(FsError::Io(format!(
                    "unsafe transcript relocation path: {error}"
                )))
            })?)
        };

        let operation_path = path.clone();
        let operation_old_path = old_path.clone();
        let operation_payload = payload;
        let operation_root = shared_root.clone();
        let operation_durable_lock = durable_lock.clone();
        let operation = tokio::task::spawn_blocking(move || {
            operation_durable_lock.with_transaction(|transaction| {
                let old_exists = match std::fs::symlink_metadata(&operation_old_path) {
                    Ok(metadata) if metadata.file_type().is_file() => true,
                    Ok(_) => {
                        return Err(TranscriptWriterError::Fs(FsError::Io(format!(
                            "transcript source is not a regular file: {}",
                            operation_old_path.display()
                        ))));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                    Err(error) => {
                        return Err(TranscriptWriterError::Fs(FsError::Io(format!(
                            "could not inspect transcript source: {error}"
                        ))));
                    }
                };
                let mut moved_existing = false;

                if !same_path {
                    if let Some(parent) = operation_path.parent() {
                        if !parent.as_os_str().is_empty() && !parent.exists() {
                            #[cfg(unix)]
                            {
                                use std::os::unix::fs::DirBuilderExt;
                                std::fs::DirBuilder::new()
                                    .recursive(true)
                                    .mode(0o700)
                                    .create(parent)
                                    .map_err(|error| {
                                        TranscriptWriterError::Fs(FsError::Io(error.to_string()))
                                    })?;
                            }
                            #[cfg(not(unix))]
                            std::fs::create_dir_all(parent).map_err(|error| {
                                TranscriptWriterError::Fs(FsError::Io(error.to_string()))
                            })?;
                        }
                    }

                    let root = operation_root
                        .as_deref()
                        .expect("different transcript paths have a shared root");
                    validate_relocation_parents(
                        root,
                        &operation_old_path,
                        &operation_path,
                        old_exists,
                    )
                    .map_err(|error| {
                        TranscriptWriterError::Fs(FsError::Io(format!(
                            "unsafe transcript relocation parent: {error}"
                        )))
                    })?;
                    let target_exists = match std::fs::symlink_metadata(&operation_path) {
                        Ok(_) => true,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                        Err(error) => {
                            return Err(TranscriptWriterError::Fs(FsError::Io(format!(
                                "could not inspect transcript destination: {error}"
                            ))));
                        }
                    };
                    let superseded = if target_exists {
                        Some(move_to_superseded_path(&operation_path).map_err(|error| {
                            TranscriptWriterError::Fs(FsError::Io(format!(
                                "transcript destination quarantine failed: {error}"
                            )))
                        })?)
                    } else {
                        None
                    };

                    if !old_exists {
                        if let Some(superseded) = superseded {
                            if let Err(error) = std::fs::rename(&superseded, &operation_path) {
                                tracing::warn!(
                                    path = %operation_path.display(),
                                    %error,
                                    "failed to restore occupied transcript destination"
                                );
                            }
                            return Err(TranscriptWriterError::Fs(FsError::Io(format!(
                                "transcript source missing and destination occupied: {}",
                                operation_path.display()
                            ))));
                        }
                    } else {
                        let sidecars = match relocate_identity_sidecars(
                            &operation_old_path,
                            &operation_path,
                        ) {
                            Ok(sidecars) => sidecars,
                            Err(error) => {
                                if let Some(superseded) = superseded.as_ref() {
                                    let _ = std::fs::rename(superseded, &operation_path);
                                }
                                return Err(TranscriptWriterError::Fs(FsError::Io(format!(
                                    "Host identity sidecar relocation failed: {error}"
                                ))));
                            }
                        };
                        if let Err(error) = move_file_with_cross_device_fallback(
                            &operation_old_path,
                            &operation_path,
                        ) {
                            sidecars.rollback();
                            if let Some(superseded) = superseded.as_ref() {
                                let _ = std::fs::rename(superseded, &operation_path);
                            }
                            return Err(TranscriptWriterError::Fs(FsError::Io(format!(
                                "transcript move failed: {error}"
                            ))));
                        }
                        moved_existing = true;
                    }

                    if !old_exists {
                        validate_real_parent_chain(
                            root,
                            operation_path
                                .parent()
                                .expect("transcript destination has a parent"),
                        )
                        .map_err(|error| {
                            TranscriptWriterError::Fs(FsError::Io(format!(
                                "unsafe transcript relocation parent: {error}"
                            )))
                        })?;
                    }
                }

                let should_append_marker = (same_path && old_exists) || moved_existing;
                if should_append_marker {
                    let parent = operation_path.parent().ok_or_else(|| {
                        TranscriptWriterError::Fs(FsError::Io(
                            "transcript destination has no parent".into(),
                        ))
                    })?;
                    let identity = lingxi_core::host::rooted_fs::root_identity(parent)?;
                    let relative =
                        operation_path
                            .file_name()
                            .map(PathBuf::from)
                            .ok_or_else(|| {
                                TranscriptWriterError::Fs(FsError::Io(
                                    "transcript destination has no file name".into(),
                                ))
                            })?;
                    if let Err(error) = transaction.append_raw_json_at(
                        parent,
                        &identity,
                        &relative,
                        Utf16JsonProjection::parse(&operation_payload).map_err(|error| {
                            TranscriptWriterError::Fs(FsError::Io(error.to_string()))
                        })?,
                    ) {
                        tracing::warn!(
                            path = %operation_path.display(),
                            %error,
                            "transcript relocation marker append failed"
                        );
                    }
                }
                Ok((old_exists, moved_existing))
            })
        })
        .await
        .map_err(|error| WriterError::Fs(FsError::Io(error.to_string())))??;

        let (_, moved_existing) = operation;
        if let Some(session_id) = target_session_id {
            let target = DurableTranscriptTarget {
                path: path.clone(),
                writer: durable_lock.clone(),
                cwd: PathBuf::from(&relocated_cwd),
            };
            self.session_targets
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(session_id, target.clone());
            *self
                .active_durable_target
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(target);
        }
        *self
            .active_path
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = path.clone();
        metadata_state.relocated_cwd = Some(relocated_cwd);
        if !same_path {
            self.bytes_since_metadata_re_append
                .store(0, Ordering::Relaxed);
            self.bytes_since_compact.store(0, Ordering::Relaxed);
        }
        drop(_writer_guard);
        drop(metadata_state);
        if same_path || moved_existing {
            self.maybe_re_append_metadata().await;
        }
        Ok(())
    }

    async fn retarget_without_durable_transaction(
        &self,
        path: PathBuf,
        session_id: &str,
        relocated_cwd: &str,
    ) -> Result<(), WriterError> {
        let line = serde_json::to_string(&serde_json::json!({
            "type": "relocated",
            "relocatedCwd": relocated_cwd,
            "sessionId": session_id,
        }))?;
        let mut payload = String::with_capacity(line.len() + 1);
        payload.push_str(&line);
        payload.push('\n');

        // Keep lock ordering consistent with maybe_re_append_metadata:
        // metadata_state -> append lock.  No append path takes these locks in
        // the opposite order while the metadata guard is held.
        let metadata_slot = self
            .session_metadata_state(session_id, &self.active_path())
            .await;
        let mut metadata_state = metadata_slot.lock().await;
        let _g = self.lock.lock().await;
        let old_path = self.active_path();
        let same_path = old_path == path;
        let relocation_root = if same_path {
            None
        } else {
            Some(relocation_root(&old_path, &path).map_err(|error| {
                FsError::Io(format!("unsafe transcript relocation path: {error}"))
            })?)
        };

        let old_exists = match std::fs::symlink_metadata(&old_path) {
            // `symlink_metadata` deliberately inspects the directory entry
            // itself.  A relocation may only rehome the regular transcript
            // file; accepting a directory or symlink here would let `/cd`
            // rename an unrelated tree or redirect the move outside the
            // session root.  Keep this validation before touching the target
            // so a failed relocation is entirely side-effect free.
            Ok(metadata) if metadata.file_type().is_file() => true,
            Ok(_) => {
                return Err(WriterError::Fs(FsError::Io(format!(
                    "transcript source is not a regular file: {}",
                    old_path.display()
                ))));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                return Err(WriterError::Fs(FsError::Io(format!(
                    "could not inspect transcript source: {error}"
                ))));
            }
        };
        let mut moved_existing = false;
        if !same_path {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() && !parent.exists() {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::DirBuilderExt;
                        std::fs::DirBuilder::new()
                            .recursive(true)
                            .mode(0o700)
                            .create(parent)
                            .map_err(|e| FsError::Io(e.to_string()))?;
                    }
                    #[cfg(not(unix))]
                    std::fs::create_dir_all(parent).map_err(|e| FsError::Io(e.to_string()))?;
                }
            }

            validate_relocation_parents(
                relocation_root
                    .as_deref()
                    .expect("different paths have a relocation root"),
                &old_path,
                &path,
                old_exists,
            )
            .map_err(|error| {
                FsError::Io(format!("unsafe transcript relocation parent: {error}"))
            })?;

            // A missing source has no sidecar to move. In particular, do not
            // create/open a lock below its nonexistent parent: the transcript
            // relocation contract validates paths and reports an occupied
            // destination before Host-only identity bookkeeping is relevant.
            let _old_identity_guard = if old_exists {
                Some(self.identity_sidecar_guard(&old_path).await?)
            } else {
                None
            };
            let _new_identity_guard = if old_exists {
                Some(self.identity_sidecar_guard(&path).await?)
            } else {
                None
            };

            // A stale/occupied destination must never be overwritten. Set it
            // aside under a non-JSONL suffix so session discovery cannot treat
            // it as the active transcript. If the move fails, put it back. Do
            // this before checking the source: an absent source must not make
            // us silently adopt an unrelated file already at the target.
            let target_exists = match std::fs::symlink_metadata(&path) {
                Ok(_) => true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => {
                    return Err(WriterError::Fs(FsError::Io(format!(
                        "could not inspect transcript destination: {error}"
                    ))));
                }
            };
            let superseded = if target_exists {
                Some(move_to_superseded_path(&path).map_err(|e| {
                    FsError::Io(format!("transcript destination quarantine failed: {e}"))
                })?)
            } else {
                None
            };

            if !old_exists {
                if let Some(superseded) = superseded {
                    if let Err(error) = std::fs::rename(&superseded, &path) {
                        tracing::warn!(
                            path = %path.display(),
                            %error,
                            "failed to restore occupied transcript destination"
                        );
                    }
                    return Err(WriterError::Fs(FsError::Io(format!(
                        "transcript source missing and destination occupied: {}",
                        path.display()
                    ))));
                }
                // Claude retargets a writer whose old file disappeared, but
                // deliberately skips the relocation marker; the first future
                // append will create the target transcript.
            } else {
                // `JsonlWriter` is only constructed with the host path selected
                // by the session composition root. Rehome the regular
                // transcript atomically; on EXDEV (e.g. a mounted config
                // directory), copy the bytes durably and remove the source
                // only after the copy succeeds.
                if let Err(error) = validate_relocation_parents(
                    relocation_root
                        .as_deref()
                        .expect("different paths have a relocation root"),
                    &old_path,
                    &path,
                    true,
                ) {
                    if let Some(superseded) = superseded.as_ref() {
                        let _ = std::fs::rename(superseded, &path);
                    }
                    return Err(WriterError::Fs(FsError::Io(format!(
                        "unsafe transcript relocation parent: {error}"
                    ))));
                }
                let sidecars = match relocate_identity_sidecars(&old_path, &path) {
                    Ok(sidecars) => sidecars,
                    Err(error) => {
                        if let Some(superseded) = superseded.as_ref() {
                            let _ = std::fs::rename(superseded, &path);
                        }
                        return Err(WriterError::Fs(FsError::Io(format!(
                            "Host identity sidecar relocation failed: {error}"
                        ))));
                    }
                };
                if let Err(error) = move_file_with_cross_device_fallback(&old_path, &path) {
                    sidecars.rollback();
                    if let Some(superseded) = superseded.as_ref() {
                        let _ = std::fs::rename(superseded, &path);
                    }
                    return Err(WriterError::Fs(FsError::Io(format!(
                        "transcript move failed: {error}"
                    ))));
                }
                moved_existing = true;
            }

            // With no source there is no rename seam at which to perform the
            // second check above. Revalidate immediately before publishing the
            // future append path so a parent swapped after the first check is
            // not accepted as the writer's new destination.
            if !old_exists {
                validate_real_parent_chain(
                    relocation_root
                        .as_deref()
                        .expect("different paths have a relocation root"),
                    path.parent()
                        .expect("a transcript destination always has a parent"),
                )
                .map_err(|error| {
                    FsError::Io(format!("unsafe transcript relocation parent: {error}"))
                })?;
            }
        }

        // Publish the target before writing the marker.  Marker persistence is
        // deliberately best-effort in Claude: a failed marker must not turn an
        // already-completed cwd move into a split-brain rollback.  In
        // particular, when the old transcript is gone, do not create a
        // marker-only file in the new project directory.
        *self
            .active_path
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = path;
        let should_append_marker = (same_path && old_exists) || moved_existing;
        if should_append_marker {
            let active_path = self.active_path();
            if let Err(error) = self
                .append_payload_to_path(&active_path, &payload, true)
                .await
            {
                tracing::warn!(
                    path = %active_path.display(),
                    %error,
                    "transcript relocation marker append failed"
                );
            }
        }
        metadata_state.relocated_cwd = Some(relocated_cwd.to_string());
        if !same_path {
            self.bytes_since_metadata_re_append
                .store(0, Ordering::Relaxed);
            self.bytes_since_compact.store(0, Ordering::Relaxed);
        }
        drop(_g);
        drop(metadata_state);

        // A same-directory move may have crossed the metadata backstop while
        // writing the marker.  Poll outside the critical section just like all
        // other public writer paths.
        self.maybe_re_append_metadata().await;
        Ok(())
    }

    /// Append one JSONL line — `serde_json::to_string(msg) + "\n"`.
    ///
    /// Creates the parent directory on first call. The `FileSystem` trait
    /// in M1 does not expose `mkdir_p`; we use `tokio::fs::create_dir_all`
    /// directly because parent-dir creation is not a sandboxed operation we
    /// virtualize for tests (each `FileSystem` impl that hosts real files
    /// would do the same syscall internally). M5-08 may extend the trait.
    pub async fn append(&self, msg: &JsonlMessage) -> Result<(), WriterError> {
        if self.active_durable_target().is_some() {
            let stamped = stamp_session_kind(msg);
            let msg = stamped.as_ref().unwrap_or(msg);
            let payload = native_message_projection(msg)?;
            let payload_bytes = payload
                .to_json_string()
                .map_err(crate::jsonl::exact_json::projection_error)?
                .len()
                + 1;
            self.append_json_durable(payload, Some(msg.uuid.clone()))
                .await?;
            self.bytes_since_metadata_re_append
                .fetch_add(payload_bytes, Ordering::Relaxed);
            self.bytes_since_compact
                .fetch_add(payload_bytes as u64, Ordering::Relaxed);
        } else {
            let _g = self.lock.lock().await;
            self.flush_queue_operations_locked().await?;
            let stamped = stamp_session_kind(msg);
            let line = String::from_utf8(native_message_bytes(stamped.as_ref().unwrap_or(msg))?)
                .expect("native JSON encoder emits UTF-8");
            let mut payload = String::with_capacity(line.len() + 1);
            payload.push_str(&line);
            payload.push('\n');
            let path = self.active_path();
            let _identity_guard = self
                .append_identity_non_durable(&path, Some(&msg.uuid))
                .await?;
            self.append_payload(&payload).await?;
        }
        // Drive both backstops from the ordinary append path. Deliberately
        // AFTER the critical section above: each of these re-takes
        // `self.lock`, so polling inside would deadlock.
        //
        // Order matches the oracle's `drainQueuesOnce` tail (@296777200):
        // the transcript rewrite runs FIRST, then the metadata re-append —
        // otherwise the re-append's freshly written records are the ones the
        // rewrite would immediately supersede.
        self.maybe_compact_transcript(writes_compact_boundary(msg))
            .await;
        self.maybe_re_append_metadata().await;
        Ok(())
    }

    /// Append one JSON object through the active transcript path while holding
    /// the coordinator's durable session-state transaction. The active path
    /// is resolved *inside* that transaction, so a concurrent `/cd` retarget
    /// cannot split an ordinary append and a Fusion outbox delivery.
    pub async fn append_json_once_durable(
        &self,
        delivery_id: &str,
        payload: Utf16JsonProjection,
    ) -> Result<TranscriptAppendOutcome, TranscriptWriterError> {
        let _guard = self.lock.lock().await;
        let Some(target) = self.active_durable_target() else {
            return Err(TranscriptWriterError::Fs(FsError::Io(
                "durable transcript target is not configured".into(),
            )));
        };
        self.append_json_once_durable_locked(
            target.path,
            target.writer,
            delivery_id,
            payload,
            false,
        )
        .await
        .map(|(outcome, _)| outcome)
    }

    /// Publish a prepared native row once by its immutable outer UUID. Exact
    /// tool-input keys/strings stay attached through duplicate comparison and
    /// the same bound session transaction; no deliveryId is added to the row.
    pub async fn append_native_message_once(
        &self,
        message: &JsonlMessage,
    ) -> Result<(TranscriptAppendOutcome, bool), TranscriptWriterError> {
        let session_id = SessionId::parse_prefixed(&message.session_id).ok_or_else(|| {
            TranscriptWriterError::Fs(FsError::Io("native row has an invalid session id".into()))
        })?;
        let _guard = self.lock.lock().await;
        self.flush_queue_operations_locked()
            .await
            .map_err(queue_journal_error)?;
        let target = self
            .session_targets
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&session_id)
            .cloned()
            .ok_or_else(|| {
                TranscriptWriterError::Fs(FsError::Io(format!(
                    "durable transcript target is not bound for session {session_id}"
                )))
            })?;
        let stamped = stamp_session_kind(message);
        let message = stamped.as_ref().unwrap_or(message);
        let payload = native_message_projection(message)?;
        self.append_json_once_durable_locked(
            target.path,
            target.writer,
            &message.uuid,
            payload,
            true,
        )
        .await
    }

    /// Persist one transcript row from a durable pending prompt exactly once. The
    /// stable UUID and all source fields are checked under the transcript
    /// transaction; only its regenerated timestamp and derived parent are
    /// ignored on a retry. The returned UUID is the transcript's actual tail
    /// observed by that same transaction, including when this row was already
    /// present earlier in the file.
    pub async fn append_pending_prompt_message_once(
        &self,
        message: &JsonlMessage,
    ) -> Result<(TranscriptAppendOutcome, Option<String>), TranscriptWriterError> {
        let session_id = SessionId::parse_prefixed(&message.session_id).ok_or_else(|| {
            TranscriptWriterError::Fs(FsError::Io(
                "pending prompt row has an invalid session id".into(),
            ))
        })?;
        let _guard = self.lock.lock().await;
        self.flush_queue_operations_locked()
            .await
            .map_err(queue_journal_error)?;
        let target = self
            .session_targets
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&session_id)
            .cloned()
            .ok_or_else(|| {
                TranscriptWriterError::Fs(FsError::Io(format!(
                    "durable transcript target is not bound for session {session_id}"
                )))
            })?;
        let stamped = stamp_session_kind(message);
        let message = stamped.as_ref().unwrap_or(message);
        let payload = native_message_projection(message)?;
        let message_uuid = message.uuid.clone();
        let identity_store = self.identity_store.clone();
        tokio::task::spawn_blocking(move || {
            target.writer.with_transaction(|transaction| {
                let parent = target.path.parent().ok_or_else(|| {
                    TranscriptWriterError::Fs(FsError::Io(
                        "active transcript path has no parent".into(),
                    ))
                })?;
                std::fs::create_dir_all(parent)
                    .map_err(|error| TranscriptWriterError::Fs(FsError::Io(error.to_string())))?;
                let identity = lingxi_core::host::rooted_fs::root_identity(parent)?;
                let relative = target.path.file_name().map(PathBuf::from).ok_or_else(|| {
                    TranscriptWriterError::Fs(FsError::Io(
                        "active transcript path has no file name".into(),
                    ))
                })?;
                transaction.append_pending_prompt_json_once_at_with_tip_identity(
                    parent,
                    &identity,
                    &relative,
                    &message_uuid,
                    payload,
                    &identity_store,
                    &target.path,
                )
            })
        })
        .await
        .map_err(|error| TranscriptWriterError::Fs(FsError::Io(error.to_string())))?
    }

    /// Append once for a run pinned to its originating session. The target is
    /// looked up only after acquiring the ordinary writer mutex, while the
    /// durable transaction then serializes it with `/cd` relocation and other
    /// cross-process writers.
    pub async fn append_json_once_durable_for_session(
        &self,
        session_id: SessionId,
        delivery_id: &str,
        payload: Utf16JsonProjection,
    ) -> Result<TranscriptAppendOutcome, TranscriptWriterError> {
        self.append_json_once_durable_for_session_with_tip(session_id, delivery_id, payload)
            .await
            .map(|(outcome, _)| outcome)
    }

    /// Append to a pinned session and return whether the acknowledged delivery
    /// is still its durable UUID tip. The observation shares the bounded
    /// duplicate scan and transaction lock with the append itself. Callers
    /// reconciling live cursors must also serialize their foreground turns.
    pub async fn append_json_once_durable_for_session_with_tip(
        &self,
        session_id: SessionId,
        delivery_id: &str,
        payload: Utf16JsonProjection,
    ) -> Result<(TranscriptAppendOutcome, bool), TranscriptWriterError> {
        self.append_json_once_durable_for_session_with_tip_mode(
            session_id,
            delivery_id,
            payload,
            false,
        )
        .await
    }

    /// Append a prepared Native row without adding the Fusion deliveryId.
    pub async fn append_native_json_once_durable_for_session_with_tip(
        &self,
        session_id: SessionId,
        delivery_id: &str,
        payload: Utf16JsonProjection,
    ) -> Result<(TranscriptAppendOutcome, bool), TranscriptWriterError> {
        self.append_json_once_durable_for_session_with_tip_mode(
            session_id,
            delivery_id,
            payload,
            true,
        )
        .await
    }

    async fn append_json_once_durable_for_session_with_tip_mode(
        &self,
        session_id: SessionId,
        delivery_id: &str,
        payload: Utf16JsonProjection,
        native_uuid_only: bool,
    ) -> Result<(TranscriptAppendOutcome, bool), TranscriptWriterError> {
        let _guard = self.lock.lock().await;
        let target = self
            .session_targets
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&session_id)
            .cloned();
        let target = target.ok_or_else(|| {
            TranscriptWriterError::Fs(FsError::Io(format!(
                "durable transcript target is not bound for session {session_id}"
            )))
        })?;
        self.append_json_once_durable_locked(
            target.path,
            target.writer,
            delivery_id,
            payload,
            native_uuid_only,
        )
        .await
    }

    /// Append an ordinary transcript record under the active durable
    /// transaction. Unlike Fusion delivery this deliberately performs no UUID
    /// scan: ordinary history is append-only and its loader preserves the
    /// established last-write-wins semantics for repeated UUID updates.
    async fn append_json_durable(
        &self,
        payload: Utf16JsonProjection,
        identity_uuid: Option<String>,
    ) -> Result<(), TranscriptWriterError> {
        let _guard = self.lock.lock().await;
        self.flush_queue_operations_locked()
            .await
            .map_err(queue_journal_error)?;
        let Some(target) = self.active_durable_target() else {
            return Err(TranscriptWriterError::Fs(FsError::Io(
                "durable transcript target is not configured".into(),
            )));
        };
        self.append_json_durable_locked(target.path, target.writer, payload, identity_uuid)
            .await
    }

    async fn append_json_durable_for_session(
        &self,
        session_id: SessionId,
        payload: Utf16JsonProjection,
        identity_uuid: Option<String>,
    ) -> Result<(), TranscriptWriterError> {
        let _guard = self.lock.lock().await;
        let target = self
            .session_targets
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&session_id)
            .cloned()
            .ok_or_else(|| {
                TranscriptWriterError::Fs(FsError::Io(format!(
                    "durable transcript target is not bound for session {session_id}"
                )))
            })?;
        self.append_json_durable_locked(target.path, target.writer, payload, identity_uuid)
            .await
    }

    async fn append_json_durable_locked(
        &self,
        active_path: PathBuf,
        durable_lock: Arc<DurableTranscriptWriter>,
        payload: Utf16JsonProjection,
        identity_uuid: Option<String>,
    ) -> Result<(), TranscriptWriterError> {
        let identity_store = self.identity_store.clone();
        tokio::task::spawn_blocking(move || {
            durable_lock.with_transaction(|transaction| {
                let parent = active_path.parent().ok_or_else(|| {
                    TranscriptWriterError::Fs(FsError::Io(
                        "active transcript path has no parent".into(),
                    ))
                })?;
                std::fs::create_dir_all(parent)
                    .map_err(|error| TranscriptWriterError::Fs(FsError::Io(error.to_string())))?;
                let identity = lingxi_core::host::rooted_fs::root_identity(parent)?;
                let relative = active_path.file_name().map(PathBuf::from).ok_or_else(|| {
                    TranscriptWriterError::Fs(FsError::Io(
                        "active transcript path has no file name".into(),
                    ))
                })?;
                if let Some(uuid) = identity_uuid.as_deref() {
                    message_identity::append_row_identity_at(
                        &identity_store,
                        &active_path,
                        parent,
                        &identity,
                        uuid,
                    )?;
                } else if payload.string_at("/type") == Some("queue-operation") {
                    message_identity::initialize_empty_host_log_at(
                        &identity_store,
                        &active_path,
                        parent,
                        &identity,
                    )?;
                } else {
                    initialize_identity_for_fresh_metadata(
                        &identity_store,
                        &active_path,
                        parent,
                        &identity,
                    )?;
                }
                transaction.append_raw_json_at(parent, &identity, &relative, payload)
            })
        })
        .await
        .map_err(|error| TranscriptWriterError::Fs(FsError::Io(error.to_string())))?
    }

    async fn append_json_once_durable_locked(
        &self,
        active_path: PathBuf,
        durable_lock: Arc<DurableTranscriptWriter>,
        delivery_id: &str,
        payload: Utf16JsonProjection,
        native_uuid_only: bool,
    ) -> Result<(TranscriptAppendOutcome, bool), TranscriptWriterError> {
        let delivery_id = delivery_id.to_string();
        let identity_store = self.identity_store.clone();
        tokio::task::spawn_blocking(move || {
            durable_lock.with_transaction(|transaction| {
                let parent = active_path.parent().ok_or_else(|| {
                    TranscriptWriterError::Fs(FsError::Io(
                        "active transcript path has no parent".into(),
                    ))
                })?;
                std::fs::create_dir_all(parent)
                    .map_err(|error| TranscriptWriterError::Fs(FsError::Io(error.to_string())))?;
                let identity = lingxi_core::host::rooted_fs::root_identity(parent)?;
                let relative = active_path.file_name().map(PathBuf::from).ok_or_else(|| {
                    TranscriptWriterError::Fs(FsError::Io(
                        "active transcript path has no file name".into(),
                    ))
                })?;
                if native_uuid_only {
                    transaction.append_native_json_once_at_with_tip_identity(
                        parent,
                        &identity,
                        &relative,
                        &delivery_id,
                        payload,
                        &identity_store,
                        &active_path,
                    )
                } else {
                    transaction.append_json_once_at_with_tip_identity(
                        parent,
                        &identity,
                        &relative,
                        &delivery_id,
                        payload,
                        &identity_store,
                        &active_path,
                    )
                }
            })
        })
        .await
        .map_err(|error| TranscriptWriterError::Fs(FsError::Io(error.to_string())))?
    }

    /// Append one line to an explicit session transcript without retargeting
    /// the writer's active session. The shared append lock prevents an
    /// in-process current-session write from interleaving with this line.
    pub async fn append_to_path(&self, path: &Path, msg: &JsonlMessage) -> Result<(), WriterError> {
        if self.active_path() == path {
            return self.append(msg).await;
        }

        if self.durable_transcript_enabled() {
            let session_id = SessionId::parse_prefixed(&msg.session_id).ok_or_else(|| {
                WriterError::Fs(FsError::Io(format!(
                    "durable transcript message has invalid session id {:?}",
                    msg.session_id
                )))
            })?;
            let stamped = stamp_session_kind(msg);
            let msg = stamped.as_ref().unwrap_or(msg);
            let payload = native_message_projection(msg)?;
            self.append_json_durable_for_session(session_id, payload, Some(msg.uuid.clone()))
                .await?;
            return Ok(());
        }

        let _g = self.lock.lock().await;
        let stamped = stamp_session_kind(msg);
        let line = String::from_utf8(native_message_bytes(stamped.as_ref().unwrap_or(msg))?)
            .expect("native JSON encoder emits UTF-8");
        let mut payload = String::with_capacity(line.len() + 1);
        payload.push_str(&line);
        payload.push('\n');
        let _identity_guard = self
            .append_identity_non_durable(path, Some(&msg.uuid))
            .await?;
        self.append_payload_to_path(path, &payload, false).await
    }

    /// Write `payload` verbatim to the active transcript and account it against
    /// the metadata-re-append backstop counter.
    ///
    /// The caller MUST already hold [`Self::lock`] — this is the shared body of
    /// every append path and takes no lock of its own so that
    /// [`Self::re_append_session_metadata`] can read the tail and write the
    /// plan under one critical section.
    async fn append_payload(&self, payload: &str) -> Result<(), WriterError> {
        let path = self.active_path();
        self.append_payload_to_path(&path, payload, true).await
    }

    /// Write `payload` to an explicit path while the caller holds
    /// [`Self::lock`]. `account_backstops` is false when the write is the final
    /// record on an old path during a retarget; those bytes must not arm
    /// backstops for the newly selected transcript.
    async fn append_payload_to_path(
        &self,
        path: &Path,
        payload: &str,
        account_backstops: bool,
    ) -> Result<(), WriterError> {
        if self.append_persistent_chat_payload(path, payload).await? {
            if account_backstops {
                self.bytes_since_metadata_re_append
                    .fetch_add(payload.len(), Ordering::Relaxed);
            }
            return Ok(());
        }
        let path_str = path.to_str().expect("session paths are UTF-8");
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                // Sync std::fs is fine here — we already hold the in-process
                // mutex and parent-dir creation is a one-shot syscall.
                // claude-code `appendToFile` creates the project dir with
                // `{ mode: 0o700 }` (owner-only); mirror that on unix.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    std::fs::DirBuilder::new()
                        .recursive(true)
                        .mode(0o700)
                        .create(parent)
                        .map_err(|e| FsError::Io(e.to_string()))?;
                }
                #[cfg(not(unix))]
                std::fs::create_dir_all(parent).map_err(|e| FsError::Io(e.to_string()))?;
            }
        }
        // claude-code `appendToFile`: `fsAppendFile(path, data, { mode: 0o600 })`
        // — the `<uuid>.jsonl` transcript is owner-only (prompt + tool content).
        self.fs
            .append_file_with_mode(path_str, payload, 0o600)
            .await?;
        // `bytesSinceMetadataReAppend += Buffer.byteLength(t,"utf8")` — counted
        // only on a SUCCESSFUL write, matching the oracle's post-await position.
        // `appendToFile` (@296775839) bumps BOTH counters from the same
        // `Buffer.byteLength`, so they never drift apart.
        if account_backstops {
            self.bytes_since_metadata_re_append
                .fetch_add(payload.len(), Ordering::Relaxed);
            self.bytes_since_compact
                .fetch_add(payload.len() as u64, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Bytes appended since the last metadata re-append
    /// (`bytesSinceMetadataReAppend`).
    #[must_use]
    pub fn bytes_since_metadata_re_append(&self) -> usize {
        self.bytes_since_metadata_re_append.load(Ordering::Relaxed)
    }

    /// `true` once [`METADATA_REAPPEND_BACKSTOP_BYTES`] have been appended since
    /// the last re-append — the oracle's periodic backstop condition at the tail
    /// of `drainQueuesOnce` (`if(this.bytesSinceMetadataReAppend>=kI/2)`,
    /// 2.1.220 @237851998), which fires
    /// `reAppendSessionMetadataAsync(false, /*skip_dedup=*/true)`.
    #[must_use]
    pub fn metadata_re_append_due(&self) -> bool {
        self.bytes_since_metadata_re_append() >= METADATA_REAPPEND_BACKSTOP_BYTES
    }

    /// Clear the backstop counter without writing — the oracle's
    /// `resetSessionFile()` also zeroes `bytesSinceMetadataReAppend`.
    pub fn reset_metadata_re_append_counter(&self) {
        self.bytes_since_metadata_re_append
            .store(0, Ordering::Relaxed);
    }

    /// The session id this writer is writing, i.e. the transcript's file stem.
    ///
    /// The oracle passes the id in; here the writer already knows which session
    /// it targets, so nothing has to thread it. `<uuid>.jsonl` -> `<uuid>`,
    /// which is exactly the BARE form the loader keys its maps by (see
    /// [`Self::append_custom_title`]'s contract).
    fn session_id_from_path(&self) -> Option<String> {
        Self::metadata_session_id_from_path(&self.active_path())
    }

    fn metadata_session_id_from_path(path: &Path) -> Option<String> {
        path.file_stem()
            .and_then(|s| s.to_str())
            .map(canonical_metadata_session_id)
    }

    /// Fire the metadata backstop if enough bytes have accumulated.
    ///
    /// THIS is what makes the port live. Without it `re_append_session_metadata`
    /// is a mechanism nobody invokes, and metadata still scrolls out of the
    /// 64 KiB window that every session-index reader scans.
    ///
    /// Uses the backstop polarity `(skip_title_adopt: false, skip_dedup: true)`
    /// — the oracle's periodic/post-compaction call. Best-effort: a write error
    /// here must not fail the append that triggered it, so it is swallowed (the
    /// counter has already been reset, so the next window retries).
    ///
    /// Takes NO lock of its own; `re_append_session_metadata` takes it.
    pub async fn maybe_re_append_metadata(&self) -> usize {
        if !self.metadata_re_append_due() {
            return 0;
        }
        let target = self.capture_active_target();
        let Some(sid) = Self::metadata_session_id_from_path(&target.0) else {
            return 0;
        };
        let slot = self.session_metadata_state(&sid, &target.0).await;
        let mut state = slot.lock().await;
        match self
            .re_append_session_metadata_to_target(&mut state, &sid, false, true, target)
            .await
        {
            Ok(n) => n,
            Err(e) => {
                // `catch(e){…w(`Metadata re-append failed (${$t(e)}): ${le(e)}`,
                // {level:"error"})…}` — the oracle LOGS this rather than letting
                // it escape, because the append that triggered the backstop has
                // already succeeded and must not fail retroactively.
                tracing::error!("Metadata re-append failed: {e}");
                0
            }
        }
    }

    /// Bytes appended since the last successful transcript rewrite
    /// (`bytesSinceCompact`).
    #[must_use]
    pub fn bytes_since_compact(&self) -> u64 {
        self.bytes_since_compact.load(Ordering::Relaxed)
    }

    /// The current transcript-rewrite byte backstop (`backstopThresholdBytes`).
    #[must_use]
    pub fn compact_backstop_bytes(&self) -> u64 {
        self.compact_backstop_bytes.load(Ordering::Relaxed)
    }

    /// The RECLAMATION half of the metadata backstop — SC-08.
    ///
    /// THIS is what makes [`crate::jsonl::transcript_compact`] live. Without it
    /// the port ships only the growth side: every 32 KiB the metadata set is
    /// re-appended, and nothing ever removes the copy it superseded.
    ///
    /// Two triggers, both from the oracle:
    ///
    /// * `boundary_written` — a `compact_boundary` line was just persisted
    ///   (@296794903). The threshold is reset to [`COMPACT_BACKSTOP_BYTES`] and
    ///   a rewrite is requested immediately: a boundary is exactly the moment
    ///   the largest amount of the file became reclaimable.
    /// * the byte backstop — `bytesSinceCompact >= backstopThresholdBytes`
    ///   (@296777239).
    ///
    /// # Inert in a default install
    ///
    /// Gated on [`local_gc_enabled`], which is `false` unless
    /// `LINGXI_TRANSCRIPT_LOCAL_GC` is set — the same shape as upstream's
    /// `localGcEnabled`, whose only setter reads
    /// `CLAUDE_CODE_TRANSCRIPT_LOCAL_GC ?? gate("tengu_transcript_local_gc", false)`.
    /// So this costs one env read per append today and nothing else; flipping
    /// the env matches upstream with the gate on.
    ///
    /// Best-effort, like the metadata backstop: a rewrite failure must never
    /// fail the append that triggered it.
    pub async fn maybe_compact_transcript(&self, boundary_written: bool) -> Option<CompactStats> {
        if self.persistent_chat.is_some() {
            return None;
        }
        if !local_gc_enabled() {
            return None;
        }
        if boundary_written {
            self.compact_backstop_bytes
                .store(COMPACT_BACKSTOP_BYTES, Ordering::Relaxed);
        }
        let due = self.bytes_since_compact() >= self.compact_backstop_bytes();
        if !boundary_written && !due {
            return None;
        }
        if due {
            // `this.bytesSinceCompact=0, await this.performCompactTranscript(...)`
            // — the backstop path zeroes BEFORE the rewrite so a slow rewrite
            // cannot re-arm itself. The boundary path does not; the rewrite
            // zeroes it on success either way.
            self.bytes_since_compact.store(0, Ordering::Relaxed);
        }

        let outcome = {
            // Hold the append lock across the rewrite. The safety envelope
            // tolerates concurrent appends, but there is no reason to make it
            // work for appends THIS writer controls — and holding it keeps
            // `bytes_since_compact` honest.
            let _g = self.lock.lock().await;
            let path = self.active_path();
            match tokio::task::spawn_blocking(move || perform_compact_transcript(&path)).await {
                Ok(outcome) => outcome,
                Err(e) => {
                    tracing::warn!("Transcript compact failed (io): {e}");
                    return None;
                }
            }
        };

        let CompactOutcome::Compacted(stats) = outcome else {
            return None;
        };
        self.bytes_since_compact.store(0, Ordering::Relaxed);
        self.compact_backstop_bytes.store(
            next_backstop(
                self.compact_backstop_bytes(),
                stats.bytes_before,
                stats.bytes_after,
            ),
            Ordering::Relaxed,
        );
        // `if(e===this.sessionFile) await this.reAppendSessionMetadataAsync(!1,!0)`
        // — the rewrite just deleted every superseded metadata record, so the
        // survivors have to be re-stated at the tail with dedup SKIPPED (the
        // tail it would have deduped against no longer exists).
        let target = self.capture_active_target();
        if let Some(sid) = Self::metadata_session_id_from_path(&target.0) {
            let slot = self.session_metadata_state(&sid, &target.0).await;
            let mut state = slot.lock().await;
            if let Err(e) = self
                .re_append_session_metadata_to_target(&mut state, &sid, false, true, target)
                .await
            {
                tracing::error!("Metadata re-append after transcript compact failed: {e}");
            }
        }
        Some(stats)
    }

    /// Re-append the session's metadata sidecar records — 1:1 with
    /// `Isp.reAppendSessionMetadata` (2.1.220 @237852347) and its async twin
    /// `reAppendSessionMetadataAsync` (@237852577).
    ///
    /// Reads the last 64 KiB of the active transcript, runs
    /// [`plan_re_append`] over it (adopt-back → rebuild → dedup, mutating
    /// `state` in place), and appends the surviving records. Follows the ASYNC
    /// variant's write shape: all entries in ONE append (`jsonlJoin`), which is
    /// byte-identical to the sync variant's per-entry appends.
    ///
    /// Both flags are SKIP flags — see [`plan_re_append`]. The three production
    /// polarities at the oracle are:
    /// - resume adopt (`adoptResumedSessionFile`): `(true, false)`
    /// - periodic backstop / post-compaction: `(false, true)`
    /// - process exit (`reAppendSessionMetadataAtExit`): `(false, false)`
    ///
    /// The counter is zeroed FIRST, exactly as the oracle does, so a failure
    /// mid-way does not immediately re-arm the backstop.
    ///
    /// Returns the number of records written (0 when everything deduped away).
    pub async fn re_append_session_metadata(
        &self,
        state: &mut SessionMetadataState,
        session_id: &str,
        skip_title_adopt: bool,
        skip_dedup: bool,
    ) -> Result<usize, WriterError> {
        self.re_append_session_metadata_to_target(
            state,
            session_id,
            skip_title_adopt,
            skip_dedup,
            self.capture_active_target(),
        )
        .await
    }

    async fn re_append_session_metadata_to_target(
        &self,
        state: &mut SessionMetadataState,
        session_id: &str,
        skip_title_adopt: bool,
        skip_dedup: bool,
        target: (PathBuf, Option<Arc<DurableTranscriptWriter>>),
    ) -> Result<usize, WriterError> {
        let session_id = canonical_metadata_session_id(session_id);
        let session_id = session_id.as_str();
        let _g = self.lock.lock().await;
        let (path, durable) = self.admitted_metadata_target(session_id, target);
        if Self::metadata_session_id_from_path(&path).as_deref() != Some(session_id) {
            return Err(FsError::Io(
                "metadata session does not match its captured transcript".into(),
            )
            .into());
        }
        if self.active_path() == path {
            self.bytes_since_metadata_re_append
                .store(0, Ordering::Relaxed);
        }
        let tail = read_tail(&path);
        // Electron can append a title through its host process while this
        // writer remains live. If a single large append pushed that record
        // beyond the 64 KiB tail window, the normal tail adoption would miss
        // it and re-emit the writer's stale in-memory title. The full-file
        // fallback is only needed when the cheap tail scan has no title.
        if !skip_title_adopt && !tail.contains("\"type\":\"custom-title\"") {
            if let Some(title) = latest_custom_title(&path, session_id) {
                state.title = title;
            }
        }
        let Some(plan) = plan_re_append(&tail, state, session_id, skip_title_adopt, skip_dedup)
        else {
            return Ok(0);
        };
        if plan.is_empty() {
            return Ok(0);
        }
        let count = plan.entries.len();
        if let Some(durable) = durable {
            for line in plan.to_jsonl().lines() {
                let payload = Utf16JsonProjection::parse(line)
                    .map_err(crate::jsonl::exact_json::projection_error)?;
                self.append_json_durable_locked(path.clone(), durable.clone(), payload, None)
                    .await?;
            }
        } else {
            let _identity_guard = self.prepare_metadata_identity_non_durable(&path).await?;
            self.append_payload_to_path(&path, &plan.to_jsonl(), true)
                .await?;
        }
        // The re-appended bytes are the metadata itself; they must not count
        // toward the next backstop.
        if self.active_path() == path {
            self.bytes_since_metadata_re_append
                .store(0, Ordering::Relaxed);
        }
        Ok(count)
    }

    /// Append a user-set `custom-title` metadata line for `session_id` — the
    /// `/rename` write path, 1:1 with claude-code `saveCustomTitle`'s
    /// `appendEntryToFile(path, { type: 'custom-title', customTitle, sessionId })`.
    ///
    /// `session_id` MUST be the BARE session uuid (the `<uuid>.jsonl` file stem),
    /// NOT the `sess:`-prefixed `SessionId` display form — the loader keys the
    /// `custom_titles` map by file stem (`loader.rs`), so a prefixed id would
    /// never match on read. Same lock / dir-mode / file-mode contract as
    /// [`Self::append`].
    /// Append a `/rewind` `file-history-snapshot` side-map line (the checkpoint
    /// index for one turn) — same lock / dir-mode / file-mode contract as
    /// [`Self::append`].
    pub async fn append_file_history_snapshot(
        &self,
        value: &serde_json::Value,
    ) -> Result<(), WriterError> {
        let line = serde_json::to_string(value)?;
        let _g = self.lock.lock().await;
        let mut payload = String::with_capacity(line.len() + 1);
        payload.push_str(&line);
        payload.push('\n');
        self.append_payload(&payload).await
    }

    /// Append a host-consumed queue operation through the canonical writer
    /// lease, preserving its source content/identity projection.
    pub async fn append_queue_operation(
        &self,
        record: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) -> Result<(), WriterError> {
        let mut line = record.to_json_string().map_err(|error| {
            WriterError::ExactJson(ExactJsonError::InvalidOverride(error.to_string()))
        })?;
        line.push('\n');
        {
            let _guard = self.lock.lock().await;
            self.flush_queue_operations_locked().await?;
            self.append_payload(&line).await?;
        }
        self.maybe_re_append_metadata().await;
        Ok(())
    }

    pub async fn append_custom_title(
        &self,
        session_id: &str,
        custom_title: Utf16JsonProjection,
    ) -> Result<(), WriterError> {
        let session_id = canonical_metadata_session_id(session_id);
        let session_id = session_id.as_str();
        let value = Utf16JsonProjection::object(vec![
            ("type".into(), Utf16JsonProjection::from("custom-title")),
            ("customTitle".into(), custom_title.clone()),
            ("sessionId".into(), Utf16JsonProjection::from(session_id)),
        ])
        .expect("custom-title source record has unique fields");
        // Mirror the oracle's `currentSessionTitle`. NOTE this is belt-and-braces,
        // not the load-bearing path: `plan_re_append` ADOPTS the title back out
        // of the tail, and the backstop (32 KiB) is deliberately half the tail
        // window (64 KiB) so it fires while the record is still readable.
        // Removing this line does NOT fail `appends_alone_drive_the_metadata_backstop`
        // — verified by mutation. It matters only when state is set without a
        // corresponding record already on disk.
        let target = self.capture_metadata_target(session_id)?;
        let slot = self.session_metadata_state(session_id, &target.0).await;
        let mut state = slot.lock().await;
        state.title = Some(custom_title);
        self.append_source_side_record_to_target(&value, target)
            .await?;
        drop(state);
        self.maybe_re_append_metadata().await;
        Ok(())
    }

    /// Persist a mobile-created zero-message session before its first turn.
    ///
    /// The record deliberately remains a `custom-title` side record so the
    /// existing session catalog can list it with `message_count == 0`, while the
    /// versioned marker lets the mobile host distinguish a genuine empty session
    /// from an arbitrary metadata-only/corrupt transcript.
    pub async fn append_mobile_empty_session(
        &self,
        session_id: &str,
        title: &str,
        cwd: &str,
    ) -> Result<(), WriterError> {
        let value = serde_json::json!({
            "type": "custom-title",
            "customTitle": title,
            "sessionId": session_id,
            "mobileEmptySession": 1,
            "cwd": cwd,
        });
        self.append_side_record(&value).await
    }

    /// Persist the active permission mode for this transcript's session.
    ///
    /// The mode is session metadata, not a user-wide default: reopening this
    /// transcript restores its last selected mode without changing other
    /// sessions. The active writer path is the source of the bare session UUID.
    pub async fn append_permission_mode(&self, permission_mode: &str) -> Result<(), WriterError> {
        let target = self.capture_active_target();
        let Some(session_id) = Self::metadata_session_id_from_path(&target.0) else {
            return Ok(());
        };
        let slot = self.session_metadata_state(&session_id, &target.0).await;
        let mut state = slot.lock().await;
        state.permission_mode = Some(permission_mode.to_string());
        let value = serde_json::json!({
            "type": "permission-mode",
            "permissionMode": permission_mode,
            "sessionId": session_id,
        });
        let source = Utf16JsonProjection::from_serde_value(&value)
            .map_err(crate::jsonl::exact_json::projection_error)?;
        self.append_source_side_record_to_target(&source, target)
            .await?;
        drop(state);
        self.maybe_re_append_metadata().await;
        Ok(())
    }

    /// Persist the mobile chat/code capability profile for this transcript.
    pub async fn append_session_mode(&self, session_mode: &str) -> Result<(), WriterError> {
        let target = self.capture_active_target();
        let Some(session_id) = Self::metadata_session_id_from_path(&target.0) else {
            return Ok(());
        };
        let slot = self.session_metadata_state(&session_id, &target.0).await;
        let mut state = slot.lock().await;
        state.session_mode = Some(session_mode.to_string());
        let value = serde_json::json!({
            "type": "session-mode",
            "sessionMode": session_mode,
            "sessionId": session_id,
        });
        let source = Utf16JsonProjection::from_serde_value(&value)
            .map_err(crate::jsonl::exact_json::projection_error)?;
        self.append_source_side_record_to_target(&source, target)
            .await?;
        drop(state);
        self.maybe_re_append_metadata().await;
        Ok(())
    }

    /// Append an `agent-setting` metadata line for `session_id` — the persisted
    /// main-thread `--agent` selection (`agentSetting` = the agent's `agentType`)
    /// so a later `--resume` (with no `--agent`) can re-adopt it. 1:1 with
    /// claude-code's session persist `appendEntryToFile(path, {type:
    /// 'agent-setting', agentSetting: currentSessionAgentSetting, sessionId})`
    /// (`sessionStorage.ts`; read back by the `agentSettings.set(N.sessionId,
    /// N.agentSetting)` routing and fed to `rVe` on resume).
    ///
    /// `session_id` MUST be the BARE session uuid (the `<uuid>.jsonl` file stem),
    /// NOT the `sess:`-prefixed display form — the loader keys the
    /// `agent_settings` map by that stem, so a prefixed id would never match on
    /// read. Same lock / dir-mode / file-mode contract as [`Self::append`].
    pub async fn append_agent_setting(
        &self,
        session_id: &str,
        agent_setting: &str,
    ) -> Result<(), WriterError> {
        let value = serde_json::json!({
            "type": "agent-setting",
            "agentSetting": agent_setting,
            "sessionId": session_id,
        });
        self.append_side_record(&value).await
    }

    /// Persist the Claude-compatible agent name together with a versioned,
    /// immutable resolved definition. The sibling `agentSnapshot` field is an
    /// additive LingXi extension; old readers continue consuming
    /// `agentSetting`, while new readers can resume even if the catalog entry is
    /// later edited or removed.
    pub async fn append_agent_setting_snapshot(
        &self,
        session_id: &str,
        agent_setting: &str,
        definition: &serde_json::Value,
    ) -> Result<(), WriterError> {
        use sha2::{Digest, Sha256};

        let canonical = serde_json::to_vec(definition).map_err(WriterError::Serialize)?;
        let hash = format!("{:x}", Sha256::digest(&canonical));
        let value = serde_json::json!({
            "type": "agent-setting",
            "agentSetting": agent_setting,
            "agentSnapshot": {
                "schemaVersion": 1,
                "sha256": hash,
                "definition": definition,
            },
            "sessionId": session_id,
        });
        self.append_side_record(&value).await
    }

    /// Append a `worktree-state` metadata line for `session_id` — the persisted
    /// active-worktree record so a later `--continue`/`--resume` can rehydrate
    /// the session's `EnterWorktree` state (making `ExitWorktree` operate instead
    /// of no-oping). 1:1 with claude-code's `saveWorktreeState`
    /// (`gne` → `appendEntryToFile(path, {type:'worktree-state', worktreeSession,
    /// sessionId})`; read back by the `worktreeStates.set(N.sessionId,
    /// N.worktreeSession)` routing).
    ///
    /// `worktree_session` is the serialized session payload for an active
    /// worktree, or [`None`] for the `ExitWorktree` clear record (persisted as
    /// JSON `null`, matching claude's `gne(null)`). `session_id` MUST be the BARE
    /// session uuid (the `<uuid>.jsonl` file stem the loader keys the
    /// `worktree_states` map by). Same lock / dir-mode / file-mode contract as
    /// [`Self::append`].
    pub async fn append_worktree_state(
        &self,
        session_id: &str,
        worktree_session: Option<&serde_json::Value>,
    ) -> Result<(), WriterError> {
        let session_id = canonical_metadata_session_id(session_id);
        let session_id = session_id.as_str();
        let value = serde_json::json!({
            "type": "worktree-state",
            "worktreeSession": worktree_session.cloned().unwrap_or(serde_json::Value::Null),
            "sessionId": session_id,
        });
        let target = self.capture_metadata_target(session_id)?;
        let slot = self.session_metadata_state(session_id, &target.0).await;
        let mut state = slot.lock().await;
        let source = Utf16JsonProjection::from_serde_value(&value)
            .map_err(crate::jsonl::exact_json::projection_error)?;
        self.append_source_side_record_to_target(&source, target)
            .await?;
        state.worktree = Some(worktree_session.cloned().unwrap_or(serde_json::Value::Null));
        drop(state);
        self.maybe_re_append_metadata().await;
        Ok(())
    }

    /// Persist worktree state to the currently selected transcript with the
    /// identity and durable authority captured from that same target. Reusing
    /// the main writer makes this follow hot resume and transcript relocation.
    pub async fn append_worktree_state_for_current_session(
        &self,
        worktree_session: Option<&serde_json::Value>,
    ) -> Result<(), WriterError> {
        loop {
            let target = self.capture_active_target();
            let session_id = Self::metadata_session_id_from_path(&target.0)
                .as_deref()
                .filter(|id| uuid::Uuid::parse_str(id).is_ok())
                .ok_or_else(|| {
                    FsError::Io("worktree transcript target has no session UUID".into())
                })?
                .to_owned();
            let slot = self.session_metadata_state(&session_id, &target.0).await;
            let mut state = slot.lock().await;
            let guard = self.lock.lock().await;
            let admitted = self.capture_active_target();
            let same_authority = match (&target.1, &admitted.1) {
                (None, None) => true,
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                _ => false,
            };
            if target.0 != admitted.0 || !same_authority {
                drop(guard);
                drop(state);
                continue;
            }
            let value = serde_json::json!({
                "type": "worktree-state",
                "worktreeSession": worktree_session.cloned().unwrap_or(serde_json::Value::Null),
                "sessionId": session_id,
            });
            if let Some(durable) = admitted.1 {
                self.flush_queue_operations_locked().await?;
                let payload = Utf16JsonProjection::from_serde_value(&value)
                    .map_err(crate::jsonl::exact_json::projection_error)?;
                self.append_json_durable_locked(admitted.0, durable, payload, None)
                    .await?;
            } else {
                let mut line = serde_json::to_string(&value)?;
                line.push('\n');
                let _identity_guard = self
                    .prepare_metadata_identity_non_durable(&admitted.0)
                    .await?;
                self.append_payload_to_path(&admitted.0, &line, true)
                    .await?;
            }
            state.worktree = Some(worktree_session.cloned().unwrap_or(serde_json::Value::Null));
            drop(guard);
            drop(state);
            break;
        }
        self.maybe_re_append_metadata().await;
        Ok(())
    }

    /// Append one ordered context-collapse commit record.
    ///
    /// The field order is the upstream `{type, sessionId, ...commit}` spread
    /// order and is intentionally locked because transcript JSONL is a byte-level
    /// compatibility surface.
    #[allow(clippy::too_many_arguments)]
    pub async fn append_context_collapse_commit(
        &self,
        session_id: &str,
        collapse_id: &str,
        summary_uuid: &str,
        summary_content: &str,
        summary: &str,
        first_archived_uuid: &str,
        last_archived_uuid: &str,
    ) -> Result<(), WriterError> {
        let value = serde_json::json!({
            "type": "marble-origami-commit",
            "sessionId": session_id,
            "collapseId": collapse_id,
            "summaryUuid": summary_uuid,
            "summaryContent": summary_content,
            "summary": summary,
            "firstArchivedUuid": first_archived_uuid,
            "lastArchivedUuid": last_archived_uuid,
        });
        self.append_side_record(&value).await
    }

    /// Append the last-wins staged-queue/spawn-state snapshot.
    pub async fn append_context_collapse_snapshot(
        &self,
        session_id: &str,
        staged: &serde_json::Value,
        armed: bool,
        last_spawn_tokens: u64,
    ) -> Result<(), WriterError> {
        let value = serde_json::json!({
            "type": "marble-origami-snapshot",
            "sessionId": session_id,
            "staged": staged,
            "armed": armed,
            "lastSpawnTokens": last_spawn_tokens,
        });
        self.append_side_record(&value).await
    }

    /// Physically remove a rejected attempt by its message or row identities,
    /// as cc 2.1.263 does for each tombstoned assistant message. Preserve
    /// untouched rows byte for byte and repair children after removing the
    /// eagerly persisted streamed rows. Returns the surviving chain tail.
    pub async fn remove_retry_attempt(
        &self,
        message_id: &str,
    ) -> Result<Option<String>, WriterError> {
        self.remove_retry_attempt_rows(&[message_id.to_owned()])
            .await
    }

    /// Remove all transcript rows belonging to one rejected attempt.
    ///
    /// The identifiers may contain the logical assistant identity, streamed
    /// row UUIDs, or provider message IDs. Streaming rows have their own UUIDs,
    /// so callers pass every row UUID rather than assuming the logical turn ID
    /// is the persisted row ID.
    pub async fn remove_retry_attempt_rows(
        &self,
        message_ids: &[String],
    ) -> Result<Option<String>, WriterError> {
        self.reject_persistent_chat_mutation("retry transcript rewrite")?;
        let _guard = self.lock.lock().await;
        let path = self.active_path();
        let source = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| FsError::Io(e.to_string()))?;
        let message_ids = message_ids
            .iter()
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        let mut removed = std::collections::HashMap::new();
        for line in source.lines() {
            if let Ok(row) = Utf16JsonProjection::parse(line) {
                if row.string_at("/type") == Some("assistant")
                    && (row
                        .string_at("/uuid")
                        .is_some_and(|id| message_ids.contains(id))
                        || row
                            .string_at("/message/id")
                            .is_some_and(|id| message_ids.contains(id)))
                {
                    if let Some(id) = row.string_at("/uuid") {
                        removed.insert(
                            id.to_owned(),
                            row.string_at("/parentUuid").map(str::to_owned),
                        );
                    }
                }
            }
        }
        let mut output = String::with_capacity(source.len());
        let mut tail = None;
        for line in source.split_inclusive('\n') {
            let json_line = line.strip_suffix('\n').unwrap_or(line);
            let Ok(mut row) = Utf16JsonProjection::parse(json_line) else {
                output.push_str(line);
                continue;
            };
            if row
                .string_at("/uuid")
                .is_some_and(|id| removed.contains_key(id))
            {
                continue;
            }
            let mut parent = row.string_at("/parentUuid").map(str::to_owned);
            let original_parent = parent.clone();
            let mut visited = std::collections::HashSet::new();
            while let Some(id) = parent.as_ref() {
                if !visited.insert(id.clone()) {
                    break;
                }
                let Some(previous) = removed.get(id) else {
                    break;
                };
                parent = previous.clone();
            }
            if parent != original_parent {
                let parent_value = parent.map_or_else(
                    || Utf16JsonProjection::parse("null").expect("null is valid source JSON"),
                    Utf16JsonProjection::from,
                );
                row.set_field("parentUuid", parent_value)
                    .map_err(crate::jsonl::exact_json::projection_error)?;
                output.push_str(
                    &row.to_json_string()
                        .map_err(crate::jsonl::exact_json::projection_error)?,
                );
                if line.ends_with('\n') {
                    output.push('\n');
                }
            } else {
                output.push_str(line);
            }
            if matches!(
                row.string_at("/type"),
                Some("assistant" | "user" | "system" | "attachment")
            ) {
                if let Some(id) = row.string_at("/uuid") {
                    tail = Some(id.to_owned());
                }
            }
        }
        if !removed.is_empty() {
            let parent = path
                .parent()
                .ok_or_else(|| FsError::Io("transcript has no parent directory".into()))?;
            let identity = lingxi_core::host::rooted_fs::root_identity(parent)?;
            let _identity_guard = self.identity_sidecar_guard(&path).await?;
            message_identity::require_initialized_at(&path, parent, &identity)?;
            let filename = path
                .file_name()
                .ok_or_else(|| FsError::Io("transcript has no filename".into()))?;
            self.fs
                .write_file_rooted_atomic(parent, Path::new(filename), &output)
                .await?;
            for uuid in removed.keys() {
                message_identity::remove_row_identity_at(
                    &self.identity_store,
                    &path,
                    parent,
                    &identity,
                    uuid,
                )?;
            }
        }
        Ok(tail)
    }

    /// Physically remove one exact transcript row, as Native
    /// `TranscriptWriter.removeMessageByUuid` does for assistant tombstones.
    ///
    /// Unlike rejected-attempt removal, this deliberately leaves children with
    /// their original `parentUuid`; Native removes only the row itself. It
    /// first scans the final 64 KiB. Durable transcripts use the shared
    /// cross-process transaction and atomically replace the row; ordinary
    /// transcripts use an atomic rewrite after Native's bounded tail scan. A
    /// target outside the tail window can be found only in files up to 50 MiB.
    pub async fn remove_message_by_uuid(&self, message_uuid: &str) -> Result<bool, WriterError> {
        self.reject_persistent_chat_mutation("transcript tombstone")?;
        use std::io::{Read, Seek};
        use tokio::io::{AsyncReadExt, AsyncSeekExt};

        let _guard = self.lock.lock().await;
        if let Some(target) = self.active_durable_target() {
            let parent = target
                .path
                .parent()
                .ok_or_else(|| FsError::Io("transcript has no parent directory".into()))?;
            let filename = target
                .path
                .file_name()
                .ok_or_else(|| FsError::Io("transcript has no filename".into()))?;
            let transcript_root = parent.to_path_buf();
            let transcript_identity = lingxi_core::host::rooted_fs::root_identity(parent)?;
            let transcript_relative = PathBuf::from(filename);
            let message_uuid = message_uuid.to_owned();
            let identity_store = self.identity_store.clone();
            let transcript_path = target.path.clone();
            return tokio::task::spawn_blocking(move || {
                target.writer.with_transaction(|transaction| {
                    message_identity::require_initialized_at(
                        &transcript_path,
                        &transcript_root,
                        &transcript_identity,
                    )?;
                    let removed = transaction.remove_message_by_uuid_at(
                        &transcript_root,
                        &transcript_identity,
                        &transcript_relative,
                        &message_uuid,
                    )?;
                    if removed {
                        message_identity::remove_row_identity_at(
                            &identity_store,
                            &transcript_path,
                            &transcript_root,
                            &transcript_identity,
                            &message_uuid,
                        )?;
                    }
                    Ok(removed)
                })
            })
            .await
            .map_err(|error| FsError::Io(error.to_string()))?
            .map_err(WriterError::from);
        }

        let path = self.active_path();
        let parent = path
            .parent()
            .ok_or_else(|| FsError::Io("transcript has no parent directory".into()))?;
        let filename = path
            .file_name()
            .ok_or_else(|| FsError::Io("transcript has no filename".into()))?;
        let relative = PathBuf::from(filename);
        let identity = lingxi_core::host::rooted_fs::root_identity(parent)?;
        let _identity_guard = self.identity_sidecar_guard(&path).await?;
        message_identity::require_initialized_at(&path, parent, &identity)?;
        let source = match lingxi_core::host::rooted_fs::open_read_file_pinned(
            parent,
            &relative,
            Some(&identity),
        ) {
            Ok(file) => file,
            Err(FsError::NotFound(_)) => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        let mut file = tokio::fs::File::from_std(source);
        let file_len = file
            .metadata()
            .await
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
        file.seek(std::io::SeekFrom::Start(tail_start))
            .await
            .map_err(|error| FsError::Io(error.to_string()))?;
        file.read_exact(&mut tail)
            .await
            .map_err(|error| FsError::Io(error.to_string()))?;

        let mut offset = 0usize;
        let mut fast_line = None;
        for line in tail.split_inclusive(|byte| *byte == b'\n') {
            let line_start = offset;
            offset += line.len();
            // The first tail fragment may begin in the middle of a JSONL
            // record; only inspect it when the scan starts at byte zero.
            if line_start == 0 && tail_start != 0 {
                continue;
            }
            let json_line = line.strip_suffix(b"\n").unwrap_or(line);
            let Some(json_line) = std::str::from_utf8(json_line).ok() else {
                continue;
            };
            let matches_uuid = parse_exact_json(json_line)
                .ok()
                .and_then(|exact| {
                    exact
                        .value
                        .get("uuid")
                        .and_then(serde_json::Value::as_str)
                        .map(|uuid| uuid == message_uuid)
                })
                .unwrap_or(false);
            if matches_uuid {
                fast_line = Some(line_start..offset);
                break;
            }
        }
        if fast_line.is_none() && file_len > TOMBSTONE_REWRITE_LIMIT_BYTES {
            tracing::warn!(
                bytes = file_len,
                message_uuid,
                "skipping transcript tombstone removal because the target is outside the tail window of a large session file"
            );
            return Ok(false);
        }

        if let Some(line) = fast_line {
            let line_start = tail_start
                .checked_add(line.start as u64)
                .ok_or_else(|| FsError::Io("transcript tail offset overflow".into()))?;
            let line_end = tail_start
                .checked_add(line.end as u64)
                .ok_or_else(|| FsError::Io("transcript tail offset overflow".into()))?;
            let mut source = file.into_std().await;
            lingxi_core::host::rooted_fs::atomic_write_stream_pinned(
                parent,
                &relative,
                lingxi_core::host::rooted_fs::AtomicWriteOptions {
                    overwrite: true,
                    create_parents: false,
                    dir_mode: 0o700,
                    file_mode: 0o600,
                },
                &identity,
                |temporary| {
                    source
                        .seek(std::io::SeekFrom::Start(0))
                        .map_err(|error| FsError::Io(error.to_string()))?;
                    let copied_prefix = {
                        let mut prefix = (&mut source).take(line_start);
                        std::io::copy(&mut prefix, temporary)
                            .map_err(|error| FsError::Io(error.to_string()))?
                    };
                    if copied_prefix != line_start {
                        return Err(FsError::Io(
                            "transcript changed while staging tombstone prefix".into(),
                        ));
                    }
                    source
                        .seek(std::io::SeekFrom::Start(line_end))
                        .map_err(|error| FsError::Io(error.to_string()))?;
                    let copied_suffix = std::io::copy(&mut source, temporary)
                        .map_err(|error| FsError::Io(error.to_string()))?;
                    if copied_suffix != file_len.saturating_sub(line_end) {
                        return Err(FsError::Io(
                            "transcript changed while staging tombstone suffix".into(),
                        ));
                    }
                    Ok(())
                },
            )?;
            message_identity::remove_row_identity_at(
                &self.identity_store,
                &path,
                parent,
                &identity,
                message_uuid,
            )?;
            return Ok(true);
        }

        // Keep the original inode intact until a complete replacement is
        // ready, including the common tail hit. If the process exits during
        // the write, the previous transcript remains readable.
        let capacity = usize::try_from(file_len)
            .map_err(|_| FsError::Io("transcript file exceeds addressable memory".into()))?;
        let mut body = Vec::new();
        body.try_reserve_exact(capacity).map_err(|error| {
            FsError::Io(format!("could not buffer transcript tombstone: {error}"))
        })?;
        file.seek(std::io::SeekFrom::Start(0))
            .await
            .map_err(|error| FsError::Io(error.to_string()))?;
        let mut bounded_file = file.take(TOMBSTONE_REWRITE_LIMIT_BYTES + 1);
        bounded_file
            .read_to_end(&mut body)
            .await
            .map_err(|error| FsError::Io(error.to_string()))?;
        if body.len() as u64 > TOMBSTONE_REWRITE_LIMIT_BYTES {
            tracing::warn!(
                bytes = body.len(),
                message_uuid,
                "skipping transcript tombstone removal because the transcript grew past the rewrite bound"
            );
            return Ok(false);
        }
        let mut line_start = 0usize;
        let mut match_range = None;
        for line in body.split_inclusive(|byte| *byte == b'\n') {
            let line_end = line_start + line.len();
            let json_line = line.strip_suffix(b"\n").unwrap_or(line);
            let matches_uuid = std::str::from_utf8(json_line)
                .ok()
                .and_then(|line| parse_exact_json(line).ok())
                .and_then(|exact| {
                    exact
                        .value
                        .get("uuid")
                        .and_then(serde_json::Value::as_str)
                        .map(|uuid| uuid == message_uuid)
                })
                .unwrap_or(false);
            if matches_uuid {
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
        let parent = path
            .parent()
            .ok_or_else(|| FsError::Io("transcript has no parent directory".into()))?;
        path.file_name()
            .ok_or_else(|| FsError::Io("transcript has no filename".into()))?;
        lingxi_core::host::rooted_fs::atomic_write_pinned(
            parent,
            &relative,
            &body,
            lingxi_core::host::rooted_fs::AtomicWriteOptions {
                overwrite: true,
                create_parents: false,
                dir_mode: 0o700,
                file_mode: 0o600,
            },
            &identity,
        )?;
        message_identity::remove_row_identity_at(
            &self.identity_store,
            &path,
            parent,
            &identity,
            message_uuid,
        )?;
        Ok(true)
    }

    /// Append a context-collapse reset tombstone.
    pub async fn append_context_collapse_reset(
        &self,
        session_id: &str,
        reason: &str,
    ) -> Result<(), WriterError> {
        let value = serde_json::json!({
            "type": "marble-origami-reset",
            "sessionId": session_id,
            "reason": reason,
        });
        self.append_side_record(&value).await
    }

    /// Shared body for the metadata side-record appenders ([`Self::append_custom_title`],
    /// [`Self::append_agent_setting`]): serialize one JSON object + `\n` and append
    /// it under the same lock / dir-mode (0o700) / file-mode (0o600) contract as
    /// [`Self::append`].
    /// Test-only raw append, so re-append tests can plant arbitrary filler /
    /// hand-written lines without going through a typed appender.
    #[cfg(test)]
    async fn append_payload_for_test(&self, payload: &str) {
        let _g = self.lock.lock().await;
        self.append_payload(payload).await.expect("raw append");
    }

    /// Test-only wrapper over the REAL [`Self::append_side_record`] path, so a
    /// test can generate bulk transcript without bypassing the backstop poll
    /// the way [`Self::append_payload_for_test`] does.
    #[cfg(test)]
    async fn append_side_record_for_test(&self, value: &serde_json::Value) {
        self.append_side_record(value).await.expect("side record");
    }

    async fn append_side_record(&self, value: &serde_json::Value) -> Result<(), WriterError> {
        {
            let line = serde_json::to_string(value)?;
            let _g = self.lock.lock().await;
            let mut payload = String::with_capacity(line.len() + 1);
            payload.push_str(&line);
            payload.push('\n');
            self.append_payload(&payload).await?;
        }
        // Every write path can trip the backstop: the oracle polls once at the
        // end of its write-queue DRAIN (@237852006), which all writes funnel
        // through. LingXi writes immediately, so the analog is a poll at the end
        // of each public append path — outside the critical section, since
        // `maybe_re_append_metadata` re-takes the lock.
        self.maybe_re_append_metadata().await;
        Ok(())
    }

    async fn append_source_side_record_to_target(
        &self,
        value: &Utf16JsonProjection,
        target: (PathBuf, Option<Arc<DurableTranscriptWriter>>),
    ) -> Result<(), WriterError> {
        let line = value
            .to_json_string()
            .map_err(crate::jsonl::exact_json::projection_error)?;
        {
            let _g = self.lock.lock().await;
            let target = value.string_at("/sessionId").map_or_else(
                || target.clone(),
                |session_id| self.admitted_metadata_target(session_id, target.clone()),
            );
            if let Some(durable) = target.1 {
                self.append_json_durable_locked(target.0, durable, value.clone(), None)
                    .await?;
            } else {
                let mut payload = String::with_capacity(line.len() + 1);
                payload.push_str(&line);
                payload.push('\n');
                let _identity_guard = self
                    .prepare_metadata_identity_non_durable(&target.0)
                    .await?;
                self.append_payload_to_path(&target.0, &payload, true)
                    .await?;
            }
        }
        Ok(())
    }

}

#[cfg(test)]
mod tests {
    fn read_message(value: serde_json::Value) -> JsonlMessage {
        let source = serde_json::to_string(&value).unwrap();
        crate::jsonl::reader::route_lines(&source)
            .messages_in_order
            .into_iter()
            .next()
            .expect("message row")
    }

    use super::*;
    use serde_json::json;

    fn temp_writer(tag: &str) -> (PathBuf, PathBuf, JsonlWriter) {
        let dir = std::env::temp_dir().join(format!(
            "lingxi-writer-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("11111111-2222-3333-4444-555555555555.jsonl");
        let fs: Arc<dyn FileSystem> =
            Arc::new(platform_posix::fs::PosixFileSystem::new(dir.clone()));
        (dir, path.clone(), JsonlWriter::new(path, fs))
    }

    fn identity_row(uuid: &str, content: &str) -> JsonlMessage {
        serde_json::from_value(json!({
            "parentUuid": null,
            "isSidechain": false,
            "type": "assistant",
            "message": {"id": format!("provider-{uuid}"), "role": "assistant", "content": content},
            "uuid": uuid,
            "timestamp": "2026-10-04T12:00:00.000Z",
            "cwd": "/workspace",
            "sessionId": "11111111-2222-3333-4444-555555555555",
            "version": "test"
        }))
        .expect("identity test row")
    }

    fn pending_user_row(
        uuid: &str,
        parent_uuid: Option<&str>,
        timestamp: &str,
        text: &str,
        turn_companion: Option<bool>,
    ) -> JsonlMessage {
        let mut value = json!({
            "parentUuid": parent_uuid,
            "isSidechain": false,
            "type": "user",
            "message": {"role":"user","content":[{"type":"text","text":text}]},
            "uuid": uuid,
            "timestamp": timestamp,
            "cwd": "/workspace",
            "sessionId": "11111111-2222-3333-4444-555555555555",
            "version": "test",
            "promptId": "stable-prompt",
            "permissionMode": "default",
        });
        if let Some(turn_companion) = turn_companion {
            value["turnCompanion"] = json!(turn_companion);
        }
        read_message(value)
    }

    fn source_value(value: &serde_json::Value) -> Utf16JsonProjection {
        Utf16JsonProjection::from_serde_value(value)
            .expect("generated fixture JSON is representable as a source projection")
    }

    #[test]
    fn queue_drain_restores_unfinished_records_before_concurrent_arrivals() {
        let source: QueueOperationBuffer = Default::default();
        let mut drain = QueueOperationDrain {
            source: source.clone(),
            records: VecDeque::from([source_value(&json!("second"))]),
            current: Some(source_value(&json!("first"))),
        };
        source
            .lock()
            .unwrap()
            .push_back(source_value(&json!("later")));
        // The same guard restores pending records on cancellation and I/O failure.
        drain.records.push_back(source_value(&json!("third")));
        drop(drain);
        let values = source
            .lock()
            .unwrap()
            .iter()
            .map(|projection| {
                serde_json::from_str::<serde_json::Value>(&projection.to_json_string().unwrap())
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            values,
            [
                json!("first"),
                json!("second"),
                json!("third"),
                json!("later")
            ]
        );
    }

    #[tokio::test]
    async fn queue_operations_flush_before_native_message_with_exact_utf16() {
        use lingxi_core::types::utf16_json::Utf16JsonProjection as Projection;
        let (dir, path, writer) = temp_writer("queue-before-message");
        let session = SessionId::parse_prefixed("11111111-2222-3333-4444-555555555555").unwrap();
        let queued = Projection::parse(r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-10-04T12:00:00.000Z","sessionId":"11111111-2222-3333-4444-555555555555","content":"\ud800"}"#).unwrap();
        let dequeued = source_value(&json!({
            "type":"queue-operation",
            "operation":"dequeue",
            "timestamp":"2026-10-04T12:00:00.001Z",
            "sessionId":session.as_uuid().to_string()
        }));
        let pending: QueueOperationBuffer = Arc::new(std::sync::Mutex::new(VecDeque::from([
            queued.clone(),
            dequeued.clone(),
        ])));
        writer.bind_queue_operations(session, pending.clone());
        writer
            .append(&identity_row(
                "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
                "done",
            ))
            .await
            .unwrap();
        let raw = std::fs::read_to_string(path).unwrap();
        let lines = raw.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], queued.to_json_string().unwrap());
        assert_eq!(lines[1], dequeued.to_json_string().unwrap());
        assert!(pending.lock().unwrap().is_empty());
        writer.flush_queue_operations().await.unwrap();
        assert_eq!(std::fs::read_to_string(writer.path()).unwrap(), raw);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn native_once_round_trips_tool_key_units_and_rejects_equal_display_conflict() {
        use lingxi_core::types::utf16_json::Utf16JsonProjection;
        let (dir, path, mut writer) = temp_writer("native-once-exact-tool-keys");
        let state_root = dir.join("state");
        std::fs::create_dir_all(&state_root).unwrap();
        writer = writer.with_durable_lock(Arc::new(
            DurableTranscriptWriter::open(&state_root).unwrap(),
        ));
        let session_id = SessionId::parse_prefixed("11111111-2222-3333-4444-555555555555").unwrap();
        writer
            .activate_session_target(session_id, path.clone(), dir.clone())
            .unwrap();
        let input = Utf16JsonProjection::parse(r#"{"\ud800":"\udfff"}"#).unwrap();
        let mut row = identity_row("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee", "");
        row.message = Utf16JsonProjection::parse(&format!(
            r#"{{"role":"assistant","content":[{{"type":"tool_use","id":"toolu_exact","name":"Echo","input":{}}}]}}"#,
            input.to_json_string().unwrap()
        ))
        .unwrap();
        writer.append_native_message_once(&row).await.unwrap();
        writer.append_native_message_once(&row).await.unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw.lines().count(), 1);
        assert!(!raw.contains("deliveryId"));
        assert!(!raw.contains("lingxi_exact_json_utf16"));
        let loaded = crate::jsonl::reader::route_lines(&raw);
        assert_eq!(loaded.malformed_line_count, 0);
        let recovered = loaded.messages_in_order[0]
            .message
            .subprojection("/content/0/input")
            .unwrap();
        assert_eq!(
            recovered.to_json_string().unwrap(),
            input.to_json_string().unwrap()
        );
        let other = Utf16JsonProjection::parse(r#"{"\ud801":"\udfff"}"#).unwrap();
        let input_key = input.object_entries().unwrap().remove(0);
        let other_key = other.object_entries().unwrap().remove(0);
        assert_eq!(input_key.display_key, other_key.display_key);
        assert_ne!(input_key.code_units, other_key.code_units);
        let mut conflicting = row.clone();
        conflicting.message = Utf16JsonProjection::parse(&format!(
            r#"{{"role":"assistant","content":[{{"type":"tool_use","id":"toolu_exact","name":"Echo","input":{}}}]}}"#,
            other.to_json_string().unwrap()
        ))
        .unwrap();
        assert!(writer
            .append_native_message_once(&conflicting)
            .await
            .is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn pending_prompt_once_ignores_only_timestamp_and_derived_parent_and_returns_actual_tip()
    {
        let (dir, path, mut writer) = temp_writer("pending-prompt-once");
        let state_root = dir.join("state");
        std::fs::create_dir_all(&state_root).unwrap();
        writer = writer.with_durable_lock(Arc::new(
            DurableTranscriptWriter::open(&state_root).unwrap(),
        ));
        let session_id = SessionId::parse_prefixed("11111111-2222-3333-4444-555555555555").unwrap();
        writer
            .activate_session_target(session_id, path.clone(), dir.clone())
            .unwrap();

        let first = pending_user_row(
            "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
            None,
            "2026-10-12T12:00:00.000Z",
            "same source",
            None,
        );
        let prior = identity_row("99999999-8888-4777-8666-555555555555", "prior row");
        let tail = identity_row("bbbbbbbb-cccc-4ddd-8eee-ffffffffffff", "later row");
        writer.append(&prior).await.unwrap();
        assert_eq!(
            writer
                .append_pending_prompt_message_once(&first)
                .await
                .unwrap(),
            (TranscriptAppendOutcome::Appended, Some(first.uuid.clone()))
        );
        writer.append(&tail).await.unwrap();

        let retry = pending_user_row(
            &first.uuid,
            None,
            "2026-10-12T12:05:00.000Z",
            "same source",
            None,
        );
        assert_eq!(
            writer
                .append_pending_prompt_message_once(&retry)
                .await
                .unwrap(),
            (
                TranscriptAppendOutcome::AlreadyPresent,
                Some(tail.uuid.clone())
            ),
            "a duplicate earlier in the transcript returns the actual current tail"
        );

        let changed_content = pending_user_row(
            &first.uuid,
            None,
            "2026-10-12T12:05:00.000Z",
            "different source",
            None,
        );
        assert!(matches!(
            writer
                .append_pending_prompt_message_once(&changed_content)
                .await,
            Err(TranscriptWriterError::DeliveryConflict { .. })
        ));

        let exact_units_id = "eeeeeeee-ffff-4000-8000-222222222222";
        let mut exact_units =
            pending_user_row(exact_units_id, None, "2026-10-12T12:00:00.000Z", "", None);
        exact_units.message = Utf16JsonProjection::parse(
            r#"{"role":"user","content":[{"type":"text","text":"\ud800"}]}"#,
        )
        .unwrap();
        writer
            .append_pending_prompt_message_once(&exact_units)
            .await
            .unwrap();
        let mut replacement_character = exact_units.clone();
        replacement_character.message =
            Utf16JsonProjection::parse(r#"{"role":"user","content":[{"type":"text","text":"�"}]}"#)
                .unwrap();
        assert!(matches!(
            writer
                .append_pending_prompt_message_once(&replacement_character)
                .await,
            Err(TranscriptWriterError::DeliveryConflict { .. })
        ));

        let companion_id = "cccccccc-dddd-4eee-8fff-000000000000";
        let companion = pending_user_row(
            companion_id,
            None,
            "2026-10-12T12:00:00.000Z",
            "body source",
            Some(true),
        );
        writer
            .append_pending_prompt_message_once(&companion)
            .await
            .unwrap();
        let changed_companion = pending_user_row(
            companion_id,
            None,
            "2026-10-12T12:05:00.000Z",
            "body source",
            Some(false),
        );
        assert!(matches!(
            writer
                .append_pending_prompt_message_once(&changed_companion)
                .await,
            Err(TranscriptWriterError::DeliveryConflict { .. })
        ));

        let hook_id = "dddddddd-eeee-4fff-8000-111111111111";
        let mut hook = read_message(json!({
            "parentUuid":null,
            "isSidechain":false,
            "attachment":{"type":"hook_success","hookEvent":"UserPromptExpansion","hookName":"before","content":"source"},
            "type":"attachment",
            "uuid":hook_id,
            "timestamp":"2026-10-12T12:00:00.000Z",
            "cwd":"/workspace",
            "sessionId":"11111111-2222-3333-4444-555555555555",
            "version":"test"
        }));
        writer
            .append_pending_prompt_message_once(&hook)
            .await
            .unwrap();
        hook.timestamp = "2026-10-12T12:05:00.000Z".into();
        assert_eq!(
            writer
                .append_pending_prompt_message_once(&hook)
                .await
                .unwrap()
                .0,
            TranscriptAppendOutcome::AlreadyPresent
        );
        hook.attachment_projection
            .as_mut()
            .unwrap()
            .set_field("hookName", Utf16JsonProjection::from("changed"))
            .unwrap();
        assert!(matches!(
            writer.append_pending_prompt_message_once(&hook).await,
            Err(TranscriptWriterError::DeliveryConflict { .. })
        ));

        let lines = std::fs::read_to_string(&path).unwrap();
        let count_uuid = |uuid: &str| {
            lines
                .lines()
                .filter_map(|line| Utf16JsonProjection::parse(line).ok())
                .filter(|row| row.string_at("/uuid") == Some(uuid))
                .count()
        };
        assert_eq!(count_uuid(&first.uuid), 1);
        assert_eq!(count_uuid(companion_id), 1);
        assert_eq!(count_uuid(hook_id), 1);
        drop(writer);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn pending_prompt_duplicate_uses_native_json_stringify_semantics_and_source_order() {
        let (dir, path, mut writer) = temp_writer("pending-prompt-native-json");
        let state_root = dir.join("state");
        std::fs::create_dir_all(&state_root).unwrap();
        writer = writer.with_durable_lock(Arc::new(
            DurableTranscriptWriter::open(&state_root).unwrap(),
        ));
        let session_id = SessionId::parse_prefixed("11111111-2222-3333-4444-555555555555").unwrap();
        writer
            .activate_session_target(session_id, path.clone(), dir.clone())
            .unwrap();

        let mut negative_zero = pending_user_row(
            "11111111-aaaa-4bbb-8ccc-111111111111",
            None,
            "2026-10-12T12:00:00.000Z",
            "number source",
            None,
        );
        negative_zero.extra = Utf16JsonProjection::parse(r#"{"nativeNumber":-0}"#).unwrap();
        assert_eq!(
            writer
                .append_pending_prompt_message_once(&negative_zero)
                .await
                .unwrap()
                .0,
            TranscriptAppendOutcome::Appended
        );
        let negative_zero_line = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .find(|line| line.contains(&negative_zero.uuid))
            .unwrap()
            .to_owned();
        assert!(negative_zero_line.contains(r#""nativeNumber":0"#));
        assert!(!negative_zero_line.contains(r#""nativeNumber":-0"#));

        let mut positive_zero = negative_zero.clone();
        positive_zero.timestamp = "2026-10-12T12:05:00.000Z".into();
        positive_zero.extra = Utf16JsonProjection::parse(r#"{"nativeNumber":0}"#).unwrap();
        assert_eq!(
            writer
                .append_pending_prompt_message_once(&positive_zero)
                .await
                .unwrap()
                .0,
            TranscriptAppendOutcome::AlreadyPresent,
            "JSON.stringify makes the persisted -0 source equal to a retry with 0"
        );

        let mut overflow = pending_user_row(
            "22222222-bbbb-4ccc-8ddd-222222222222",
            None,
            "2026-10-12T12:00:00.000Z",
            "overflow source",
            None,
        );
        overflow.extra = Utf16JsonProjection::parse(r#"{"nativeNumber":1e309}"#).unwrap();
        assert_eq!(
            writer
                .append_pending_prompt_message_once(&overflow)
                .await
                .unwrap()
                .0,
            TranscriptAppendOutcome::Appended
        );
        let overflow_line = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .find(|line| line.contains(&overflow.uuid))
            .unwrap()
            .to_owned();
        assert!(overflow_line.contains(r#""nativeNumber":null"#));

        let mut null_retry = overflow.clone();
        null_retry.timestamp = "2026-10-12T12:05:00.000Z".into();
        null_retry.extra = Utf16JsonProjection::parse(r#"{"nativeNumber":null}"#).unwrap();
        assert_eq!(
            writer
                .append_pending_prompt_message_once(&null_retry)
                .await
                .unwrap()
                .0,
            TranscriptAppendOutcome::AlreadyPresent,
            "JSON.stringify persists an overflowing number as null"
        );

        let mut ordered = pending_user_row(
            "33333333-cccc-4ddd-8eee-333333333333",
            None,
            "2026-10-12T12:00:00.000Z",
            "ordered source",
            None,
        );
        ordered.extra =
            Utf16JsonProjection::parse(r#"{"nativeFirst":"a","nativeSecond":"b"}"#).unwrap();
        assert_eq!(
            writer
                .append_pending_prompt_message_once(&ordered)
                .await
                .unwrap()
                .0,
            TranscriptAppendOutcome::Appended
        );
        let mut reordered = ordered.clone();
        reordered.timestamp = "2026-10-12T12:05:00.000Z".into();
        reordered.extra =
            Utf16JsonProjection::parse(r#"{"nativeSecond":"b","nativeFirst":"a"}"#).unwrap();
        assert!(matches!(
            writer.append_pending_prompt_message_once(&reordered).await,
            Err(TranscriptWriterError::DeliveryConflict { .. })
        ));

        let mut lone_surrogate_key = pending_user_row(
            "44444444-dddd-4eee-8fff-444444444444",
            None,
            "2026-10-12T12:00:00.000Z",
            "UTF-16 key source",
            None,
        );
        lone_surrogate_key.extra = Utf16JsonProjection::parse(r#"{"\ud800":"key value"}"#).unwrap();
        assert_eq!(
            writer
                .append_pending_prompt_message_once(&lone_surrogate_key)
                .await
                .unwrap()
                .0,
            TranscriptAppendOutcome::Appended
        );
        let mut replacement_key = lone_surrogate_key.clone();
        replacement_key.timestamp = "2026-10-12T12:05:00.000Z".into();
        replacement_key.extra = Utf16JsonProjection::parse(r#"{"�":"key value"}"#).unwrap();
        assert!(matches!(
            writer
                .append_pending_prompt_message_once(&replacement_key)
                .await,
            Err(TranscriptWriterError::DeliveryConflict { .. })
        ));

        let lines = std::fs::read_to_string(&path).unwrap();
        for uuid in [
            negative_zero.uuid.as_str(),
            overflow.uuid.as_str(),
            ordered.uuid.as_str(),
            lone_surrogate_key.uuid.as_str(),
        ] {
            assert_eq!(
                lines
                    .lines()
                    .filter_map(|line| Utf16JsonProjection::parse(line).ok())
                    .filter(|row| row.string_at("/uuid") == Some(uuid))
                    .count(),
                1,
                "one durable source row remains for {uuid}"
            );
        }
        drop(writer);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn pending_prompt_cold_retry_repairs_every_durable_row_prefix_by_uuid() {
        for persisted_prefix_len in 0..=4 {
            let tag = format!("pending-prefix-{persisted_prefix_len}");
            let (dir, path, first_writer) = temp_writer(&tag);
            let state_root = dir.join("state");
            std::fs::create_dir_all(&state_root).unwrap();
            let session_id =
                SessionId::parse_prefixed("11111111-2222-3333-4444-555555555555").unwrap();
            let first_writer = first_writer.with_durable_lock(Arc::new(
                DurableTranscriptWriter::open(&state_root).unwrap(),
            ));
            first_writer
                .activate_session_target(session_id, path.clone(), dir.clone())
                .unwrap();

            let command = pending_user_row(
                "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
                None,
                "2026-10-12T12:00:00.000Z",
                "/skill same source",
                None,
            );
            let body = pending_user_row(
                "bbbbbbbb-cccc-4ddd-8eee-ffffffffffff",
                None,
                "2026-10-12T12:00:00.000Z",
                "expanded body",
                Some(true),
            );
            let hook_one = read_message(json!({
                "parentUuid":null,"isSidechain":false,
                "attachment":{"type":"hook_success","hookEvent":"UserPromptExpansion","sequence":1},
                "type":"attachment","uuid":"cccccccc-dddd-4eee-8fff-000000000000",
                "timestamp":"2026-10-12T12:00:00.000Z","cwd":"/workspace",
                "sessionId":"11111111-2222-3333-4444-555555555555","version":"test"
            }));
            let hook_two = read_message(json!({
                "parentUuid":null,"isSidechain":false,
                "attachment":{"type":"hook_additional_context","hookEvent":"UserPromptExpansion","sequence":2},
                "type":"attachment","uuid":"dddddddd-eeee-4fff-8000-111111111111",
                "timestamp":"2026-10-12T12:00:00.000Z","cwd":"/workspace",
                "sessionId":"11111111-2222-3333-4444-555555555555","version":"test"
            }));
            let rows = [command, body, hook_one, hook_two];
            for row in rows.iter().take(persisted_prefix_len) {
                first_writer
                    .append_pending_prompt_message_once(row)
                    .await
                    .unwrap();
            }
            drop(first_writer);

            let fs: Arc<dyn FileSystem> =
                Arc::new(platform_posix::fs::PosixFileSystem::new(dir.clone()));
            let restarted = JsonlWriter::new(path.clone(), fs).with_durable_lock(Arc::new(
                DurableTranscriptWriter::open(&state_root).unwrap(),
            ));
            restarted
                .activate_session_target(session_id, path.clone(), dir.clone())
                .unwrap();
            let mut actual_tip = None;
            for row in &rows {
                let (outcome, tip) = restarted
                    .append_pending_prompt_message_once(row)
                    .await
                    .unwrap();
                actual_tip = tip;
                assert_eq!(
                    outcome,
                    if rows
                        .iter()
                        .position(|candidate| candidate.uuid == row.uuid)
                        .unwrap()
                        < persisted_prefix_len
                    {
                        TranscriptAppendOutcome::AlreadyPresent
                    } else {
                        TranscriptAppendOutcome::Appended
                    },
                    "prefix length {persisted_prefix_len} resumes the expected suffix"
                );
            }
            assert_eq!(
                actual_tip.as_deref(),
                rows.last().map(|row| row.uuid.as_str())
            );
            let raw = std::fs::read_to_string(&path).unwrap();
            let actual_uuids = raw
                .lines()
                .filter_map(|line| Utf16JsonProjection::parse(line).ok())
                .filter_map(|row| row.string_at("/uuid").map(str::to_owned))
                .collect::<Vec<_>>();
            assert_eq!(
                actual_uuids,
                rows.iter().map(|row| row.uuid.clone()).collect::<Vec<_>>(),
                "cold retry preserves source order and never duplicates a UUID"
            );
            drop(restarted);
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[tokio::test]
    async fn identity_ledger_preserves_outer_uuid_indices_and_deleted_high_water_after_restart() {
        let (dir, path, writer) = temp_writer("identity-ledger-restart");
        for uuid in [
            "11111111-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            "22222222-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
            "33333333-cccc-4ccc-8ccc-cccccccccccc",
        ] {
            writer.append(&identity_row(uuid, "block")).await.unwrap();
        }
        let first = writer
            .read_session_message_identity_snapshot(&path)
            .await
            .unwrap();
        assert_eq!(first.by_uuid["11111111-aaaa-4aaa-8aaa-aaaaaaaaaaaa"], 0);
        assert_eq!(first.by_uuid["22222222-bbbb-4bbb-8bbb-bbbbbbbbbbbb"], 1);
        assert_eq!(first.by_uuid["33333333-cccc-4ccc-8ccc-cccccccccccc"], 2);
        assert!(!first
            .by_uuid
            .contains_key("provider-11111111-aaaa-4aaa-8aaa-aaaaaaaaaaaa"));
        assert_eq!(first.next_message_index, 3);

        assert!(writer
            .remove_message_by_uuid("33333333-cccc-4ccc-8ccc-cccccccccccc")
            .await
            .unwrap());
        drop(writer);

        let fs: Arc<dyn FileSystem> =
            Arc::new(platform_posix::fs::PosixFileSystem::new(dir.clone()));
        let restarted = JsonlWriter::new(path.clone(), fs);
        let after_delete = restarted
            .read_session_message_identity_snapshot(&path)
            .await
            .unwrap();
        assert!(!after_delete
            .by_uuid
            .contains_key("33333333-cccc-4ccc-8ccc-cccccccccccc"));
        assert_eq!(after_delete.next_message_index, 3);

        restarted
            .append(&identity_row(
                "33333333-cccc-4ccc-8ccc-cccccccccccc",
                "same UUID after tombstone",
            ))
            .await
            .unwrap();
        let after_append = restarted
            .read_session_message_identity_snapshot(&path)
            .await
            .unwrap();
        assert_eq!(
            after_append.by_uuid["33333333-cccc-4ccc-8ccc-cccccccccccc"],
            3
        );
        assert_eq!(after_append.next_message_index, 4);
        restarted
            .append(&identity_row(
                "44444444-dddd-4ddd-8ddd-dddddddddddd",
                "next block",
            ))
            .await
            .unwrap();
        let after_new_uuid = restarted
            .read_session_message_identity_snapshot(&path)
            .await
            .unwrap();
        assert_eq!(
            after_new_uuid.by_uuid["44444444-dddd-4ddd-8ddd-dddddddddddd"],
            4
        );
        assert_eq!(after_new_uuid.next_message_index, 5);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn identity_bootstrap_imports_existing_outer_uuids_without_rewriting_native_jsonl() {
        let (dir, path, writer) = temp_writer("identity-native-bootstrap");
        let native = concat!(
            "{\"type\":\"user\",\"uuid\":\"native-user\",\"parentUuid\":null,\"sessionId\":\"11111111-2222-3333-4444-555555555555\",\"timestamp\":\"2026-10-04T12:00:00.000Z\",\"cwd\":\"/workspace\",\"version\":\"test\",\"isSidechain\":false,\"userType\":\"external\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
            "{\"type\":\"assistant\",\"uuid\":\"native-assistant\",\"parentUuid\":\"native-user\",\"sessionId\":\"11111111-2222-3333-4444-555555555555\",\"timestamp\":\"2026-10-04T12:00:01.000Z\",\"cwd\":\"/workspace\",\"version\":\"test\",\"isSidechain\":false,\"userType\":\"external\",\"message\":{\"role\":\"assistant\",\"content\":\"hello\"}}\n",
            "{\"type\":\"user\",\"uuid\":\"native-user\",\"parentUuid\":null,\"sessionId\":\"11111111-2222-3333-4444-555555555555\",\"timestamp\":\"2026-10-04T12:00:02.000Z\",\"cwd\":\"/workspace\",\"version\":\"test\",\"isSidechain\":false,\"userType\":\"external\",\"message\":{\"role\":\"user\",\"content\":\"updated\"}}\n"
        );
        std::fs::write(&path, native).unwrap();
        assert_eq!(
            writer
                .read_session_message_identity_snapshot(&path)
                .await
                .unwrap(),
            SessionMessageIdentitySnapshot::default(),
            "a valid Native transcript remains readable before explicit Host import"
        );
        assert!(writer
            .append(&identity_row(
                "native-followup",
                "must wait for explicit import"
            ))
            .await
            .is_err());
        let imported = writer
            .bootstrap_session_message_identity_snapshot(&path)
            .await
            .unwrap();
        assert_eq!(imported.by_uuid["native-user"], 0);
        assert_eq!(imported.by_uuid["native-assistant"], 1);
        assert_eq!(imported.next_message_index, 2);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), native);
        let loaded = crate::jsonl::reader::route_lines(native);
        assert_eq!(
            loaded.by_uuid["native-user"].message.string_at("/content"),
            Some("updated"),
            "Native JSONL keeps last-write-wins content while Host identity keeps first index"
        );
        writer
            .append(&identity_row("native-followup", "after import"))
            .await
            .unwrap();
        let after_append = writer
            .read_session_message_identity_snapshot(&path)
            .await
            .unwrap();
        assert_eq!(after_append.by_uuid["native-followup"], 2);
        assert_eq!(after_append.next_message_index, 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn identity_reader_does_not_infer_from_process_cache_when_sidecar_is_absent() {
        let (dir, path, writer) = temp_writer("identity-no-memory-facts");
        writer
            .append(&identity_row("55555555-eeee-4eee-8eee-eeeeeeeeeeee", "row"))
            .await
            .unwrap();
        let sidecar = message_identity::log_path(&path).unwrap();
        std::fs::remove_file(path.parent().unwrap().join(sidecar)).unwrap();
        assert_eq!(
            writer
                .read_session_message_identity_snapshot(&path)
                .await
                .unwrap(),
            SessionMessageIdentitySnapshot::default()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn identity_sidecar_reader_rejects_a_final_component_symlink() {
        use std::os::unix::fs::symlink;

        let (dir, path, writer) = temp_writer("identity-sidecar-symlink");
        let target = dir.join("victim");
        std::fs::write(&target, "{\"op\":\"bootstrap\",\"next_message_index\":0}\n").unwrap();
        let sidecar = dir.join(message_identity::log_path(&path).unwrap());
        symlink(&target, &sidecar).unwrap();
        assert!(writer
            .read_session_message_identity_snapshot(&path)
            .await
            .is_err());
        assert_eq!(
            std::fs::read_to_string(target).unwrap(),
            "{\"op\":\"bootstrap\",\"next_message_index\":0}\n"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn identity_sidecar_moves_with_transcript_relocation_and_keeps_allocator() {
        let (dir, old_path, writer) = temp_writer("identity-relocation");
        let new_parent = dir.join("relocated-project");
        std::fs::create_dir_all(&new_parent).unwrap();
        let new_path = new_parent.join(old_path.file_name().unwrap());
        writer
            .append(&identity_row(
                "66666666-ffff-4fff-8fff-ffffffffffff",
                "before relocation",
            ))
            .await
            .unwrap();

        writer
            .retarget_with_relocation(
                new_path.clone(),
                "11111111-2222-3333-4444-555555555555",
                "/workspace/relocated",
            )
            .await
            .unwrap();
        let moved = writer
            .read_session_message_identity_snapshot(&new_path)
            .await
            .unwrap();
        assert_eq!(moved.by_uuid["66666666-ffff-4fff-8fff-ffffffffffff"], 0);
        assert_eq!(moved.next_message_index, 1);

        writer
            .append(&identity_row(
                "77777777-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "after relocation",
            ))
            .await
            .unwrap();
        let after = writer
            .read_session_message_identity_snapshot(&new_path)
            .await
            .unwrap();
        assert_eq!(after.by_uuid["77777777-aaaa-4aaa-8aaa-aaaaaaaaaaaa"], 1);
        assert_eq!(after.next_message_index, 2);
        assert!(!old_path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn ordinary_append_preserves_recovered_exact_strings_in_both_writer_modes() {
        let fixture = include_str!("../../tests/fixtures/handback_exact_utf16_2_1_286.jsonl");
        let loaded = crate::jsonl::reader::route_lines(fixture);
        for durable in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let session_id =
                SessionId::parse_prefixed("11111111-2222-4333-8444-555555555555").unwrap();
            let path = dir.path().join(format!("{}.jsonl", session_id.as_uuid()));
            let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(
                dir.path().to_path_buf(),
            ));
            let mut writer = JsonlWriter::new(path.clone(), fs);
            if durable {
                let state_root = dir.path().join("state");
                std::fs::create_dir_all(&state_root).unwrap();
                writer = writer.with_durable_lock(Arc::new(
                    DurableTranscriptWriter::open(&state_root).unwrap(),
                ));
                writer
                    .activate_session_target(session_id, path.clone(), dir.path().to_path_buf())
                    .unwrap();
            }
            for message in &loaded.messages_in_order {
                writer.append(message).await.unwrap();
            }
            assert_eq!(std::fs::read_to_string(&path).unwrap(), fixture);
            assert_eq!(writer.bytes_since_metadata_re_append(), fixture.len());
            let second_path = dir.path().join("second.jsonl");
            // The non-durable explicit path has the same exact serializer.
            if !durable {
                for message in &loaded.messages_in_order {
                    writer.append_to_path(&second_path, message).await.unwrap();
                }
                assert_eq!(std::fs::read_to_string(second_path).unwrap(), fixture);
            } else {
                let other_session =
                    SessionId::parse_prefixed("22222222-3333-4444-8555-666666666666").unwrap();
                writer
                    .activate_session_target(
                        other_session,
                        second_path.clone(),
                        dir.path().to_path_buf(),
                    )
                    .unwrap();
                for message in &loaded.messages_in_order {
                    writer.append_to_path(&path, message).await.unwrap();
                }
                assert_eq!(std::fs::read_to_string(&path).unwrap(), fixture.repeat(2));
                assert!(!second_path.exists());
            }
        }
    }

    #[tokio::test]
    async fn retry_reparenting_preserves_native_peer_exact_strings() {
        let (dir, path, writer) = temp_writer("retry-exact-reparent");
        let fixture = include_str!("../../tests/fixtures/handback_exact_utf16_2_1_286.jsonl");
        let first = fixture.lines().next().unwrap();
        let mut child = Utf16JsonProjection::parse(first).unwrap();
        child
            .set_field("parentUuid", Utf16JsonProjection::from("rejected"))
            .unwrap();
        child
            .set_field(
                "opaque",
                Utf16JsonProjection::parse(r#"{"\ud800":"opaque value"}"#).unwrap(),
            )
            .unwrap();
        let opaque = child.subprojection("/opaque").unwrap();
        let child_line = child.to_json_string().unwrap();
        let body = format!(
            "{{\"type\":\"user\",\"uuid\":\"root\",\"parentUuid\":null}}\n{{\"type\":\"assistant\",\"uuid\":\"rejected\",\"parentUuid\":\"root\",\"message\":{{\"id\":\"rejected-model-id\"}}}}\n{child_line}\n"
        );
        std::fs::write(&path, body).unwrap();
        writer
            .bootstrap_session_message_identity_snapshot(&path)
            .await
            .unwrap();
        assert_eq!(
            writer
                .remove_retry_attempt("rejected-model-id")
                .await
                .unwrap()
                .as_deref(),
            child.string_at("/uuid")
        );
        let output = std::fs::read_to_string(&path).unwrap();
        assert_eq!(output.lines().count(), 2);
        let repaired = Utf16JsonProjection::parse(output.lines().last().unwrap()).unwrap();
        assert_eq!(repaired.string_at("/parentUuid"), Some("root"));
        assert_eq!(
            repaired
                .subprojection("/origin")
                .unwrap()
                .to_json_string()
                .unwrap(),
            child
                .subprojection("/origin")
                .unwrap()
                .to_json_string()
                .unwrap()
        );
        let repaired_opaque = repaired.subprojection("/opaque").unwrap();
        assert_eq!(
            repaired_opaque.to_json_string().unwrap(),
            opaque.to_json_string().unwrap()
        );
        assert_eq!(
            repaired_opaque.object_entries().unwrap()[0].code_units,
            [0xd800]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn native_exact_append_cold_retry_preserves_units_uuid_and_pinned_session() {
        let dir = tempfile::tempdir().unwrap();
        let state_root = dir.path().join("session-state");
        std::fs::create_dir_all(&state_root).unwrap();
        let session_a = SessionId::parse_prefixed("11111111-2222-4333-8444-555555555555").unwrap();
        let session_b = SessionId::parse_prefixed("22222222-3333-4444-8555-666666666666").unwrap();
        let path_a = dir.path().join(format!("{}.jsonl", session_a.as_uuid()));
        let path_b = dir.path().join(format!("{}.jsonl", session_b.as_uuid()));
        let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(
            dir.path().to_path_buf(),
        ));
        let durable = Arc::new(DurableTranscriptWriter::open(&state_root).unwrap());
        let writer = JsonlWriter::new(path_a.clone(), fs.clone()).with_durable_lock(durable);
        writer
            .activate_session_target(session_a, path_a.clone(), dir.path().to_path_buf())
            .unwrap();
        writer
            .activate_session_target(session_b, path_b.clone(), dir.path().to_path_buf())
            .unwrap();
        let fixture = include_str!("../../tests/fixtures/handback_exact_utf16_2_1_286.jsonl");
        let prepared: Vec<_> = fixture
            .lines()
            .map(|line| Utf16JsonProjection::parse(line).unwrap())
            .collect();
        for row in &prepared {
            let delivery = format!("subagent-handback:{}", row.string_at("/uuid").unwrap());
            assert_eq!(
                writer
                    .append_native_json_once_durable_for_session_with_tip(
                        session_a,
                        &delivery,
                        row.clone()
                    )
                    .await
                    .unwrap(),
                (TranscriptAppendOutcome::Appended, true)
            );
        }
        let raw = std::fs::read_to_string(&path_a).unwrap();
        assert_eq!(raw, fixture);
        assert!(
            !path_b.exists(),
            "late native peer appends stay in their originating session"
        );
        assert!(!raw.contains("deliveryId"));
        assert!(!raw.contains("utf16_code_units"));
        drop(writer);

        let cold = JsonlWriter::new(path_b.clone(), fs).with_durable_lock(Arc::new(
            DurableTranscriptWriter::open(&state_root).unwrap(),
        ));
        cold.activate_session_target(session_a, path_a.clone(), dir.path().to_path_buf())
            .unwrap();
        cold.activate_session_target(session_b, path_b.clone(), dir.path().to_path_buf())
            .unwrap();
        for (index, row) in prepared.iter().enumerate() {
            let delivery = format!("subagent-handback:{}", row.string_at("/uuid").unwrap());
            assert_eq!(
                cold.append_native_json_once_durable_for_session_with_tip(
                    session_a,
                    &delivery,
                    row.clone()
                )
                .await
                .unwrap(),
                (
                    TranscriptAppendOutcome::AlreadyPresent,
                    index == prepared.len() - 1
                )
            );
        }
        assert_eq!(std::fs::read_to_string(&path_a).unwrap(), raw);
        let mut different_units = prepared[0].clone();
        different_units
            .set_string_units("/origin/body", &[0xde00])
            .unwrap();
        assert!(matches!(
            cold.append_native_json_once_durable_for_session_with_tip(
                session_a,
                "subagent-handback:aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaa1",
                different_units
            )
            .await,
            Err(TranscriptWriterError::DeliveryConflict { .. })
        ));
        let mut display_only = prepared[0].clone();
        display_only
            .set_string_units("/origin/body", &[0xfffd])
            .unwrap();
        assert!(matches!(
            cold.append_native_json_once_durable_for_session_with_tip(
                session_a,
                "subagent-handback:aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaa1",
                display_only
            )
            .await,
            Err(TranscriptWriterError::DeliveryConflict { .. })
        ));
        let loaded = crate::jsonl::reader::route_lines(&raw);
        assert_eq!(loaded.malformed_line_count, 0);
        assert_eq!(loaded.messages_in_order.len(), 4);
        let origin = loaded.messages_in_order[0]
            .origin_projection
            .as_ref()
            .unwrap();
        assert_eq!(origin.string_units("/body"), Some(vec![0xd83d]));
        assert_eq!(origin.string_at("/kind"), Some("peer"));
    }

    #[tokio::test]
    async fn durable_ordinary_append_stays_constant_work_and_last_write_wins() {
        let (dir, path, _) = temp_writer("durable-ordinary-last-write");
        let state_root = dir.join("session-state");
        std::fs::create_dir_all(&state_root).expect("create state root");
        let durable =
            Arc::new(DurableTranscriptWriter::open(&state_root).expect("open durable transaction"));
        let fs: Arc<dyn FileSystem> =
            Arc::new(platform_posix::fs::PosixFileSystem::new(dir.clone()));
        let writer = JsonlWriter::new(path.clone(), fs).with_durable_lock(durable.clone());
        let session_id =
            SessionId::parse_prefixed("11111111-2222-3333-4444-555555555555").expect("session id");
        writer
            .activate_session_target(session_id, path.clone(), dir.clone())
            .expect("activate target");

        let message = |content: &str| {
            read_message(serde_json::json!({
                "parentUuid": null,
                "isSidechain": false,
                "type": "user",
                "message": {"role": "user", "content": content},
                "uuid": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "timestamp": "2026-09-06T00:00:00.000Z",
                "cwd": dir.to_string_lossy(),
                "sessionId": session_id.as_uuid().to_string(),
                "version": "test"
            }))
        };
        writer
            .append(&message("first"))
            .await
            .expect("first append");
        writer
            .append(&message("updated"))
            .await
            .expect("updated append");

        let raw = std::fs::read_to_string(&path).expect("read transcript");
        assert_eq!(
            raw.lines().count(),
            2,
            "ordinary updates remain append-only"
        );
        assert!(!raw.contains("deliveryId"));
        let identities = writer
            .read_session_message_identity_snapshot(&path)
            .await
            .expect("durable row identities");
        assert_eq!(
            identities.by_uuid["aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"], 0,
            "content updates retain the original stream identity for an outer UUID"
        );
        assert_eq!(
            identities.next_message_index, 1,
            "an update for an active UUID does not allocate a second stream index"
        );
        assert_eq!(
            durable.duplicate_scan_count_for_test(),
            0,
            "ordinary durable appends must not rescan transcript history"
        );
        let loaded = crate::jsonl::reader::route_lines(&raw);
        assert_eq!(
            loaded
                .by_uuid
                .get("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
                .and_then(|message| message.message.string_at("/content")),
            Some("updated"),
            "the existing loader's duplicate-uuid last-write rule is preserved"
        );

        drop(writer);
        let cold_fs: Arc<dyn FileSystem> =
            Arc::new(platform_posix::fs::PosixFileSystem::new(dir.clone()));
        let cold = JsonlWriter::new(path.clone(), cold_fs).with_durable_lock(Arc::new(
            DurableTranscriptWriter::open(&state_root).expect("reopen durable transaction"),
        ));
        cold.activate_session_target(session_id, path.clone(), dir.clone())
            .expect("activate restarted target");
        let after_restart = cold
            .read_session_message_identity_snapshot(&path)
            .await
            .expect("identity after writer restart");
        assert_eq!(
            after_restart.by_uuid["aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"],
            0
        );
        assert_eq!(after_restart.next_message_index, 1);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn late_origin_appends_stay_pinned_after_active_session_switch() {
        let (dir, path_a, _) = temp_writer("durable-origin-pinning");
        let path_b = dir.join("22222222-3333-4444-8555-666666666666.jsonl");
        let state_a = dir.join("state-a");
        let state_b = dir.join("state-b");
        std::fs::create_dir_all(&state_a).unwrap();
        std::fs::create_dir_all(&state_b).unwrap();
        let durable_a = Arc::new(DurableTranscriptWriter::open(&state_a).unwrap());
        let durable_b = Arc::new(DurableTranscriptWriter::open(&state_b).unwrap());
        let fs: Arc<dyn FileSystem> =
            Arc::new(platform_posix::fs::PosixFileSystem::new(dir.clone()));
        let writer = JsonlWriter::new(path_a.clone(), fs).with_durable_lock(durable_a);
        let session_a = SessionId::parse_prefixed("11111111-2222-3333-4444-555555555555").unwrap();
        let session_b = SessionId::parse_prefixed("22222222-3333-4444-8555-666666666666").unwrap();
        writer
            .activate_session_target(session_a, path_a.clone(), dir.join("project-a"))
            .unwrap();
        writer.set_durable_lock(durable_b);
        writer
            .activate_session_target(session_b, path_b.clone(), dir.join("project-b"))
            .unwrap();

        let message = |session_id: SessionId, uuid: &str, content: &str| {
            read_message(serde_json::json!({
                "parentUuid": null,
                "isSidechain": false,
                "type": "user",
                "message": {"role": "user", "content": content},
                "uuid": uuid,
                "timestamp": "2026-09-06T00:00:00.000Z",
                "cwd": dir.to_string_lossy(),
                "sessionId": session_id.as_uuid().to_string(),
                "version": "test"
            }))
        };
        writer
            .append(&message(
                session_b,
                "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                "active B",
            ))
            .await
            .unwrap();
        writer
            .append_to_path(
                &path_a,
                &message(
                    session_a,
                    "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                    "late ordinary A",
                ),
            )
            .await
            .unwrap();
        writer
            .append_json_once_durable_for_session(
                session_a,
                "fusion-delivery:fu_0123456789abcdef0123456789abcdef",
                source_value(&json!({
                    "uuid": "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
                    "parentUuid": null,
                    "type": "user",
                    "sessionId": session_a.as_uuid().to_string(),
                    "message": {"role": "user", "content": "late Fusion A"}
                })),
            )
            .await
            .unwrap();

        let a = std::fs::read_to_string(&path_a).unwrap();
        let b = std::fs::read_to_string(&path_b).unwrap();
        assert!(a.contains("late ordinary A"));
        assert!(a.contains("late Fusion A"));
        assert!(!a.contains("active B"));
        let fusion_row: serde_json::Value =
            serde_json::from_str(a.lines().last().unwrap()).unwrap();
        assert_eq!(
            fusion_row
                .get("parentUuid")
                .and_then(serde_json::Value::as_str),
            Some("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"),
            "origin-session parent resolution happens under A's transaction"
        );
        assert!(b.contains("active B"));
        assert!(!b.contains("late ordinary A"));
        assert!(!b.contains("late Fusion A"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn server_fallback_tombstone_deletes_exact_row_without_reparenting_children() {
        let (dir, path, writer) = temp_writer("server-fallback-tombstone");
        let root = "{\"type\":\"user\",\"uuid\":\"root\",\"parentUuid\":null}\n";
        let removed = "{\"type\":\"assistant\",\"uuid\":\"removed\",\"parentUuid\":\"root\",\"message\":{\"id\":\"provider-a\"}}\n";
        let child = "{\"type\":\"assistant\",\"uuid\":\"child\",\"parentUuid\":\"removed\",\"message\":{\"id\":\"provider-b\"}}\n";
        let untouched = "{ \"type\": \"custom-title\", \"customTitle\": \"keep exact bytes\" }\n";
        let source = format!("{root}{removed}{child}{untouched}");
        std::fs::write(&path, &source).unwrap();
        writer
            .bootstrap_session_message_identity_snapshot(&path)
            .await
            .unwrap();

        assert!(writer.remove_message_by_uuid("removed").await.unwrap());
        let expected = format!("{root}{child}{untouched}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
        let rows: Vec<serde_json::Value> = expected
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows[1]["parentUuid"], "removed");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn durable_server_fallback_tombstone_uses_atomic_transaction_path() {
        let (dir, path, _) = temp_writer("durable-server-fallback-tombstone");
        let session_state = dir.join("session-state");
        std::fs::create_dir_all(&session_state).unwrap();
        let root = "{\"type\":\"user\",\"uuid\":\"root\",\"parentUuid\":null}\n";
        let removed = "{\"type\":\"assistant\",\"uuid\":\"removed\",\"parentUuid\":\"root\"}\n";
        let child = "{\"type\":\"assistant\",\"uuid\":\"child\",\"parentUuid\":\"removed\"}\n";
        std::fs::write(&path, format!("{root}{removed}{child}")).unwrap();
        let fs: Arc<dyn FileSystem> =
            Arc::new(platform_posix::fs::PosixFileSystem::new(dir.clone()));
        let writer = JsonlWriter::new(path.clone(), fs).with_durable_lock(Arc::new(
            DurableTranscriptWriter::open(&session_state).unwrap(),
        ));
        writer
            .activate_session_target(SessionId::new(), path.clone(), dir.to_path_buf())
            .unwrap();
        writer
            .bootstrap_session_message_identity_snapshot(&path)
            .await
            .unwrap();
        let before = writer
            .read_session_message_identity_snapshot(&path)
            .await
            .unwrap();
        assert_eq!(before.by_uuid["root"], 0);
        assert_eq!(before.by_uuid["removed"], 1);
        assert_eq!(before.by_uuid["child"], 2);
        assert_eq!(before.next_message_index, 3);

        assert!(writer.remove_message_by_uuid("removed").await.unwrap());
        let after = writer
            .read_session_message_identity_snapshot(&path)
            .await
            .unwrap();
        assert!(!after.by_uuid.contains_key("removed"));
        assert_eq!(after.by_uuid["child"], 2);
        assert_eq!(after.next_message_index, 3);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{root}{child}")
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn server_fallback_tail_hit_streams_large_files_in_both_writer_modes() {
        use std::io::{BufWriter, Read, Seek, SeekFrom, Write};

        const FILE_BYTES: u64 = 50 * 1024 * 1024 + 4096;
        let content = "x".repeat(900);

        for durable in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let session_id =
                SessionId::parse_prefixed("11111111-2222-4333-8444-555555555555").unwrap();
            let path = dir.path().join(format!("{}.jsonl", session_id.as_uuid()));
            let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(
                dir.path().to_path_buf(),
            ));
            let mut writer = JsonlWriter::new(path.clone(), fs);
            if durable {
                let state_root = dir.path().join("session-state");
                std::fs::create_dir_all(&state_root).unwrap();
                writer = writer.with_durable_lock(Arc::new(
                    DurableTranscriptWriter::open(&state_root).unwrap(),
                ));
                writer
                    .activate_session_target(session_id, path.clone(), dir.path().to_path_buf())
                    .unwrap();
            }

            let prefix_row = |index: u64| {
                format!(
                    "{{\"type\":\"assistant\",\"uuid\":\"prefix-{index:09}\",\"parentUuid\":null,\"message\":{{\"content\":\"{content}\"}}}}\n"
                )
                .into_bytes()
            };
            let first_prefix_row = prefix_row(0);
            let row_count = FILE_BYTES / first_prefix_row.len() as u64 + 1;
            let mut source = BufWriter::new(std::fs::File::create(&path).unwrap());
            for index in 0..row_count {
                source.write_all(&prefix_row(index)).unwrap();
            }
            let target_line = serde_json::to_vec(&serde_json::json!({
                "type": "assistant",
                "uuid": "fallback-target",
                "parentUuid": format!("prefix-{:09}", row_count - 1),
                "message": {"content": "discard me"}
            }))
            .unwrap();
            let mut target_line = target_line;
            target_line.push(b'\n');
            let child_line = serde_json::to_vec(&serde_json::json!({
                "type": "assistant",
                "uuid": "fallback-child",
                "parentUuid": "fallback-target",
                "message": {"content": "keep me"}
            }))
            .unwrap();
            let mut child_line = child_line;
            child_line.push(b'\n');
            source.write_all(&target_line).unwrap();
            source.write_all(&child_line).unwrap();
            source.flush().unwrap();
            drop(source);

            writer
                .bootstrap_session_message_identity_snapshot(&path)
                .await
                .expect("import existing Native rows before Host tombstone mutation");

            let original_len = std::fs::metadata(&path).unwrap().len();
            assert!(original_len > 50 * 1024 * 1024);
            assert!(writer
                .remove_message_by_uuid("fallback-target")
                .await
                .unwrap());
            assert_eq!(
                std::fs::metadata(&path).unwrap().len(),
                original_len - target_line.len() as u64
            );

            let mut output = std::fs::File::open(&path).unwrap();
            let mut first = vec![0; first_prefix_row.len()];
            output.read_exact(&mut first).unwrap();
            assert_eq!(first, first_prefix_row);

            let middle_index = row_count / 2;
            output
                .seek(SeekFrom::Start(
                    middle_index * first_prefix_row.len() as u64,
                ))
                .unwrap();
            let mut middle = vec![0; first_prefix_row.len()];
            output.read_exact(&mut middle).unwrap();
            assert_eq!(middle, prefix_row(middle_index));

            output
                .seek(SeekFrom::End(-(child_line.len() as i64)))
                .unwrap();
            let mut suffix = vec![0; child_line.len()];
            output.read_exact(&mut suffix).unwrap();
            assert_eq!(suffix, child_line);
        }
    }

    #[tokio::test]
    async fn server_fallback_tombstone_rewrites_small_file_when_uuid_is_outside_tail() {
        let (dir, path, writer) = temp_writer("server-fallback-tombstone-old-row");
        let mut source =
            String::from("{\"type\":\"assistant\",\"uuid\":\"target\",\"parentUuid\":null}\n");
        for index in 0..80 {
            source.push_str(&format!(
                "{{\"type\":\"assistant\",\"uuid\":\"keep-{index}\",\"parentUuid\":null,\"message\":{{\"content\":\"{}\"}}}}\n",
                "x".repeat(1_000)
            ));
        }
        assert!(source.len() > TOMBSTONE_TAIL_BYTES as usize);
        std::fs::write(&path, &source).unwrap();
        writer
            .bootstrap_session_message_identity_snapshot(&path)
            .await
            .unwrap();

        assert!(writer.remove_message_by_uuid("target").await.unwrap());
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            written,
            source
                .strip_prefix("{\"type\":\"assistant\",\"uuid\":\"target\",\"parentUuid\":null}\n")
                .unwrap()
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn retry_removal_reparents_all_block_siblings_and_preserves_side_record_bytes() {
        let (dir, path, writer) = temp_writer("retry-remove");
        let source = concat!(
            "{\"type\":\"user\",\"uuid\":\"user\",\"parentUuid\":null}\n",
            "{\"type\":\"assistant\",\"uuid\":\"block1\",\"parentUuid\":\"user\",\"message\":{\"id\":\"attempt\"}}\n",
            "{\"type\":\"assistant\",\"uuid\":\"block2\",\"parentUuid\":\"block1\",\"message\":{\"id\":\"attempt\"}}\n",
            "{ \"type\": \"custom-title\", \"customTitle\": \"keep exact spaces\" }\n",
            "{\"type\":\"user\",\"uuid\":\"nudge\",\"parentUuid\":\"block2\"}\n"
        );
        std::fs::write(&path, source).unwrap();
        writer
            .bootstrap_session_message_identity_snapshot(&path)
            .await
            .unwrap();
        assert_eq!(
            writer.remove_retry_attempt("attempt").await.unwrap(),
            Some("nudge".into())
        );
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(!body.contains("block1"));
        assert!(!body.contains("block2"));
        assert!(body
            .contains("{ \"type\": \"custom-title\", \"customTitle\": \"keep exact spaces\" }\n"));
        let child: serde_json::Value = serde_json::from_str(body.lines().last().unwrap()).unwrap();
        assert_eq!(child["parentUuid"], "user");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn context_collapse_side_records_are_byte_exact() {
        let (dir, path, writer) = temp_writer("context-collapse-records");
        let session_id = "11111111-2222-4333-8444-555555555555";
        writer
            .append_context_collapse_commit(
                session_id,
                "0000000000000001",
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "<collapsed id=\"0000000000000001\">summary</collapsed>",
                "summary",
                "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
            )
            .await
            .expect("commit");
        writer
            .append_context_collapse_snapshot(
                session_id,
                &serde_json::json!([{
                    "startUuid": "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                    "endUuid": "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
                    "summary": "next",
                    "risk": 0.25,
                    "stagedAt": 123,
                }]),
                true,
                90_000,
            )
            .await
            .expect("snapshot");
        writer
            .append_context_collapse_reset(session_id, "compact")
            .await
            .expect("reset");

        let bytes = std::fs::read_to_string(&path).expect("read transcript");
        assert_eq!(
            bytes,
            concat!(
                "{\"type\":\"marble-origami-commit\",\"sessionId\":\"11111111-2222-4333-8444-555555555555\",\"collapseId\":\"0000000000000001\",\"summaryUuid\":\"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa\",\"summaryContent\":\"<collapsed id=\\\"0000000000000001\\\">summary</collapsed>\",\"summary\":\"summary\",\"firstArchivedUuid\":\"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb\",\"lastArchivedUuid\":\"cccccccc-cccc-4ccc-8ccc-cccccccccccc\"}\n",
                "{\"type\":\"marble-origami-snapshot\",\"sessionId\":\"11111111-2222-4333-8444-555555555555\",\"staged\":[{\"startUuid\":\"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb\",\"endUuid\":\"cccccccc-cccc-4ccc-8ccc-cccccccccccc\",\"summary\":\"next\",\"risk\":0.25,\"stagedAt\":123}],\"armed\":true,\"lastSpawnTokens\":90000}\n",
                "{\"type\":\"marble-origami-reset\",\"sessionId\":\"11111111-2222-4333-8444-555555555555\",\"reason\":\"compact\"}\n",
            )
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The backstop counter accounts every appended byte (payload INCLUDING the
    /// trailing newline) and arms at `kI/2` = 32768, matching the oracle's
    /// `bytesSinceMetadataReAppend >= kI/2` gate.
    #[tokio::test]
    async fn appends_accumulate_the_backstop_counter() {
        let (dir, path, writer) = temp_writer("backstop");

        assert_eq!(writer.bytes_since_metadata_re_append(), 0);
        assert!(!writer.metadata_re_append_due());

        writer
            .append_custom_title("11111111-2222-3333-4444-555555555555", "t".into())
            .await
            .expect("append");
        let on_disk = std::fs::metadata(&path).expect("stat").len() as usize;
        assert_eq!(
            writer.bytes_since_metadata_re_append(),
            on_disk,
            "counter must equal the bytes actually written"
        );
        assert!(!writer.metadata_re_append_due());

        // Push it over the 32 KiB line with one big title.
        writer
            .append_custom_title(
                "11111111-2222-3333-4444-555555555555",
                "p".repeat(METADATA_REAPPEND_BACKSTOP_BYTES).into(),
            )
            .await
            .expect("append big");
        // CORRECTED. This used to assert `metadata_re_append_due()` is still
        // true here. That held only while nothing polled the backstop: the
        // public append paths now fire `maybe_re_append_metadata` on the way
        // out, so crossing the line SELF-CLEARS the counter. Asserting it stays
        // due would now be asserting that the wiring does not work.
        assert!(
            !writer.metadata_re_append_due(),
            "crossing the backstop must have triggered a re-append and reset the counter"
        );

        writer.reset_metadata_re_append_counter();
        assert_eq!(writer.bytes_since_metadata_re_append(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }
    /// THE WIRING TEST: a long session re-appends its metadata on its own.
    ///
    /// The mechanism was previously driven only by an explicit call nobody made,
    /// which made the whole port inert. The writer now owns the state and polls
    /// the backstop itself after every append, so metadata that scrolls out of
    /// the 64 KiB tail window comes back without anyone asking.
    ///
    /// `session_id` is derived from the transcript's own file stem — the writer
    /// already knows which session it is writing, so no caller has to thread it.
    #[tokio::test]
    async fn appends_alone_drive_the_metadata_backstop() {
        let (_dir, path, writer) = temp_writer("drive-backstop");
        let session_id = path.file_stem().unwrap().to_str().unwrap();

        // A title recorded through the writer is remembered in its own state.
        writer
            .append_custom_title(
                session_id,
                "Long Session".into(),
            )
            .await
            .unwrap();

        // Bury it under more than a full TAIL WINDOW of transcript — not merely
        // the backstop. The backstop (32 KiB) is half the window (64 KiB), so
        // filling only past the backstop leaves the title still visible in the
        // tail and the assertion below would pass without anything being
        // re-appended at all.
        // Through a REAL append path — `append_payload_for_test` bypasses the
        // public entry points and so would never trip the poll, making this test
        // green for the wrong reason.
        let mut wrote = 0usize;
        while wrote < super::METADATA_REAPPEND_BACKSTOP_BYTES * 2 + 8192 {
            let rec = serde_json::json!({ "type": "filler", "pad": "f".repeat(4000) });
            writer.append_side_record_for_test(&rec).await;
            wrote += 4050;
        }

        let tail = crate::jsonl::read_tail(&path);
        assert!(
            tail.contains("custom-title") && tail.contains("Long Session"),
            "the backstop must have restored the title into the tail window \
             without an explicit re_append call"
        );
    }

    fn metadata_identity_writer(
        durable: bool,
    ) -> (tempfile::TempDir, PathBuf, SessionId, JsonlWriter) {
        let root = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let path = root.path().join(format!("{}.jsonl", id.as_uuid()));
        let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(
            root.path().to_path_buf(),
        ));
        let writer = JsonlWriter::new(path.clone(), fs);
        let writer = if durable {
            let authority = root.path().join("authority");
            std::fs::create_dir(&authority).unwrap();
            let lock = Arc::new(DurableTranscriptWriter::open(authority).unwrap());
            let writer = writer.with_durable_lock(lock.clone());
            writer.activate_session_target_with_durable_lock(
                id,
                path.clone(),
                root.path().to_path_buf(),
                lock,
            );
            writer
        } else {
            writer
        };
        (root, path, id, writer)
    }

    #[tokio::test]
    async fn uppercase_uuid_filename_keeps_one_canonical_metadata_owner_and_the_selected_path() {
        let upper = "ABCDEFAB-CDEF-4ABC-8DEF-ABCDEFABCDEF";
        let sid = upper.to_ascii_lowercase();
        let id = SessionId::parse_prefixed(&sid).unwrap();
        for durable in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join(format!("{upper}.jsonl"));
            let content = format!(
                "{}\n{}\n{}\n",
                json!({"type":"user","uuid":uuid::Uuid::new_v4(),"sessionId":sid,"parentUuid":null,"message":{"role":"user","content":"selected source"}}),
                json!({"type":"custom-title","sessionId":sid,"customTitle":"saved title"}),
                json!({"type":"relocated","sessionId":sid,"relocatedCwd":"/selected-root"}),
            );
            std::fs::write(&path, &content).unwrap();
            let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(
                root.path().to_path_buf(),
            ));
            let mut writer = JsonlWriter::new(path.clone(), fs);
            if durable {
                let authority = root.path().join("state");
                std::fs::create_dir_all(&authority).unwrap();
                writer = writer.with_durable_lock(Arc::new(
                    DurableTranscriptWriter::open(&authority).unwrap(),
                ));
                writer
                    .activate_session_target(id, path.clone(), root.path().to_path_buf())
                    .unwrap();
            }
            writer
                .bootstrap_session_message_identity_snapshot(&path)
                .await
                .unwrap();
            let captured = JsonlWriter::capture_session_metadata_source(&content, upper);
            assert_eq!(captured.len(), 2);
            writer.install_session_metadata_source(upper, &captured);
            writer
                .append_custom_title(&sid, "renamed selected copy".into())
                .await
                .unwrap();
            writer.append_permission_mode("acceptEdits").await.unwrap();
            writer.append_session_mode("chat").await.unwrap();
            writer
                .append_worktree_state_for_current_session(None)
                .await
                .unwrap();
            let mut message = identity_row(
                "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
                &"x".repeat(METADATA_REAPPEND_BACKSTOP_BYTES),
            );
            message.session_id = sid.clone();
            writer.append(&message).await.unwrap();
            let contents = std::fs::read_to_string(&path).unwrap();
            let tail = read_tail(&path);
            for value in [
                "renamed selected copy",
                "acceptEdits",
                "\"sessionMode\":\"chat\"",
                "\"worktreeSession\":null",
                "/selected-root",
            ] {
                assert!(tail.contains(value), "missing {value} (durable={durable})");
            }
            let loaded = crate::jsonl::route_lines(&contents);
            assert_eq!(loaded.custom_titles[&sid], "renamed selected copy");
            assert_eq!(writer.active_path(), path);
            let mirrors = writer.metadata_states.read().unwrap();
            assert_eq!(mirrors.len(), 1);
            assert!(mirrors.contains_key(&sid));
            let files = std::fs::read_dir(root.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "jsonl")
                })
                .map(|entry| entry.file_name())
                .collect::<Vec<_>>();
            assert_eq!(files, [std::ffi::OsString::from(format!("{upper}.jsonl"))]);
        }
    }

    #[tokio::test]
    async fn metadata_waiting_for_relocation_follows_the_same_session_to_its_new_target() {
        use std::future::{poll_fn, Future};
        use std::task::Poll;

        for durable in [false, true] {
            for operation in ["backstop", "title", "permission", "mode", "worktree"] {
                let root = tempfile::tempdir().unwrap();
                let id = SessionId::new();
                let sid = id.as_uuid().to_string();
                let old = root.path().join("A").join(format!("{sid}.jsonl"));
                let new = root.path().join("B").join(format!("{sid}.jsonl"));
                let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(
                    root.path().to_path_buf(),
                ));
                let mut writer = JsonlWriter::new(old.clone(), fs);
                if durable {
                    let authority = root.path().join("state");
                    std::fs::create_dir_all(&authority).unwrap();
                    writer = writer.with_durable_lock(Arc::new(
                        DurableTranscriptWriter::open(&authority).unwrap(),
                    ));
                    writer
                        .activate_session_target(id, old.clone(), root.path().join("A"))
                        .unwrap();
                }
                writer
                    .append_custom_title(&sid, "before-move".into())
                    .await
                    .unwrap();
                writer
                    .bytes_since_metadata_re_append
                    .store(METADATA_REAPPEND_BACKSTOP_BYTES, Ordering::Relaxed);
                let slot = writer.session_metadata_state(&sid, &old).await;
                let held = slot.lock().await;
                let movement = writer.retarget_with_relocation(new.clone(), &sid, "/new-workspace");
                tokio::pin!(movement);
                poll_fn(|cx| {
                    assert!(movement.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
                let metadata = async {
                    match operation {
                        "backstop" => {
                            assert!(writer.maybe_re_append_metadata().await > 0);
                            Ok(())
                        }
                        "title" => writer.append_custom_title(&sid, "during-move".into()).await,
                        "permission" => writer.append_permission_mode("acceptEdits").await,
                        "mode" => writer.append_session_mode("chat").await,
                        "worktree" => writer.append_worktree_state(&sid, None).await,
                        _ => unreachable!(),
                    }
                };
                tokio::pin!(metadata);
                poll_fn(|cx| {
                    assert!(metadata.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
                drop(held);
                movement.await.unwrap();
                metadata.await.unwrap();
                assert!(
                    !old.exists(),
                    "{operation} recreated the old target (durable={durable})"
                );
                assert_eq!(writer.active_path(), new);
                let contents = std::fs::read_to_string(new).unwrap();
                assert!(contents.contains("before-move"));
                assert!(contents.contains("/new-workspace"));
                match operation {
                    "backstop" => assert!(contents.matches("before-move").count() >= 2),
                    "title" => assert!(contents.contains("during-move")),
                    "permission" => assert!(contents.contains("acceptEdits")),
                    "mode" => assert!(contents.contains("\"sessionMode\":\"chat\"")),
                    "worktree" => assert!(contents.contains("\"worktreeSession\":null")),
                    _ => unreachable!(),
                }
            }
        }
    }

    #[tokio::test]
    async fn fresh_title_then_first_message_persists_with_message_index_zero() {
        for durable in [false, true] {
            let (_root, path, id, writer) = metadata_identity_writer(durable);
            assert!(!path.exists());
            writer
                .append_custom_title(&id.as_uuid().to_string(), "Before first message".into())
                .await
                .unwrap();
            let title_only = std::fs::read_to_string(&path).unwrap();
            assert!(title_only.contains("Before first message"));
            let mut row = identity_row("first-human-message", "First message after the title");
            row.session_id = id.as_uuid().to_string();
            writer.append(&row).await.unwrap();
            let content = std::fs::read_to_string(&path).unwrap();
            assert!(content.starts_with(&title_only));
            assert!(content.contains("First message after the title"));
            let identities = writer
                .read_session_message_identity_snapshot(&path)
                .await
                .unwrap();
            assert_eq!(identities.by_uuid["first-human-message"], 0);
            assert_eq!(identities.next_message_index, 1);
        }
    }

    #[tokio::test]
    async fn fresh_metadata_cannot_bind_a_foreign_uuid_or_leak_across_session_switches() {
        let (_root, path_a, id_a, writer) = metadata_identity_writer(false);
        let id_b = SessionId::new();
        let path_b = path_a.with_file_name(format!("{}.jsonl", id_b.as_uuid()));
        assert!(writer
            .append_custom_title(&id_b.as_uuid().to_string(), "Unbound B title".into())
            .await
            .is_err());
        assert!(!path_a.exists());
        assert!(!path_b.exists());
        writer
            .append_custom_title(&id_a.as_uuid().to_string(), "A title".into())
            .await
            .unwrap();
        writer.retarget(path_b.clone()).await;
        let mut row = identity_row("first-b-message", &"B content ".repeat(10_000));
        row.session_id = id_b.as_uuid().to_string();
        writer.append(&row).await.unwrap();
        writer
            .append_custom_title(&id_b.as_uuid().to_string(), "Bound B title".into())
            .await
            .unwrap();
        let content_b = std::fs::read_to_string(path_b).unwrap();
        assert!(content_b.contains("Bound B title"));
        assert!(!content_b.contains("A title"));
        assert!(!content_b.contains("Unbound B title"));
        assert!(!content_b.contains(&id_a.as_uuid().to_string()));
        let content_a = std::fs::read_to_string(path_a).unwrap();
        assert!(content_a.contains("A title"));
        assert!(!content_a.contains("Bound B title"));
    }

    #[tokio::test]
    async fn metadata_on_nonempty_native_transcript_does_not_import_its_identity_history() {
        for durable in [false, true] {
            let (_root, path, id, writer) = metadata_identity_writer(durable);
            let mut native = identity_row("native-history", "Existing Native history");
            native.session_id = id.as_uuid().to_string();
            std::fs::write(
                &path,
                format!("{}\n", serde_json::to_string(&native).unwrap()),
            )
            .unwrap();
            writer
                .append_custom_title(
                    &id.as_uuid().to_string(),
                    "Metadata preserves import guard".into(),
                )
                .await
                .unwrap();
            let mut next = identity_row("next-human-message", "After explicit import");
            next.session_id = id.as_uuid().to_string();
            assert!(writer.append(&next).await.is_err());
            assert_eq!(
                writer
                    .read_session_message_identity_snapshot(&path)
                    .await
                    .unwrap(),
                SessionMessageIdentitySnapshot::default(),
            );
            writer
                .bootstrap_session_message_identity_snapshot(&path)
                .await
                .unwrap();
            writer.append(&next).await.unwrap();
            let identities = writer
                .read_session_message_identity_snapshot(&path)
                .await
                .unwrap();
            assert_eq!(identities.by_uuid["native-history"], 0);
            assert_eq!(identities.by_uuid["next-human-message"], 1);
        }
    }

    #[tokio::test]
    async fn external_title_append_survives_a_large_writer_append() {
        let (dir, path, writer) = temp_writer("external-title");
        let sid = "11111111-2222-3333-4444-555555555555";
        writer.append_custom_title(sid, "old".into()).await.unwrap();

        // Simulate Electron's metadata-only append while the engine writer is
        // still alive, then push the new title outside the bounded tail before
        // the writer's own counter crosses its backstop.
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(
                file,
                "{}",
                serde_json::json!({
                    "type": "custom-title",
                    "customTitle": "new",
                    "sessionId": sid,
                })
            )
            .unwrap();
            writeln!(
                file,
                "{}",
                "x".repeat(crate::jsonl::LITE_READ_BUF_SIZE + 1024)
            )
            .unwrap();
        }

        let mut wrote = 0usize;
        while wrote < super::METADATA_REAPPEND_BACKSTOP_BYTES {
            writer
                .append_side_record_for_test(&serde_json::json!({
                    "type": "filler",
                    "pad": "f".repeat(4000),
                }))
                .await;
            wrote += 4050;
        }

        let tail = crate::jsonl::read_tail(&path);
        assert!(tail.contains("\"customTitle\":\"new\""));
        assert!(!tail.contains("\"customTitle\":\"old\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// End-to-end: metadata that has scrolled out of the 64 KiB tail window is
    /// re-appended so a tail-scanning reader sees it again — the entire point
    /// of `reAppendSessionMetadata`.
    #[tokio::test]
    async fn re_append_restores_metadata_into_the_tail_window() {
        let (dir, path, writer) = temp_writer("restore");
        let sid = "11111111-2222-3333-4444-555555555555";

        writer
            .append_custom_title(sid, "Kept Title".into())
            .await
            .unwrap();
        // Bury it under more than a full tail window of transcript.
        let filler = format!("{}\n", "f".repeat(4095));
        for _ in 0..20 {
            writer.append_payload_for_test(&filler).await;
        }
        let scrolled = crate::jsonl::read_tail(&path);
        assert!(
            !scrolled.contains("custom-title"),
            "precondition: the title must have scrolled out of the tail"
        );

        let mut state = SessionMetadataState {
            title: Some("Kept Title".into()),
            mode: Some("default".into()),
            ..Default::default()
        };
        let written = writer
            .re_append_session_metadata(&mut state, sid, true, false)
            .await
            .expect("re-append");
        assert_eq!(written, 2, "custom-title + mode");

        let tail = crate::jsonl::read_tail(&path);
        let routed = crate::jsonl::route_lines(&tail);
        assert_eq!(
            routed.custom_titles.get(sid).map(String::as_str),
            Some("Kept Title")
        );
        assert_eq!(routed.modes.get(sid).map(String::as_str), Some("default"));
        assert_eq!(
            writer.bytes_since_metadata_re_append(),
            0,
            "the counter is zeroed by the re-append"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Called twice back to back, the second call writes NOTHING: the dedup
    /// pass finds its own records in the tail. Without this the 32 KiB backstop
    /// would bloat every transcript without bound.
    #[tokio::test]
    async fn back_to_back_re_appends_write_nothing_the_second_time() {
        let (dir, path, writer) = temp_writer("dedup");
        let sid = "11111111-2222-3333-4444-555555555555";
        writer
            .append_payload_for_test("{\"type\":\"user\"}\n")
            .await;

        let mut state = SessionMetadataState {
            title: Some("T".into()),
            mode: Some("default".into()),
            pr_number: Some(7),
            pr_url: Some("https://example.test/pull/7".into()),
            pr_repository: Some("acme/widgets".into()),
            ..Default::default()
        };
        assert_eq!(
            writer
                .re_append_session_metadata(&mut state, sid, true, false)
                .await
                .unwrap(),
            3
        );
        let after_first = std::fs::read_to_string(&path).unwrap();

        assert_eq!(
            writer
                .re_append_session_metadata(&mut state, sid, true, false)
                .await
                .unwrap(),
            0,
            "identical metadata must not be written again (pr-link included, \
             despite its timestamp differing)"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            after_first,
            "the file must be byte-identical after the no-op re-append"
        );

        // …but `skip_dedup = true` forces it.
        assert_eq!(
            writer
                .re_append_session_metadata(&mut state, sid, true, true)
                .await
                .unwrap(),
            3
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A writer with nothing to say writes nothing and creates no file.
    #[tokio::test]
    async fn re_append_with_empty_state_is_a_no_op() {
        let (dir, path, writer) = temp_writer("noop");
        let mut state = SessionMetadataState::default();
        assert_eq!(
            writer
                .re_append_session_metadata(
                    &mut state,
                    "11111111-2222-3333-4444-555555555555",
                    true,
                    false
                )
                .await
                .unwrap(),
            0
        );
        assert!(!path.exists(), "no file may be created for an empty plan");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn retarget_moves_subsequent_appends_to_the_new_session_file() {
        let tmp =
            std::env::temp_dir().join(format!("lingxi-writer-retarget-{}", std::process::id(),));
        std::fs::create_dir_all(&tmp).expect("create temp dir");
        let first = tmp.join("11111111-2222-3333-4444-555555555555.jsonl");
        let second = tmp.join("66666666-7777-4888-8999-aaaaaaaaaaaa.jsonl");
        let fs: Arc<dyn FileSystem> =
            Arc::new(platform_posix::fs::PosixFileSystem::new(tmp.clone()));
        let writer = JsonlWriter::new(first.clone(), fs);

        writer
            .append_custom_title("11111111-2222-3333-4444-555555555555", "first".into())
            .await
            .expect("append first title");
        writer.retarget(second.clone()).await;
        writer
            .append_custom_title("66666666-7777-4888-8999-aaaaaaaaaaaa", "second".into())
            .await
            .expect("append second title");

        assert_eq!(writer.active_path(), second);
        assert!(std::fs::read_to_string(first)
            .unwrap()
            .contains("\"first\""));
        assert!(std::fs::read_to_string(second)
            .unwrap()
            .contains("\"second\""));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn retarget_with_relocation_records_special_cwd_before_switching_files() {
        let tmp = std::env::temp_dir().join(format!(
            "lingxi-writer-relocation-{}-{}",
            std::process::id(),
            "special"
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("create temp dir");
        let home = tmp.join("home");
        let session_id = "77777777-8888-4999-aaaa-bbbbbbbbbbbb";
        let old_cwd = "/tmp/work space/[old]";
        let new_cwd = "/tmp/work space/[new] \"quoted\"\\slash";
        let old_path = crate::jsonl::path::session_path(&home, old_cwd, session_id);
        let new_path = crate::jsonl::path::session_path(&home, new_cwd, session_id);
        assert_ne!(
            old_path, new_path,
            "special cwd should select a new project dir"
        );
        let fs: Arc<dyn FileSystem> =
            Arc::new(platform_posix::fs::PosixFileSystem::new(tmp.clone()));
        let writer = JsonlWriter::new(old_path.clone(), fs);

        writer
            .append_payload_for_test(&format!(
                "{}\n",
                serde_json::json!({
                "type": "user",
                "sessionId": session_id,
                "cwd": old_cwd,
                })
            ))
            .await;
        writer
            .retarget_with_relocation(new_path.clone(), session_id, new_cwd)
            .await
            .expect("record relocation");

        assert_eq!(writer.active_path(), new_path);
        assert!(
            !old_path.exists(),
            "cross-directory move must rehome the transcript"
        );
        let before_post = std::fs::read_to_string(&new_path).expect("read new transcript");
        let marker: serde_json::Value =
            serde_json::from_str(before_post.lines().nth(1).expect("relocation line"))
                .expect("relocation line parses");
        assert_eq!(marker["type"], "relocated");
        assert_eq!(marker["sessionId"], session_id);
        assert_eq!(marker["relocatedCwd"], new_cwd);
        assert!(before_post
            .lines()
            .next()
            .is_some_and(|line| line.contains("\"user\"")));

        writer
            .append_payload_for_test(&format!(
                "{}\n",
                serde_json::json!({ "type": "assistant", "sessionId": session_id })
            ))
            .await;
        let new_lines = std::fs::read_to_string(&new_path).expect("read new transcript");
        assert!(new_lines.contains("\"assistant\""));
        assert_eq!(
            new_lines.lines().count(),
            3,
            "pre-/cd, marker, and post-/cd stay together"
        );
        assert!(
            !old_path.exists(),
            "old project path must not present the moved session"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn retarget_with_relocation_keeps_sanitized_collision_on_one_transcript() {
        let tmp = std::env::temp_dir().join(format!(
            "lingxi-writer-relocation-{}-{}",
            std::process::id(),
            "collision"
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("create temp dir");
        let home = tmp.join("home");
        let session_id = "88888888-9999-4aaa-bbbb-cccccccccccc";
        let old_cwd = "/tmp/collision-a_b";
        let new_cwd = "/tmp/collision-a-b";
        let old_path = crate::jsonl::path::session_path(&home, old_cwd, session_id);
        let new_path = crate::jsonl::path::session_path(&home, new_cwd, session_id);
        assert_eq!(old_path, new_path, "both cwd values sanitize identically");
        let fs: Arc<dyn FileSystem> =
            Arc::new(platform_posix::fs::PosixFileSystem::new(tmp.clone()));
        let writer = JsonlWriter::new(old_path.clone(), fs);
        writer
            .append_payload_for_test(&format!(
                "{}\n",
                serde_json::json!({
                "type": "user",
                "sessionId": session_id,
                "cwd": old_cwd,
                })
            ))
            .await;

        writer
            .retarget_with_relocation(new_path.clone(), session_id, new_cwd)
            .await
            .expect("record collision relocation");
        assert_eq!(writer.active_path(), old_path);
        let raw = std::fs::read_to_string(&old_path).expect("read collision transcript");
        let routed = crate::jsonl::reader::route_lines(&raw);
        assert_eq!(
            routed.relocated_cwds.get(session_id).map(String::as_str),
            Some(new_cwd),
            "the marker must disambiguate cwd values sharing one sanitized dir"
        );
        assert_eq!(raw.lines().count(), 2);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn retarget_with_relocation_rejects_directory_source_before_quarantining_target() {
        let tmp = std::env::temp_dir().join(format!(
            "lingxi-writer-relocation-{}-{}",
            std::process::id(),
            "directory-source"
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("create temp dir");
        let old_path = tmp.join("old.jsonl");
        let new_path = tmp.join("new.jsonl");
        std::fs::create_dir_all(&old_path).expect("directory source");
        std::fs::write(&new_path, "stale destination\n").expect("occupied destination");
        let fs: Arc<dyn FileSystem> =
            Arc::new(platform_posix::fs::PosixFileSystem::new(tmp.clone()));
        let writer = JsonlWriter::new(old_path.clone(), fs);

        let error = writer
            .retarget_with_relocation(
                new_path.clone(),
                "11111111-2222-3333-4444-555555555555",
                "/tmp/new",
            )
            .await
            .expect_err("a directory is not a transcript source");
        assert!(error.to_string().contains("not a regular file"));
        assert_eq!(writer.active_path(), old_path);
        assert!(std::fs::symlink_metadata(&old_path)
            .expect("source remains")
            .file_type()
            .is_dir());
        assert_eq!(
            std::fs::read_to_string(&new_path).expect("target remains"),
            "stale destination\n",
            "source validation must happen before destination quarantine"
        );
        assert!(
            std::fs::read_dir(&tmp)
                .expect("list temp dir")
                .filter_map(Result::ok)
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("new.jsonl.superseded")),
            "a rejected source must not leave a quarantined target"
        );
        assert!(writer
            .session_metadata_state("11111111-2222-3333-4444-555555555555", &old_path)
            .await
            .lock()
            .await
            .relocated_cwd
            .is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn retarget_with_relocation_rejects_symlink_source_before_quarantining_target() {
        use std::os::unix::fs::symlink;

        let tmp = std::env::temp_dir().join(format!(
            "lingxi-writer-relocation-{}-{}",
            std::process::id(),
            "symlink-source"
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("create temp dir");
        let real_source = tmp.join("real.jsonl");
        let old_path = tmp.join("old.jsonl");
        let new_path = tmp.join("new.jsonl");
        std::fs::write(&real_source, "real transcript\n").expect("real source");
        symlink(&real_source, &old_path).expect("symlink source");
        std::fs::write(&new_path, "stale destination\n").expect("occupied destination");
        let fs: Arc<dyn FileSystem> =
            Arc::new(platform_posix::fs::PosixFileSystem::new(tmp.clone()));
        let writer = JsonlWriter::new(old_path.clone(), fs);

        let error = writer
            .retarget_with_relocation(
                new_path.clone(),
                "22222222-3333-4444-5555-666666666666",
                "/tmp/new",
            )
            .await
            .expect_err("a symlink is not a transcript source");
        assert!(error.to_string().contains("not a regular file"));
        assert_eq!(writer.active_path(), old_path);
        assert!(std::fs::symlink_metadata(&old_path)
            .expect("symlink remains")
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_to_string(&real_source).expect("real source remains"),
            "real transcript\n"
        );
        assert_eq!(
            std::fs::read_to_string(&new_path).expect("target remains"),
            "stale destination\n",
            "source validation must happen before destination quarantine"
        );
        assert!(writer
            .session_metadata_state("22222222-3333-4444-5555-666666666666", &old_path)
            .await
            .lock()
            .await
            .relocated_cwd
            .is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn retarget_with_relocation_rejects_symlinked_destination_project_parent() {
        use std::os::unix::fs::symlink;

        let tmp = std::env::temp_dir().join(format!(
            "lingxi-writer-relocation-{}-{}",
            std::process::id(),
            "symlink-destination-parent"
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("create temp dir");
        let home = tmp.join("home");
        let victim = tmp.join("victim");
        std::fs::create_dir_all(&victim).expect("create victim dir");
        let session_id = "33333333-4444-4555-8666-777777777777";
        let old_path = crate::jsonl::path::session_path(&home, "/tmp/safe-old", session_id);
        let new_path = crate::jsonl::path::session_path(&home, "/tmp/unsafe-new", session_id);
        assert_ne!(old_path, new_path);
        let fs: Arc<dyn FileSystem> =
            Arc::new(platform_posix::fs::PosixFileSystem::new(tmp.clone()));
        let writer = JsonlWriter::new(old_path.clone(), fs);
        writer
            .append_payload_for_test("{\"type\":\"user\",\"body\":\"original\"}\n")
            .await;
        let original = std::fs::read(&old_path).expect("read original transcript");

        let new_parent = new_path.parent().expect("new project directory");
        assert!(!new_parent.exists());
        symlink(&victim, new_parent).expect("redirect destination project directory");

        let error = writer
            .retarget_with_relocation(new_path.clone(), session_id, "/tmp/unsafe-new")
            .await
            .expect_err("a symlinked destination parent must be rejected");

        assert!(error
            .to_string()
            .contains("unsafe transcript relocation parent"));
        assert_eq!(writer.active_path(), old_path);
        assert_eq!(
            std::fs::read(&old_path).expect("old transcript remains"),
            original,
            "the rejected move must preserve the original bytes"
        );
        assert!(
            victim.read_dir().expect("read victim").next().is_none(),
            "the symlink target must remain untouched"
        );
        assert!(std::fs::symlink_metadata(new_parent)
            .expect("destination symlink remains")
            .file_type()
            .is_symlink());
        assert!(writer
            .session_metadata_state(session_id, &old_path)
            .await
            .lock()
            .await
            .relocated_cwd
            .is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn retarget_with_relocation_does_not_create_marker_for_missing_source() {
        let tmp = std::env::temp_dir().join(format!(
            "lingxi-writer-relocation-{}-{}",
            std::process::id(),
            "missing-source"
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("create temp dir");
        let home = tmp.join("home");
        let session_id = "99999999-aaaa-4bbb-8ccc-dddddddddddd";
        let old_path = crate::jsonl::path::session_path(&home, "/tmp/missing-old", session_id);
        let new_path = crate::jsonl::path::session_path(&home, "/tmp/missing-new", session_id);
        assert_ne!(old_path, new_path);
        let fs: Arc<dyn FileSystem> =
            Arc::new(platform_posix::fs::PosixFileSystem::new(tmp.clone()));
        let writer = JsonlWriter::new(old_path.clone(), fs);

        writer
            .retarget_with_relocation(new_path.clone(), session_id, "/tmp/missing-new")
            .await
            .expect("missing source still retargets successfully");

        assert_eq!(writer.active_path(), new_path);
        assert!(!old_path.exists());
        assert!(
            !new_path.exists(),
            "missing source must not leave a marker-only transcript"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn retarget_with_missing_source_rejects_occupied_destination() {
        let tmp = std::env::temp_dir().join(format!(
            "lingxi-writer-relocation-{}-{}",
            std::process::id(),
            "missing-occupied"
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("create temp dir");
        let home = tmp.join("home");
        let session_id = "bbbbbbbb-cccc-4ddd-8eee-ffffffffffff";
        let old_path = crate::jsonl::path::session_path(&home, "/tmp/missing-old", session_id);
        let new_path = crate::jsonl::path::session_path(&home, "/tmp/missing-new", session_id);
        assert_ne!(old_path, new_path);
        let fs: Arc<dyn FileSystem> =
            Arc::new(platform_posix::fs::PosixFileSystem::new(tmp.clone()));
        let writer = JsonlWriter::new(old_path.clone(), fs);
        let new_parent = new_path.parent().expect("new project parent");
        std::fs::create_dir_all(new_parent).expect("new project");
        std::fs::write(&new_path, "stale destination\n").expect("occupied destination");

        let error = writer
            .retarget_with_relocation(new_path.clone(), session_id, "/tmp/missing-new")
            .await
            .expect_err("an occupied destination cannot be adopted without the source");
        assert!(
            error.to_string().contains("source missing")
                && error.to_string().contains("destination occupied")
        );
        assert_eq!(writer.active_path(), old_path);
        assert!(!old_path.exists());
        assert_eq!(
            std::fs::read_to_string(&new_path).expect("restored target"),
            "stale destination\n",
            "the stale target must be restored byte-for-byte"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn retarget_with_relocation_keeps_success_when_marker_append_fails() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = std::env::temp_dir().join(format!(
            "lingxi-writer-relocation-{}-{}",
            std::process::id(),
            "marker-failure"
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("create temp dir");
        let home = tmp.join("home");
        let session_id = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        let old_path = crate::jsonl::path::session_path(&home, "/tmp/marker-old", session_id);
        let new_path = crate::jsonl::path::session_path(&home, "/tmp/marker-new", session_id);
        let fs: Arc<dyn FileSystem> =
            Arc::new(platform_posix::fs::PosixFileSystem::new(tmp.clone()));
        let writer = JsonlWriter::new(old_path.clone(), fs);
        writer
            .append_payload_for_test("{\"type\":\"user\"}\n")
            .await;
        std::fs::set_permissions(&old_path, std::fs::Permissions::from_mode(0o400))
            .expect("make moved transcript read-only");

        writer
            .retarget_with_relocation(new_path.clone(), session_id, "/tmp/marker-new")
            .await
            .expect("marker persistence is best-effort");

        assert_eq!(writer.active_path(), new_path);
        assert!(
            !old_path.exists(),
            "the successful transcript move remains published"
        );
        let moved = std::fs::read_to_string(&new_path).expect("read moved transcript");
        assert!(moved.contains("\"user\""));
        assert!(
            !moved.contains("\"relocated\""),
            "a failed marker write must not make /cd fail or fabricate a marker"
        );
        std::fs::set_permissions(&new_path, std::fs::Permissions::from_mode(0o600))
            .expect("restore cleanup permissions");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The `/rename` write path emits a `custom-title` line whose `sessionId`
    /// is the BARE uuid passed in (the `<uuid>.jsonl` stem the loader keys
    /// `custom_titles` by) and whose `customTitle` round-trips verbatim.
    #[tokio::test]
    async fn append_custom_title_writes_parseable_line() {
        let tmp = std::env::temp_dir().join(format!(
            "lingxi-writer-title-{}-{}",
            std::process::id(),
            "abc"
        ));
        std::fs::create_dir_all(&tmp).expect("create temp dir");
        let session_id = "11111111-2222-3333-4444-555555555555";
        let session_path = tmp.join(format!("{session_id}.jsonl"));
        let fs: Arc<dyn FileSystem> =
            Arc::new(platform_posix::fs::PosixFileSystem::new(tmp.clone()));
        let writer = JsonlWriter::new(session_path.clone(), fs);

        writer
            .append_custom_title(session_id, "My Title".into())
            .await
            .expect("append custom title");

        let raw = std::fs::read_to_string(&session_path).expect("read back");
        let value: serde_json::Value =
            serde_json::from_str(raw.trim()).expect("line parses as json");
        assert_eq!(value["type"], "custom-title");
        assert_eq!(value["customTitle"], "My Title");
        assert_eq!(value["sessionId"], session_id);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn custom_title_source_survives_write_and_metadata_reappend() {
        let tmp = std::env::temp_dir().join(format!(
            "lingxi-writer-title-source-{}-{}",
            std::process::id(),
            "abc"
        ));
        std::fs::create_dir_all(&tmp).expect("create temp dir");
        let session_id = "11111111-2222-3333-4444-555555555555";
        let path = tmp.join(format!("{session_id}.jsonl"));
        let fs: Arc<dyn FileSystem> =
            Arc::new(platform_posix::fs::PosixFileSystem::new(tmp.clone()));
        let writer = JsonlWriter::new(path.clone(), fs);
        let units = "title ".encode_utf16().chain([0xd800]).collect::<Vec<_>>();
        let title = Utf16JsonProjection::root_string("title �".into(), units.clone()).unwrap();

        writer
            .append_custom_title(session_id, title)
            .await
            .expect("write exact title source");
        let first = std::fs::read_to_string(&path).expect("read source line");
        assert!(first.contains(r#""customTitle":"title \ud800""#), "{first}");

        let mut metadata = SessionMetadataState::default();
        writer
            .re_append_session_metadata(&mut metadata, session_id, false, true)
            .await
            .expect("reappend title metadata");
        assert_eq!(
            metadata
                .title
                .as_ref()
                .and_then(|title| title.string_units("")),
            Some(units)
        );
        let lines = std::fs::read_to_string(&path).expect("read reappended source");
        assert!(
            lines
                .lines()
                .last()
                .is_some_and(|line| line.contains(r#""customTitle":"title \ud800""#)),
            "{lines}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn append_mobile_empty_session_writes_versioned_catalog_anchor() {
        let tmp = std::env::temp_dir()
            .join(format!("lingxi-writer-mobile-empty-{}", std::process::id(),));
        std::fs::create_dir_all(&tmp).expect("create temp dir");
        let session_id = "11111111-2222-3333-4444-555555555555";
        let session_path = tmp.join(format!("{session_id}.jsonl"));
        let fs: Arc<dyn FileSystem> =
            Arc::new(platform_posix::fs::PosixFileSystem::new(tmp.clone()));
        let writer = JsonlWriter::new(session_path.clone(), fs);

        writer
            .append_mobile_empty_session(session_id, "新对话", "/home/lingxi/lingxi")
            .await
            .expect("append mobile empty-session anchor");

        let raw = std::fs::read_to_string(&session_path).expect("read back");
        let value: serde_json::Value =
            serde_json::from_str(raw.trim()).expect("line parses as json");
        assert_eq!(value["type"], "custom-title");
        assert_eq!(value["customTitle"], "新对话");
        assert_eq!(value["sessionId"], session_id);
        assert_eq!(value["mobileEmptySession"], 1);
        assert_eq!(value["cwd"], "/home/lingxi/lingxi");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn append_permission_mode_round_trips_through_transcript_metadata() {
        let tmp = std::env::temp_dir().join(format!(
            "lingxi-writer-permission-mode-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&tmp).expect("create temp dir");
        let session_id = "11111111-2222-3333-4444-555555555555";
        let session_path = tmp.join(format!("{session_id}.jsonl"));
        let fs: Arc<dyn FileSystem> =
            Arc::new(platform_posix::fs::PosixFileSystem::new(tmp.clone()));
        let writer = JsonlWriter::new(session_path.clone(), fs);

        writer
            .append_permission_mode("bypassPermissions")
            .await
            .expect("append permission mode");

        let raw = std::fs::read_to_string(&session_path).expect("read back");
        let routed = crate::jsonl::route_lines(&raw);
        assert_eq!(
            routed.permission_modes.get(session_id).map(String::as_str),
            Some("bypassPermissions")
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn append_session_mode_round_trips_through_transcript_metadata() {
        let tmp =
            std::env::temp_dir().join(format!("lingxi-writer-session-mode-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).expect("create temp dir");
        let session_id = "11111111-2222-3333-4444-555555555555";
        let session_path = tmp.join(format!("{session_id}.jsonl"));
        let fs: Arc<dyn FileSystem> =
            Arc::new(platform_posix::fs::PosixFileSystem::new(tmp.clone()));
        let writer = JsonlWriter::new(session_path.clone(), fs);

        writer
            .append_session_mode("chat")
            .await
            .expect("append session mode");

        let raw = std::fs::read_to_string(&session_path).expect("read back");
        let routed = crate::jsonl::route_lines(&raw);
        assert_eq!(
            routed.session_modes.get(session_id).map(String::as_str),
            Some("chat")
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// (P2-02 cc2.1.207) The `--agent` persist path emits an `agent-setting` line
    /// whose `sessionId` is the BARE uuid (the `<uuid>.jsonl` stem the loader keys
    /// `agent_settings` by) and whose `agentSetting` is the applied `agentType`
    /// verbatim — the record `route_lines` reads back into `agent_settings` and
    /// `rVe` re-adopts on resume. Byte-shape matches claude's persist
    /// `{type:"agent-setting",agentSetting,sessionId}`.
    #[tokio::test]
    async fn append_agent_setting_writes_parseable_line() {
        let tmp = std::env::temp_dir().join(format!(
            "lingxi-writer-agent-{}-{}",
            std::process::id(),
            "xyz"
        ));
        std::fs::create_dir_all(&tmp).expect("create temp dir");
        let session_id = "22222222-3333-4444-5555-666666666666";
        let session_path = tmp.join(format!("{session_id}.jsonl"));
        let fs: Arc<dyn FileSystem> =
            Arc::new(platform_posix::fs::PosixFileSystem::new(tmp.clone()));
        let writer = JsonlWriter::new(session_path.clone(), fs);

        writer
            .append_agent_setting(session_id, "reviewer")
            .await
            .expect("append agent setting");

        let raw = std::fs::read_to_string(&session_path).expect("read back");
        let value: serde_json::Value =
            serde_json::from_str(raw.trim()).expect("line parses as json");
        assert_eq!(value["type"], "agent-setting");
        assert_eq!(value["agentSetting"], "reviewer");
        assert_eq!(value["sessionId"], session_id);

        // The loader routes it back into the `agent_settings` side-map keyed by
        // `sessionId` (the resume read side `rVe` consumes).
        let loaded = crate::jsonl::reader::route_lines(&raw);
        assert_eq!(
            loaded
                .agent_settings
                .get(session_id)
                .and_then(serde_json::Value::as_str),
            Some("reviewer"),
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn agent_snapshot_round_trips_with_integrity_check() {
        let tmp = std::env::temp_dir().join(format!(
            "lingxi-writer-agent-snapshot-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&tmp).expect("create temp dir");
        let session_id = "22222222-3333-4444-5555-777777777777";
        let path = tmp.join(format!("{session_id}.jsonl"));
        let fs: Arc<dyn FileSystem> =
            Arc::new(platform_posix::fs::PosixFileSystem::new(tmp.clone()));
        let writer = JsonlWriter::new(path.clone(), fs.clone());
        let definition = serde_json::json!({
            "agent_type": "reviewer",
            "system_prompt": "frozen prompt",
            "tools": {"Explicit": ["Read"]}
        });
        writer
            .append_agent_setting_snapshot(session_id, "reviewer", &definition)
            .await
            .expect("append snapshot");

        let restored = crate::jsonl::loader::read_agent_snapshot(&path, fs, session_id)
            .await
            .expect("snapshot restores");
        assert_eq!(restored, definition);
        let raw = std::fs::read_to_string(&path).unwrap();
        let routed = crate::jsonl::reader::route_lines(&raw);
        assert!(routed.agent_snapshots.contains_key(session_id));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn metadata_backstop_keeps_resumed_source_metadata_without_previous_session_leaks() {
        let root = tempfile::tempdir().unwrap();
        let id_a = SessionId::new();
        let id_b = SessionId::new();
        let path_a =
            crate::jsonl::session_path(root.path(), "/workspace/a", &id_a.as_uuid().to_string());
        let moved_a = crate::jsonl::session_path(
            root.path(),
            "/workspace/a-moved",
            &id_a.as_uuid().to_string(),
        );
        let path_b =
            crate::jsonl::session_path(root.path(), "/workspace/b", &id_b.as_uuid().to_string());
        let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(
            root.path().to_path_buf(),
        ));
        let writer = JsonlWriter::new(path_a.clone(), fs);
        writer
            .append_custom_title(&id_a.as_uuid().to_string(), "title-A".into())
            .await
            .unwrap();
        writer.append_permission_mode("plan").await.unwrap();
        writer.append_session_mode("code-A").await.unwrap();
        writer
            .retarget_with_relocation(
                moved_a.clone(),
                &id_a.as_uuid().to_string(),
                "/workspace/a-moved",
            )
            .await
            .unwrap();
        let before_a = std::fs::read_to_string(&moved_a).unwrap();

        let title = Utf16JsonProjection::root_string(
            "title-B �".into(),
            "title-B ".encode_utf16().chain([0xd800]).collect(),
        )
        .unwrap();
        let title_record = Utf16JsonProjection::object(vec![
            ("type".into(), "custom-title".into()),
            ("sessionId".into(), id_b.as_uuid().to_string().into()),
            ("customTitle".into(), title.clone()),
        ])
        .unwrap();
        let source_b = format!(
            "{}\n{}\n{}\n",
            title_record.to_json_string().unwrap(),
            json!({"type":"mode", "mode":"resumed-B", "sessionId":id_b.as_uuid().to_string()}),
            json!({"type":"user", "message":{"content":"b".repeat(90_000)}})
        );
        std::fs::create_dir_all(path_b.parent().unwrap()).unwrap();
        std::fs::write(&path_b, &source_b).unwrap();
        let rows =
            JsonlWriter::capture_session_metadata_source(&source_b, &id_b.as_uuid().to_string());
        writer.install_session_metadata_source(&id_b.as_uuid().to_string(), &rows);
        writer.retarget(path_b.clone()).await;
        writer
            .append_side_record_for_test(
                &json!({"type":"user", "message":{"content":"next".repeat(10_000)}}),
            )
            .await;
        let tail_b = crate::jsonl::read_tail(&path_b);
        assert!(
            tail_b.contains(r#""customTitle":"title-B \ud800""#),
            "{tail_b}"
        );
        assert!(tail_b.contains("resumed-B"));
        assert!(!tail_b.contains("/workspace/a-moved"));
        assert!(!tail_b.contains("title-A"));
        assert!(!tail_b.contains("code-A"));
        assert!(!tail_b.contains(r#""permissionMode":"plan""#));
        assert_eq!(std::fs::read_to_string(&moved_a).unwrap(), before_a);
        let slot_b = writer
            .session_metadata_state(&id_b.as_uuid().to_string(), &path_b)
            .await;
        assert_eq!(
            slot_b
                .lock()
                .await
                .title
                .as_ref()
                .and_then(|source| source.string_units("")),
            title.string_units("")
        );

        writer.retarget(moved_a.clone()).await;
        writer
            .append_side_record_for_test(
                &json!({"type":"user", "message":{"content":"back".repeat(10_000)}}),
            )
            .await;
        let tail_a = crate::jsonl::read_tail(&moved_a);
        assert!(tail_a.contains("/workspace/a-moved"));
        assert!(tail_a.contains("title-A"));
        assert!(!tail_a.contains("title-B"));
    }

    #[tokio::test]
    async fn metadata_backstop_waiting_during_activation_stays_on_captured_session() {
        let root = tempfile::tempdir().unwrap();
        let id_a = SessionId::new();
        let id_b = SessionId::new();
        let path_a = root.path().join(format!("{}.jsonl", id_a.as_uuid()));
        let path_b = root.path().join(format!("{}.jsonl", id_b.as_uuid()));
        let authority_a = root.path().join("authority-a");
        let authority_b = root.path().join("authority-b");
        std::fs::create_dir(&authority_a).unwrap();
        std::fs::create_dir(&authority_b).unwrap();
        let lock_a = Arc::new(DurableTranscriptWriter::open(&authority_a).unwrap());
        let lock_b = Arc::new(DurableTranscriptWriter::open(&authority_b).unwrap());
        let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(
            root.path().to_path_buf(),
        ));
        let writer =
            Arc::new(JsonlWriter::new(path_a.clone(), fs).with_durable_lock(lock_a.clone()));
        writer.activate_session_target_with_durable_lock(
            id_a,
            path_a.clone(),
            root.path().to_path_buf(),
            lock_a,
        );
        writer
            .append_custom_title(&id_a.as_uuid().to_string(), "captured-A".into())
            .await
            .unwrap();
        let slot = writer
            .session_metadata_state(&id_a.as_uuid().to_string(), &path_a)
            .await;
        let guard = slot.lock().await;
        writer
            .bytes_since_metadata_re_append
            .store(METADATA_REAPPEND_BACKSTOP_BYTES, Ordering::Relaxed);
        let pending = {
            let writer = writer.clone();
            tokio::spawn(async move { writer.maybe_re_append_metadata().await })
        };
        tokio::task::yield_now().await;
        writer.activate_session_target_with_durable_lock(
            id_b,
            path_b.clone(),
            root.path().to_path_buf(),
            lock_b,
        );
        drop(guard);
        assert!(pending.await.unwrap() > 0);
        assert!(
            !path_b.exists(),
            "A's pending backstop cannot manufacture metadata in B"
        );
        assert!(std::fs::read_to_string(path_a)
            .unwrap()
            .contains("captured-A"));
    }

    #[tokio::test]
    async fn prepared_identity_import_uses_destination_authority_without_activating_it() {
        let root = tempfile::tempdir().unwrap();
        let id_a = SessionId::new();
        let id_b = SessionId::new();
        let path_a = root.path().join(format!("{}.jsonl", id_a.as_uuid()));
        let path_b = root.path().join(format!("{}.jsonl", id_b.as_uuid()));
        let authority_a = root.path().join("authority-a");
        let authority_b = root.path().join("authority-b");
        std::fs::create_dir(&authority_a).unwrap();
        std::fs::create_dir(&authority_b).unwrap();
        let lock_a = Arc::new(DurableTranscriptWriter::open(&authority_a).unwrap());
        let lock_b = Arc::new(DurableTranscriptWriter::open(&authority_b).unwrap());
        let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(
            root.path().to_path_buf(),
        ));
        let writer = JsonlWriter::new(path_a.clone(), fs).with_durable_lock(lock_a.clone());
        writer.activate_session_target_with_durable_lock(
            id_a,
            path_a.clone(),
            root.path().to_path_buf(),
            lock_a,
        );
        let mut native = identity_row("native-source-row", "selected native history");
        native.session_id = id_b.as_uuid().to_string();
        let source = format!("{}\n", serde_json::to_string(&native).unwrap());
        std::fs::write(&path_b, &source).unwrap();
        let original_authority_b = root.path().join("authority-b-original");
        std::fs::rename(&authority_b, &original_authority_b).unwrap();
        std::fs::create_dir(&authority_b).unwrap();
        assert!(writer
            .bootstrap_prepared_session_message_identity_snapshot(&path_b, lock_b.clone())
            .await
            .is_err());
        assert_eq!(writer.active_path(), path_a);
        assert_eq!(writer.session_target_path(id_b), None);
        std::fs::remove_dir(&authority_b).unwrap();
        std::fs::rename(original_authority_b, &authority_b).unwrap();
        let imported = writer
            .bootstrap_prepared_session_message_identity_snapshot(&path_b, lock_b.clone())
            .await
            .unwrap();
        assert_eq!(imported.by_uuid["native-source-row"], 0);
        assert_eq!(writer.active_path(), path_a);
        assert_eq!(std::fs::read_to_string(&path_b).unwrap(), source);
        writer.activate_session_target_with_durable_lock(
            id_b,
            path_b.clone(),
            root.path().to_path_buf(),
            lock_b,
        );
        let mut appended = identity_row("after-import-row", "after prepared import");
        appended.session_id = id_b.as_uuid().to_string();
        writer.append(&appended).await.unwrap();
        assert!(std::fs::read_to_string(path_b)
            .unwrap()
            .contains("after prepared import"));
        assert!(!path_a.exists());
    }

    #[tokio::test]
    async fn current_worktree_state_follows_session_switch_and_relocation() {
        let root = tempfile::tempdir().unwrap();
        let session_a = SessionId::new();
        let session_b = SessionId::new();
        let path_a = crate::jsonl::session_path(
            root.path(),
            "/workspace/a",
            &session_a.as_uuid().to_string(),
        );
        let path_b = crate::jsonl::session_path(
            root.path(),
            "/workspace/b",
            &session_b.as_uuid().to_string(),
        );
        let moved_b = crate::jsonl::session_path(
            root.path(),
            "/workspace/moved",
            &session_b.as_uuid().to_string(),
        );
        let authority_a = root.path().join("authority-a");
        let authority_b = root.path().join("authority-b");
        std::fs::create_dir(&authority_a).unwrap();
        std::fs::create_dir(&authority_b).unwrap();
        let lock_a = Arc::new(DurableTranscriptWriter::open(&authority_a).unwrap());
        let lock_b = Arc::new(DurableTranscriptWriter::open(&authority_b).unwrap());
        let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(
            root.path().to_path_buf(),
        ));
        let writer = JsonlWriter::new(path_a.clone(), fs).with_durable_lock(lock_a.clone());
        writer.activate_session_target_with_durable_lock(
            session_a,
            path_a.clone(),
            PathBuf::from("/workspace/a"),
            lock_a,
        );
        writer
            .append_worktree_state_for_current_session(Some(&json!({"worktreePath":"/wt/a"})))
            .await
            .unwrap();
        let before_a = std::fs::read_to_string(&path_a).unwrap();
        writer.activate_session_target_with_durable_lock(
            session_b,
            path_b.clone(),
            PathBuf::from("/workspace/b"),
            lock_b,
        );
        writer
            .append_worktree_state_for_current_session(Some(&json!({"worktreePath":"/wt/b"})))
            .await
            .unwrap();
        writer
            .retarget_with_relocation(
                moved_b.clone(),
                &session_b.as_uuid().to_string(),
                "/workspace/moved",
            )
            .await
            .unwrap();
        writer
            .append_worktree_state_for_current_session(None)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(path_a).unwrap(), before_a);
        assert!(!path_b.exists());
        let raw = std::fs::read_to_string(moved_b).unwrap();
        let states = crate::jsonl::reader::route_lines(&raw).worktree_states;
        assert_eq!(states.len(), 1);
        assert_eq!(
            states[&session_b.as_uuid().to_string()],
            serde_json::Value::Null
        );
        assert!(raw
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|row| row["type"] == "worktree-state")
            .all(|row| row["sessionId"] == session_b.as_uuid().to_string()));
    }

    #[tokio::test]
    async fn current_worktree_state_captures_target_after_waiting_for_writer_lock() {
        let root = tempfile::tempdir().unwrap();
        let id_a = SessionId::new();
        let id_b = SessionId::new();
        let path_a = root.path().join(format!("{}.jsonl", id_a.as_uuid()));
        let path_b = root.path().join(format!("{}.jsonl", id_b.as_uuid()));
        let authority = root.path().join("authority");
        std::fs::create_dir(&authority).unwrap();
        let lock = Arc::new(DurableTranscriptWriter::open(&authority).unwrap());
        let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(
            root.path().to_path_buf(),
        ));
        let writer = Arc::new(JsonlWriter::new(path_a.clone(), fs).with_durable_lock(lock.clone()));
        writer.activate_session_target_with_durable_lock(
            id_a,
            path_a.clone(),
            root.path().to_path_buf(),
            lock.clone(),
        );
        let guard = writer.lock.lock().await;
        let pending = {
            let writer = writer.clone();
            tokio::spawn(
                async move { writer.append_worktree_state_for_current_session(None).await },
            )
        };
        tokio::task::yield_now().await;
        writer.activate_session_target_with_durable_lock(
            id_b,
            path_b.clone(),
            root.path().to_path_buf(),
            lock,
        );
        drop(guard);
        pending.await.unwrap().unwrap();
        assert!(!path_a.exists());
        let raw = std::fs::read_to_string(path_b).unwrap();
        let states = crate::jsonl::reader::route_lines(&raw).worktree_states;
        assert_eq!(states.len(), 1);
        assert!(states.contains_key(&id_b.as_uuid().to_string()));
    }

    #[tokio::test]
    async fn current_worktree_state_rejects_replaced_durable_authority() {
        let root = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let path = root.path().join(format!("{}.jsonl", id.as_uuid()));
        let authority = root.path().join("authority");
        std::fs::create_dir(&authority).unwrap();
        let lock = Arc::new(DurableTranscriptWriter::open(&authority).unwrap());
        let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(
            root.path().to_path_buf(),
        ));
        let writer = JsonlWriter::new(path.clone(), fs).with_durable_lock(lock.clone());
        writer.activate_session_target_with_durable_lock(
            id,
            path.clone(),
            root.path().to_path_buf(),
            lock,
        );
        std::fs::rename(&authority, root.path().join("original-authority")).unwrap();
        std::fs::create_dir(&authority).unwrap();
        assert!(writer
            .append_worktree_state_for_current_session(None)
            .await
            .is_err());
        assert!(
            !path.exists(),
            "a side-record append cannot bypass pinned durable authority"
        );
    }

    /// The `EnterWorktree` persist path emits a `worktree-state` line whose
    /// `sessionId` is the BARE uuid and whose `worktreeSession` payload
    /// round-trips; a subsequent `None` (ExitWorktree) writes an explicit
    /// `null`. The loader routes both back into `worktree_states` keyed by
    /// `sessionId`, last-write-wins.
    #[tokio::test]
    async fn append_worktree_state_writes_parseable_lines() {
        let tmp =
            std::env::temp_dir().join(format!("lingxi-writer-wt-{}-{}", std::process::id(), "wt"));
        std::fs::create_dir_all(&tmp).expect("create temp dir");
        let session_id = "33333333-4444-5555-6666-777777777777";
        let session_path = tmp.join(format!("{session_id}.jsonl"));
        let fs: Arc<dyn FileSystem> =
            Arc::new(platform_posix::fs::PosixFileSystem::new(tmp.clone()));
        let writer = JsonlWriter::new(session_path.clone(), fs);

        let payload = serde_json::json!({
            "worktreePath": "/repo/.lingxi/worktrees/feat",
            "originalCwd": "/repo",
            "worktreeBranch": "worktree-feat",
            "enteredExisting": false,
        });
        writer
            .append_worktree_state(session_id, Some(&payload))
            .await
            .expect("append active worktree state");

        let raw = std::fs::read_to_string(&session_path).expect("read back");
        let value: serde_json::Value =
            serde_json::from_str(raw.trim()).expect("line parses as json");
        assert_eq!(value["type"], "worktree-state");
        assert_eq!(value["sessionId"], session_id);
        assert_eq!(
            value["worktreeSession"]["worktreePath"],
            "/repo/.lingxi/worktrees/feat"
        );

        // Clear record (ExitWorktree) → worktreeSession: null.
        writer
            .append_worktree_state(session_id, None)
            .await
            .expect("append clear worktree state");

        let raw2 = std::fs::read_to_string(&session_path).expect("read back 2");
        let loaded = crate::jsonl::reader::route_lines(&raw2);
        // Last-write-wins: the clear record (null) supersedes the active one.
        assert!(
            loaded
                .worktree_states
                .get(session_id)
                .expect("worktree state present")
                .is_null(),
            "the trailing clear record wins"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
