//! Append-only JSONL writer — 1:1 port of
//! `claude-code/src/utils/sessionStorage.ts:2572-2584` (`appendEntryToFile`).
//!
//! Lock: serialize via `serde_json::to_string` (no whitespace, no indent),
//! terminate every line with a single `\n`, file mode `0o600`, dir mode `0o700`.

use crate::jsonl::durable_writer::{
    DurableTranscriptWriter, TranscriptAppendOutcome, TranscriptWriterError,
};
use crate::jsonl::exact_json::{
    message_utf16_overrides, native_message_bytes, parse_exact_json, to_vec_with_overrides,
    ExactJsonError, Utf16Overrides,
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
use lingxi_core::types::SessionId;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::Mutex;

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
    if msg.extra.contains_key(SESSION_KIND_KEY) {
        return None;
    }
    let mut stamped = msg.clone();
    stamped.extra.insert(
        SESSION_KIND_KEY.to_string(),
        serde_json::Value::String(kind),
    );
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
    metadata_state: Mutex<SessionMetadataState>,
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

/// `eI(e)` — the compact-boundary predicate, applied to an outgoing chain entry.
///
/// Upstream arms the transcript rewrite from inside `insertMessageChain`
/// (@296794903: `if(y&&!t&&this.sessionFile&&l===zt()) this.backstopThresholdBytes=Uyr,
/// this.requestCompact(this.sessionFile,a)`), i.e. at the moment the boundary
/// line is persisted — not from the compaction engine. Same seam here.
fn writes_compact_boundary(msg: &JsonlMessage) -> bool {
    msg.message_type == "system"
        && msg.extra.get("subtype").and_then(serde_json::Value::as_str) == Some("compact_boundary")
}

/// Read the newest custom-title record when the bounded tail no longer
/// contains one. This protects the in-process metadata state from a title
/// appended by another host (for example Electron's metadata-only rename)
/// before a large transcript write crosses the backstop in one operation.
fn latest_custom_title(path: &Path, session_id: &str) -> Option<Option<String>> {
    let contents = std::fs::read_to_string(path).ok()?;
    contents.lines().rev().find_map(|line| {
        let value: serde_json::Value = serde_json::from_str(line).ok()?;
        if value.get("type").and_then(serde_json::Value::as_str) != Some("custom-title")
            || value.get("sessionId").and_then(serde_json::Value::as_str) != Some(session_id)
        {
            return None;
        }
        Some(
            value
                .get("customTitle")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned),
        )
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
            active_path: Arc::new(std::sync::RwLock::new(path.clone())),
            path,
            fs,
            durable_lock: Arc::new(std::sync::RwLock::new(None)),
            active_durable_target: Arc::new(std::sync::RwLock::new(None)),
            session_targets: Arc::new(std::sync::RwLock::new(HashMap::new())),
            lock: Mutex::new(()),
            identity_store: Arc::new(std::sync::Mutex::new(HashMap::new())),
            bytes_since_metadata_re_append: AtomicUsize::new(0),
            metadata_state: Mutex::new(SessionMetadataState::default()),
            bytes_since_compact: AtomicU64::new(0),
            compact_backstop_bytes: AtomicU64::new(COMPACT_BACKSTOP_BYTES),
        }
    }

    /// Share the coordinator's durable transcript transaction with ordinary
    /// appends and Fusion outbox delivery. Writers without this session-state
    /// transaction append through their configured `FileSystem`.
    #[must_use]
    pub fn with_durable_lock(self, durable_lock: Arc<DurableTranscriptWriter>) -> Self {
        *self
            .durable_lock
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(durable_lock);
        self
    }

    /// Replace the active session's durable lock after a validated hot
    /// session switch. The writer remains the same shared in-process object;
    /// only the pinned session-state root changes.
    pub fn set_durable_lock(&self, durable_lock: Arc<DurableTranscriptWriter>) {
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
        let root = transcript_path
            .parent()
            .ok_or_else(|| FsError::Io("transcript has no parent directory".into()))?;
        let sidecar_lock = message_identity::lock_path(transcript_path)?;
        Ok(self.fs.flock_exclusive_rooted(root, &sidecar_lock).await?)
    }

    async fn append_identity_non_durable(
        &self,
        transcript_path: &Path,
        uuid: &str,
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
        let uuid = uuid.to_owned();
        tokio::task::spawn_blocking(move || {
            message_identity::append_row_identity_at(
                &store,
                &transcript_path,
                &root,
                &identity,
                &uuid,
            )
        })
        .await
        .map_err(|error| FsError::Io(error.to_string()))??;
        Ok(guard)
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
        tokio::task::spawn_blocking(move || {
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
        if let Some(durable_writer) = self.durable_writer_for_path(transcript_path) {
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
        tokio::task::spawn_blocking(move || {
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

    /// Atomically switch subsequent appends to another session transcript.
    ///
    /// The append mutex makes the boundary explicit: an append already in
    /// progress finishes on the previous file before this method returns.
    pub async fn retarget(&self, path: PathBuf) {
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

        let mut metadata_state = self.metadata_state.lock().await;
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
                        serde_json::from_str(&operation_payload).map_err(|error| {
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
        let mut metadata_state = self.metadata_state.lock().await;
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
            let payload = serde_json::to_value(msg)?;
            let utf16_overrides = message_utf16_overrides(msg);
            let payload_bytes = to_vec_with_overrides(&payload, &utf16_overrides)?.len() + 1;
            self.append_json_durable(payload, utf16_overrides, Some(msg.uuid.clone()))
                .await?;
            self.bytes_since_metadata_re_append
                .fetch_add(payload_bytes, Ordering::Relaxed);
            self.bytes_since_compact
                .fetch_add(payload_bytes as u64, Ordering::Relaxed);
        } else {
            let _g = self.lock.lock().await;
            let stamped = stamp_session_kind(msg);
            let line = String::from_utf8(native_message_bytes(stamped.as_ref().unwrap_or(msg))?)
                .expect("native JSON encoder emits UTF-8");
            let mut payload = String::with_capacity(line.len() + 1);
            payload.push_str(&line);
            payload.push('\n');
            let path = self.active_path();
            let _identity_guard = self.append_identity_non_durable(&path, &msg.uuid).await?;
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
        payload: serde_json::Value,
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
            Utf16Overrides::new(),
            false,
        )
        .await
        .map(|(outcome, _)| outcome)
    }

    /// Append once for a run pinned to its originating session. The target is
    /// looked up only after acquiring the ordinary writer mutex, while the
    /// durable transaction then serializes it with `/cd` relocation and other
    /// cross-process writers.
    pub async fn append_json_once_durable_for_session(
        &self,
        session_id: SessionId,
        delivery_id: &str,
        payload: serde_json::Value,
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
        payload: serde_json::Value,
    ) -> Result<(TranscriptAppendOutcome, bool), TranscriptWriterError> {
        self.append_json_once_durable_for_session_with_tip_mode(
            session_id,
            delivery_id,
            payload,
            Utf16Overrides::new(),
            false,
        )
        .await
    }

    /// Append exact JavaScript string leaves to the originating session. The
    /// private override map is encoded into native JSON strings before the
    /// same rooted append, conflict scan, and durability acknowledgement.
    pub async fn append_json_once_durable_for_session_with_tip_exact(
        &self,
        session_id: SessionId,
        delivery_id: &str,
        payload: serde_json::Value,
        utf16_overrides: Utf16Overrides,
    ) -> Result<(TranscriptAppendOutcome, bool), TranscriptWriterError> {
        self.append_json_once_durable_for_session_with_tip_mode(
            session_id,
            delivery_id,
            payload,
            utf16_overrides,
            true,
        )
        .await
    }

    async fn append_json_once_durable_for_session_with_tip_mode(
        &self,
        session_id: SessionId,
        delivery_id: &str,
        payload: serde_json::Value,
        utf16_overrides: Utf16Overrides,
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
            utf16_overrides,
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
        payload: serde_json::Value,
        utf16_overrides: Utf16Overrides,
        identity_uuid: Option<String>,
    ) -> Result<(), TranscriptWriterError> {
        let _guard = self.lock.lock().await;
        let Some(target) = self.active_durable_target() else {
            return Err(TranscriptWriterError::Fs(FsError::Io(
                "durable transcript target is not configured".into(),
            )));
        };
        self.append_json_durable_locked(
            target.path,
            target.writer,
            payload,
            utf16_overrides,
            identity_uuid,
        )
        .await
    }

    async fn append_json_durable_for_session(
        &self,
        session_id: SessionId,
        payload: serde_json::Value,
        utf16_overrides: Utf16Overrides,
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
        self.append_json_durable_locked(
            target.path,
            target.writer,
            payload,
            utf16_overrides,
            identity_uuid,
        )
        .await
    }

    async fn append_json_durable_locked(
        &self,
        active_path: PathBuf,
        durable_lock: Arc<DurableTranscriptWriter>,
        payload: serde_json::Value,
        utf16_overrides: Utf16Overrides,
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
                }
                transaction.append_raw_json_at_exact(
                    parent,
                    &identity,
                    &relative,
                    payload,
                    &utf16_overrides,
                )
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
        payload: serde_json::Value,
        utf16_overrides: Utf16Overrides,
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
                    transaction.append_json_once_at_with_tip_exact_identity(
                        parent,
                        &identity,
                        &relative,
                        &delivery_id,
                        payload,
                        &utf16_overrides,
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
            let payload = serde_json::to_value(msg)?;
            self.append_json_durable_for_session(
                session_id,
                payload,
                message_utf16_overrides(msg),
                Some(msg.uuid.clone()),
            )
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
        let _identity_guard = self.append_identity_non_durable(path, &msg.uuid).await?;
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
        self.active_path()
            .file_stem()
            .and_then(|s| s.to_str())
            .map(ToString::to_string)
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
        let Some(sid) = self.session_id_from_path() else {
            return 0;
        };
        let mut state = self.metadata_state.lock().await;
        match self
            .re_append_session_metadata(&mut state, &sid, false, true)
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
        if let Some(sid) = self.session_id_from_path() {
            let mut state = self.metadata_state.lock().await;
            if let Err(e) = self
                .re_append_session_metadata(&mut state, &sid, false, true)
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
        let _g = self.lock.lock().await;
        self.bytes_since_metadata_re_append
            .store(0, Ordering::Relaxed);
        let path = self.active_path();
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
        self.append_payload(&plan.to_jsonl()).await?;
        // The re-appended bytes are the metadata itself; they must not count
        // toward the next backstop.
        self.bytes_since_metadata_re_append
            .store(0, Ordering::Relaxed);
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

    pub async fn append_custom_title(
        &self,
        session_id: &str,
        custom_title: &str,
    ) -> Result<(), WriterError> {
        let value = serde_json::json!({
            "type": "custom-title",
            "customTitle": custom_title,
            "sessionId": session_id,
        });
        // Mirror the oracle's `currentSessionTitle`. NOTE this is belt-and-braces,
        // not the load-bearing path: `plan_re_append` ADOPTS the title back out
        // of the tail, and the backstop (32 KiB) is deliberately half the tail
        // window (64 KiB) so it fires while the record is still readable.
        // Removing this line does NOT fail `appends_alone_drive_the_metadata_backstop`
        // — verified by mutation. It matters only when state is set without a
        // corresponding record already on disk.
        self.metadata_state.lock().await.title = Some(custom_title.to_string());
        self.append_side_record(&value).await
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
    ) -> Result<(), WriterError> {
        let value = serde_json::json!({
            "type": "custom-title",
            "customTitle": title,
            "sessionId": session_id,
            "mobileEmptySession": 1,
        });
        self.append_side_record(&value).await
    }

    /// Persist the active permission mode for this transcript's session.
    ///
    /// The mode is session metadata, not a user-wide default: reopening this
    /// transcript restores its last selected mode without changing other
    /// sessions. The active writer path is the source of the bare session UUID.
    pub async fn append_permission_mode(&self, permission_mode: &str) -> Result<(), WriterError> {
        let Some(session_id) = self.session_id_from_path() else {
            return Ok(());
        };
        self.metadata_state.lock().await.permission_mode = Some(permission_mode.to_string());
        let value = serde_json::json!({
            "type": "permission-mode",
            "permissionMode": permission_mode,
            "sessionId": session_id,
        });
        self.append_side_record(&value).await
    }

    /// Persist the mobile chat/code capability profile for this transcript.
    pub async fn append_session_mode(&self, session_mode: &str) -> Result<(), WriterError> {
        let Some(session_id) = self.session_id_from_path() else {
            return Ok(());
        };
        self.metadata_state.lock().await.session_mode = Some(session_mode.to_string());
        let value = serde_json::json!({
            "type": "session-mode",
            "sessionMode": session_mode,
            "sessionId": session_id,
        });
        self.append_side_record(&value).await
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
        let value = serde_json::json!({
            "type": "worktree-state",
            "worktreeSession": worktree_session.cloned().unwrap_or(serde_json::Value::Null),
            "sessionId": session_id,
        });
        self.append_side_record(&value).await
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

    /// Physically remove a rejected attempt, as cc 2.1.263
    /// `performRemoveByUuid` does. Preserve untouched rows byte for byte;
    /// repair children because this writer persists streamed blocks eagerly.
    /// Returns the surviving chain tail for the next append.
    pub async fn remove_retry_attempt(
        &self,
        message_id: &str,
    ) -> Result<Option<String>, WriterError> {
        let _guard = self.lock.lock().await;
        let path = self.active_path();
        let source = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| FsError::Io(e.to_string()))?;
        let mut removed = std::collections::HashMap::new();
        for line in source.lines() {
            if let Ok(exact) = parse_exact_json(line) {
                let row = exact.value;
                if row["type"] == "assistant"
                    && (row["uuid"] == message_id || row["message"]["id"] == message_id)
                {
                    if let Some(id) = row["uuid"].as_str() {
                        removed
                            .insert(id.to_owned(), row["parentUuid"].as_str().map(str::to_owned));
                    }
                }
            }
        }
        let mut output = String::with_capacity(source.len());
        let mut tail = None;
        for line in source.split_inclusive('\n') {
            let Ok(mut exact) = parse_exact_json(line) else {
                output.push_str(line);
                continue;
            };
            let row = &mut exact.value;
            if row["uuid"]
                .as_str()
                .is_some_and(|id| removed.contains_key(id))
            {
                continue;
            }
            let mut parent = row["parentUuid"].as_str().map(str::to_owned);
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
                row["parentUuid"] = serde_json::to_value(parent)?;
                exact.utf16_overrides.remove("/parentUuid");
                output.push_str(
                    &String::from_utf8(to_vec_with_overrides(row, &exact.utf16_overrides)?)
                        .expect("native JSON encoder emits UTF-8"),
                );
                if line.ends_with('\n') {
                    output.push('\n');
                }
            } else {
                output.push_str(line);
            }
            if matches!(
                row["type"].as_str(),
                Some("assistant" | "user" | "system" | "attachment")
            ) {
                if let Some(id) = row["uuid"].as_str() {
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
}

#[cfg(test)]
mod tests {
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
            loaded.by_uuid["native-user"].message["content"], "updated",
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
        let mut child = parse_exact_json(first).unwrap();
        child.value["parentUuid"] = serde_json::json!("rejected");
        let child_line =
            String::from_utf8(to_vec_with_overrides(&child.value, &child.utf16_overrides).unwrap())
                .unwrap();
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
            child.value["uuid"].as_str()
        );
        let output = std::fs::read_to_string(&path).unwrap();
        assert_eq!(output.lines().count(), 2);
        let repaired = parse_exact_json(output.lines().last().unwrap()).unwrap();
        assert_eq!(repaired.value["parentUuid"], "root");
        assert_eq!(repaired.value["origin"], child.value["origin"]);
        assert_eq!(repaired.utf16_overrides, child.utf16_overrides);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn native_exact_append_cold_retry_preserves_units_uuid_and_pinned_session() {
        use crate::jsonl::exact_json::{message_utf16_overrides, parse_exact_json};
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
            .map(|line| parse_exact_json(line).unwrap())
            .collect();
        for row in &prepared {
            let delivery = format!("subagent-handback:{}", row.value["uuid"].as_str().unwrap());
            assert_eq!(
                writer
                    .append_json_once_durable_for_session_with_tip_exact(
                        session_a,
                        &delivery,
                        row.value.clone(),
                        row.utf16_overrides.clone()
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
            let delivery = format!("subagent-handback:{}", row.value["uuid"].as_str().unwrap());
            assert_eq!(
                cold.append_json_once_durable_for_session_with_tip_exact(
                    session_a,
                    &delivery,
                    row.value.clone(),
                    row.utf16_overrides.clone()
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
        let mut different_units = prepared[0].utf16_overrides.clone();
        different_units.insert("/origin/body".into(), vec![0xde00]);
        assert!(matches!(
            cold.append_json_once_durable_for_session_with_tip_exact(
                session_a,
                "subagent-handback:aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaa1",
                prepared[0].value.clone(),
                different_units
            )
            .await,
            Err(TranscriptWriterError::DeliveryConflict { .. })
        ));
        assert!(matches!(
            cold.append_json_once_durable_for_session_with_tip_exact(
                session_a,
                "subagent-handback:aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaa1",
                prepared[0].value.clone(),
                Utf16Overrides::new()
            )
            .await,
            Err(TranscriptWriterError::DeliveryConflict { .. })
        ));
        let loaded = crate::jsonl::reader::route_lines(&raw);
        assert_eq!(loaded.malformed_line_count, 0);
        assert_eq!(loaded.messages_in_order.len(), 4);
        assert_eq!(
            message_utf16_overrides(&loaded.messages_in_order[0])["/origin/body"],
            vec![0xd83d]
        );
        assert_eq!(loaded.messages_in_order[0].extra["origin"]["kind"], "peer");
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
            serde_json::from_value::<JsonlMessage>(serde_json::json!({
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
            .expect("message")
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
                .and_then(|message| message.message.get("content"))
                .and_then(serde_json::Value::as_str),
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
            serde_json::from_value::<JsonlMessage>(serde_json::json!({
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
            .unwrap()
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
                serde_json::json!({
                    "uuid": "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
                    "parentUuid": null,
                    "type": "user",
                    "sessionId": session_a.as_uuid().to_string(),
                    "message": {"role": "user", "content": "late Fusion A"}
                }),
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
            .append_custom_title("11111111-2222-3333-4444-555555555555", "t")
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
                &"p".repeat(METADATA_REAPPEND_BACKSTOP_BYTES),
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
        let (_dir, path, writer) = temp_writer("22222222-3333-4444-5555-666666666666");

        // A title recorded through the writer is remembered in its own state.
        writer
            .append_custom_title("22222222-3333-4444-5555-666666666666", "Long Session")
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

    #[tokio::test]
    async fn external_title_append_survives_a_large_writer_append() {
        let (dir, path, writer) = temp_writer("external-title");
        let sid = "11111111-2222-3333-4444-555555555555";
        writer.append_custom_title(sid, "old").await.unwrap();

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

        writer.append_custom_title(sid, "Kept Title").await.unwrap();
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
            .append_custom_title("11111111-2222-3333-4444-555555555555", "first")
            .await
            .expect("append first title");
        writer.retarget(second.clone()).await;
        writer
            .append_custom_title("66666666-7777-4888-8999-aaaaaaaaaaaa", "second")
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
        assert!(writer.metadata_state.lock().await.relocated_cwd.is_none());
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
        assert!(writer.metadata_state.lock().await.relocated_cwd.is_none());
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
        assert!(writer.metadata_state.lock().await.relocated_cwd.is_none());
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
            .append_custom_title(session_id, "My Title")
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
            .append_mobile_empty_session(session_id, "新对话")
            .await
            .expect("append mobile empty-session anchor");

        let raw = std::fs::read_to_string(&session_path).expect("read back");
        let value: serde_json::Value =
            serde_json::from_str(raw.trim()).expect("line parses as json");
        assert_eq!(value["type"], "custom-title");
        assert_eq!(value["customTitle"], "新对话");
        assert_eq!(value["sessionId"], session_id);
        assert_eq!(value["mobileEmptySession"], 1);

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
