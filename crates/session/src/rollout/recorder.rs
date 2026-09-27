//! The rollout RECORDER — faithful port of codex's `recorder.rs`.
//!
//! A background Tokio task owns the rollout file handle and performs all
//! writes; the public [`RolloutRecorder`] is a cheap `Clone` handle that talks
//! to it over a bounded mpsc channel. Newly-created sessions defer file
//! creation until the first `persist()`/`flush()` (so empty sessions never
//! touch disk); resumed sessions open the existing file immediately. Write
//! failures drop the file handle but keep the unwritten suffix buffered so a
//! later barrier can reopen the file and retry ("recovery mode").
//!
//! Differences from codex (documented, behavior-preserving):
//! - Git info collection is omitted (LingXi has no `git-utils` dep), so the
//!   session-meta line's `git` field is always `None` — same on-disk shape.
//! - State-DB listing / pagination is out of scope (LingXi has no SQLite
//!   `state_db`); only the record/append/load surface is ported.
//!
//! Compression: the read/append paths go through the [`compression`] module so
//! a cold rollout that was compressed to `.jsonl.zst` is read transparently
//! (`load_rollout_items` via [`open_rollout_line_reader`]) and materialized
//! back to plain `.jsonl` before any append (`materialize_rollout_for_append`).

use crate::rollout::compression::{materialize_rollout_for_append, open_rollout_line_reader};
use crate::rollout::initial_history::{InitialHistory, ResumedHistory};
use crate::rollout::metadata::plain_rollout_path;
use crate::rollout::record::{
    GitInfo, RolloutItem, RolloutLine, SessionMeta, SessionMetaLine, SessionSource, ThreadId,
};
use crate::rollout::SESSIONS_SUBDIR;
use chrono::{SecondsFormat, Utc};
use serde_json::Value;
use std::fs;
use std::fs::File;
use std::io::Error as IoError;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::{self, Sender};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::{error, trace, warn};

/// Minimal view of session config the recorder needs — the LingXi analog of
/// codex's `RolloutConfigView`. Implemented by [`RolloutConfig`] for tests and
/// by host config in production.
pub trait RolloutConfigView {
    fn codex_home(&self) -> &Path;
    fn cwd(&self) -> &Path;
    fn model_provider_id(&self) -> &str;
    fn generate_memories(&self) -> bool;
}

/// Simple owned config implementing [`RolloutConfigView`].
#[derive(Debug, Clone)]
pub struct RolloutConfig {
    pub codex_home: PathBuf,
    pub cwd: PathBuf,
    pub model_provider_id: String,
    pub generate_memories: bool,
}

impl RolloutConfigView for RolloutConfig {
    fn codex_home(&self) -> &Path {
        &self.codex_home
    }
    fn cwd(&self) -> &Path {
        &self.cwd
    }
    fn model_provider_id(&self) -> &str {
        &self.model_provider_id
    }
    fn generate_memories(&self) -> bool {
        self.generate_memories
    }
}

/// Writes canonical session rollout items to JSONL.
///
/// Cheap to clone; clones share the single background writer task.
#[derive(Clone)]
pub struct RolloutRecorder {
    tx: Sender<RolloutCmd>,
    writer_task: Arc<RolloutWriterTask>,
    rollout_path: PathBuf,
}

/// Parameters for creating or resuming a recorder.
#[derive(Clone)]
pub enum RolloutRecorderParams {
    Create {
        session_id: ThreadId,
        conversation_id: ThreadId,
        forked_from_id: Option<ThreadId>,
        parent_thread_id: Option<ThreadId>,
        source: SessionSource,
        originator: String,
        /// Initial context-window identity (codex: `with_initial_window_id`).
        initial_window_id: Option<String>,
    },
    Resume {
        path: PathBuf,
    },
}

impl RolloutRecorderParams {
    /// Construct create params with sensible defaults (session_id == conv id).
    #[must_use]
    pub fn new(
        conversation_id: ThreadId,
        forked_from_id: Option<ThreadId>,
        parent_thread_id: Option<ThreadId>,
        source: SessionSource,
        originator: String,
    ) -> Self {
        Self::Create {
            session_id: conversation_id,
            conversation_id,
            forked_from_id,
            parent_thread_id,
            source,
            originator,
            initial_window_id: None,
        }
    }

    #[must_use]
    pub fn with_session_id(mut self, session_id: ThreadId) -> Self {
        if let Self::Create { session_id: id, .. } = &mut self {
            *id = session_id;
        }
        self
    }

    #[must_use]
    pub fn with_initial_window_id(mut self, initial_window_id: String) -> Self {
        if let Self::Create {
            initial_window_id: window_id,
            ..
        } = &mut self
        {
            *window_id = Some(initial_window_id);
        }
        self
    }

    #[must_use]
    pub fn resume(path: PathBuf) -> Self {
        Self::Resume { path }
    }
}

enum RolloutCmd {
    AddItems(Vec<RolloutItem>),
    Persist {
        ack: oneshot::Sender<std::io::Result<()>>,
    },
    Flush {
        ack: oneshot::Sender<std::io::Result<()>>,
    },
    Shutdown {
        ack: oneshot::Sender<std::io::Result<()>>,
    },
}

/// Observable state for the background rollout writer task.
struct RolloutWriterTask {
    handle: Mutex<Option<JoinHandle<()>>>,
    terminal_failure: Mutex<Option<Arc<IoError>>>,
}

impl RolloutWriterTask {
    fn new() -> Self {
        Self {
            handle: Mutex::new(None),
            terminal_failure: Mutex::new(None),
        }
    }

    fn set_handle(&self, handle: JoinHandle<()>) {
        let mut guard = self
            .handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some(handle);
    }

    fn mark_failed(&self, err: &IoError) {
        let mut guard = self
            .terminal_failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some(Arc::new(clone_io_error(err)));
    }

    fn terminal_failure(&self) -> Option<IoError> {
        let guard = self
            .terminal_failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.as_ref().map(|err| clone_io_error(err.as_ref()))
    }
}

fn clone_io_error(err: &IoError) -> IoError {
    IoError::new(err.kind(), err.to_string())
}

impl RolloutRecorder {
    /// Create a new recorder (deferred file creation) or open a resumed one.
    pub async fn new(
        config: &impl RolloutConfigView,
        params: RolloutRecorderParams,
    ) -> std::io::Result<Self> {
        let (file, deferred_log_file_info, rollout_path, meta) = match params {
            RolloutRecorderParams::Create {
                session_id,
                conversation_id,
                forked_from_id,
                parent_thread_id,
                source,
                originator,
                initial_window_id,
            } => {
                let log_file_info = precompute_log_file_info(config, conversation_id)?;
                let path = log_file_info.path.clone();
                let thread_id = log_file_info.conversation_id;
                let started_at = log_file_info.timestamp;

                let timestamp = started_at.to_rfc3339_opts(SecondsFormat::Millis, true);

                let context_window = initial_window_id
                    .map(|window_id| serde_json::json!({ "window_id": window_id }));

                let session_meta = SessionMeta {
                    session_id,
                    id: thread_id,
                    forked_from_id,
                    parent_thread_id,
                    timestamp,
                    cwd: config.cwd().to_path_buf(),
                    originator,
                    cli_version: env!("CARGO_PKG_VERSION").to_string(),
                    agent_nickname: None,
                    agent_role: None,
                    agent_path: None,
                    model_provider: Some(config.model_provider_id().to_string()),
                    memory_mode: (!config.generate_memories()).then(|| "disabled".to_string()),
                    context_window,
                    source,
                    extra: serde_json::Map::new(),
                };

                (None, Some(log_file_info), path, Some(session_meta))
            }
            RolloutRecorderParams::Resume { path } => {
                let path = materialize_rollout_for_append(path.as_path()).await?;
                (
                    Some(
                        tokio::fs::OpenOptions::new()
                            .append(true)
                            .open(&path)
                            .await?,
                    ),
                    None,
                    path,
                    None,
                )
            }
        };

        let cwd = config.cwd().to_path_buf();

        // Bounded channel: a full buffer yields the send future rather than
        // blocking the caller's thread.
        let (tx, rx) = mpsc::channel::<RolloutCmd>(256);
        let writer_task = Arc::new(RolloutWriterTask::new());
        let writer_task_for_spawn = Arc::clone(&writer_task);
        let rollout_path_for_spawn = rollout_path.clone();
        let handle = tokio::task::spawn(async move {
            let result = rollout_writer(
                file,
                deferred_log_file_info,
                rx,
                meta,
                cwd,
                rollout_path_for_spawn.clone(),
            )
            .await;
            if let Err(err) = result {
                error!(
                    "rollout writer task failed for {}: {err}; error_kind={:?}; raw_os_error={:?}",
                    rollout_path_for_spawn.display(),
                    err.kind(),
                    err.raw_os_error()
                );
                writer_task_for_spawn.mark_failed(&err);
            }
        });
        writer_task.set_handle(handle);

        Ok(Self {
            tx,
            writer_task,
            rollout_path,
        })
    }

    #[must_use]
    pub fn rollout_path(&self) -> &Path {
        self.rollout_path.as_path()
    }

    /// Queue canonical items for the background writer.
    pub async fn record_canonical_items(&self, items: &[RolloutItem]) -> std::io::Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        self.tx
            .send(RolloutCmd::AddItems(items.to_vec()))
            .await
            .map_err(|e| {
                self.writer_task.terminal_failure().unwrap_or_else(|| {
                    IoError::other(format!("failed to queue rollout items: {e}"))
                })
            })
    }

    /// Materialize the file and persist all buffered items (idempotent).
    pub async fn persist(&self) -> std::io::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(RolloutCmd::Persist { ack: tx })
            .await
            .map_err(|e| {
                self.writer_task.terminal_failure().unwrap_or_else(|| {
                    IoError::other(format!("failed to queue rollout persist: {e}"))
                })
            })?;
        rx.await.map_err(|e| {
            self.writer_task.terminal_failure().unwrap_or_else(|| {
                IoError::other(format!("failed waiting for rollout persist: {e}"))
            })
        })?
    }

    /// Flush queued writes and wait for them to commit.
    pub async fn flush(&self) -> std::io::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(RolloutCmd::Flush { ack: tx })
            .await
            .map_err(|e| {
                self.writer_task.terminal_failure().unwrap_or_else(|| {
                    IoError::other(format!("failed to queue rollout flush: {e}"))
                })
            })?;
        rx.await.map_err(|e| {
            self.writer_task
                .terminal_failure()
                .unwrap_or_else(|| IoError::other(format!("failed waiting for rollout flush: {e}")))
        })?
    }

    /// Load all rollout items from `path`, returning `(items, thread_id,
    /// parse_errors)`. Parse-tolerant: malformed lines are counted, not fatal;
    /// legacy ghost-snapshot lines are dropped (matches codex).
    pub async fn load_rollout_items(
        path: &Path,
    ) -> std::io::Result<(Vec<RolloutItem>, Option<ThreadId>, usize)> {
        trace!("Resuming rollout from {path:?}");
        let mut items: Vec<RolloutItem> = Vec::new();
        let mut thread_id: Option<ThreadId> = None;
        let mut parse_errors = 0usize;
        // Read through the compression-aware line reader so a rollout that was
        // compressed to `.jsonl.zst` is decoded transparently (matches codex).
        let mut reader = open_rollout_line_reader(path).await?;
        let mut saw_non_empty_line = false;
        while let Some(line) = reader.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            saw_non_empty_line = true;
            let mut v: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    warn!("failed to parse line as JSON: {line:?}, error: {e}");
                    parse_errors = parse_errors.saturating_add(1);
                    continue;
                }
            };
            if strip_legacy_ghost_snapshot_rollout_line(&mut v) {
                trace!("skipping legacy ghost_snapshot rollout line");
                continue;
            }

            match serde_json::from_value::<RolloutLine>(v.clone()) {
                Ok(rollout_line) => {
                    let item = rollout_line.item;
                    if thread_id.is_none() {
                        if let RolloutItem::SessionMeta(session_meta_line) = &item {
                            thread_id = Some(session_meta_line.meta.id);
                        }
                    }
                    items.push(item);
                }
                Err(e) => {
                    trace!("failed to parse rollout line: {e}");
                    parse_errors = parse_errors.saturating_add(1);
                }
            }
        }
        if !saw_non_empty_line {
            return Err(IoError::other("empty session file"));
        }

        tracing::debug!(
            "Resumed rollout with {} items, thread ID: {:?}, parse errors: {}",
            items.len(),
            thread_id,
            parse_errors,
        );
        Ok((items, thread_id, parse_errors))
    }

    /// Reconstruct a resumable [`InitialHistory`] from a persisted rollout.
    ///
    /// Faithful port of codex's `RolloutRecorder::get_rollout_history`: loads
    /// the rollout items, takes the first `SessionMeta`'s thread id as the
    /// conversation id, and returns [`InitialHistory::Resumed`]. An empty (but
    /// non-blank) rollout collapses to [`InitialHistory::New`]; a file with no
    /// `SessionMeta` line errors, since there is no canonical thread id.
    pub async fn get_rollout_history(path: &Path) -> std::io::Result<InitialHistory> {
        let (items, thread_id, _parse_errors) = Self::load_rollout_items(path).await?;
        let conversation_id = thread_id
            .ok_or_else(|| IoError::other("failed to parse thread ID from rollout file"))?;

        if items.is_empty() {
            return Ok(InitialHistory::New);
        }

        Ok(InitialHistory::Resumed(ResumedHistory {
            conversation_id,
            history: Arc::new(items),
            rollout_path: Some(plain_rollout_path(path)),
        }))
    }

    /// Drain pending items, then stop the writer task. If draining fails the
    /// writer stays alive so callers can retry flush/shutdown.
    pub async fn shutdown(&self) -> std::io::Result<()> {
        let (tx_done, rx_done) = oneshot::channel();
        match self.tx.send(RolloutCmd::Shutdown { ack: tx_done }).await {
            Ok(_) => rx_done.await.map_err(|e| {
                self.writer_task.terminal_failure().unwrap_or_else(|| {
                    IoError::other(format!("failed waiting for rollout shutdown: {e}"))
                })
            })??,
            Err(e) => {
                if let Some(err) = self.writer_task.terminal_failure() {
                    warn!(
                        "failed to send rollout shutdown command because writer task failed: {err}"
                    );
                    return Err(err);
                }
                warn!("failed to send rollout shutdown command: {e}");
                return Err(IoError::other(format!(
                    "failed to send rollout shutdown command: {e}"
                )));
            }
        };
        Ok(())
    }
}

fn strip_legacy_ghost_snapshot_rollout_line(value: &mut Value) -> bool {
    match value.get("type").and_then(Value::as_str) {
        Some("response_item") => value
            .get("payload")
            .is_some_and(is_legacy_ghost_snapshot_response_item),
        Some("compacted") => {
            if let Some(replacement_history) = value
                .get_mut("payload")
                .and_then(|payload| payload.get_mut("replacement_history"))
                .and_then(Value::as_array_mut)
            {
                replacement_history.retain(|item| !is_legacy_ghost_snapshot_response_item(item));
            }
            false
        }
        _ => false,
    }
}

fn is_legacy_ghost_snapshot_response_item(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("ghost_snapshot")
}

pub(crate) struct LogFileInfo {
    path: PathBuf,
    conversation_id: ThreadId,
    timestamp: chrono::DateTime<Utc>,
}

fn precompute_log_file_info(
    config: &impl RolloutConfigView,
    conversation_id: ThreadId,
) -> std::io::Result<LogFileInfo> {
    // Resolve <codex_home>/sessions/YYYY/MM/DD. Codex uses local time; we use
    // UTC for deterministic paths (the bytes that matter are the filename and
    // the meta line, both reproduced).
    let timestamp = Utc::now();
    let mut dir = config.codex_home().to_path_buf();
    dir.push(SESSIONS_SUBDIR);
    dir.push(timestamp.format("%Y").to_string());
    dir.push(timestamp.format("%m").to_string());
    dir.push(timestamp.format("%d").to_string());

    // Filename: rollout-YYYY-MM-DDThh-mm-ss-<uuid>.jsonl
    let date_str = timestamp.format("%Y-%m-%dT%H-%M-%S").to_string();
    let filename = format!("rollout-{date_str}-{conversation_id}.jsonl");
    let path = dir.join(filename);

    Ok(LogFileInfo {
        path,
        conversation_id,
        timestamp,
    })
}

fn open_log_file(path: &Path) -> std::io::Result<File> {
    let path = plain_rollout_path(path);
    let Some(parent) = path.parent() else {
        return Err(IoError::other(format!(
            "rollout path has no parent: {}",
            path.display()
        )));
    };
    fs::create_dir_all(parent)?;
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
}

/// Mutable state owned by the background rollout writer.
///
/// Items are appended to `pending_items`; persist/flush/shutdown remove each
/// item only after it writes successfully. I/O failures drop the file handle
/// but keep the unwritten suffix so the next barrier can reopen and retry.
struct RolloutWriterState {
    writer: Option<JsonlWriter>,
    deferred_log_file_info: Option<LogFileInfo>,
    pending_items: Vec<RolloutItem>,
    meta: Option<SessionMeta>,
    cwd: PathBuf,
    rollout_path: PathBuf,
    last_logged_error: Option<String>,
}

impl RolloutWriterState {
    fn new(
        file: Option<tokio::fs::File>,
        deferred_log_file_info: Option<LogFileInfo>,
        meta: Option<SessionMeta>,
        cwd: PathBuf,
        rollout_path: PathBuf,
    ) -> Self {
        Self {
            writer: file.map(|file| JsonlWriter { file }),
            deferred_log_file_info,
            pending_items: Vec::new(),
            meta,
            cwd,
            rollout_path,
            last_logged_error: None,
        }
    }

    fn add_items(&mut self, items: Vec<RolloutItem>) {
        self.pending_items.extend(items);
    }

    async fn flush_if_materialized(&mut self) {
        if self.is_deferred() {
            return;
        }
        if let Err(err) = self.flush().await {
            self.enter_recovery_mode(&err);
        }
    }

    async fn persist(&mut self) -> std::io::Result<()> {
        self.write_pending_with_recovery("persist").await
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        if self.is_deferred() && self.pending_items.is_empty() {
            return Ok(());
        }
        self.write_pending_with_recovery("flush").await
    }

    async fn shutdown(&mut self) -> std::io::Result<()> {
        if self.is_deferred() && self.pending_items.is_empty() {
            return Ok(());
        }
        self.write_pending_with_recovery("shutdown").await
    }

    async fn write_pending_with_recovery(&mut self, operation: &str) -> std::io::Result<()> {
        match self.write_pending_once().await {
            Ok(()) => {
                self.last_logged_error = None;
                Ok(())
            }
            Err(first_err) => {
                self.enter_recovery_mode(&first_err);
                warn!("failed to {operation} rollout writer; reopening and retrying: {first_err}");
                match self.write_pending_once().await {
                    Ok(()) => {
                        self.last_logged_error = None;
                        Ok(())
                    }
                    Err(second_err) => {
                        self.enter_recovery_mode(&second_err);
                        warn!(
                            "retrying rollout writer {operation} failed; first error: \
                             {first_err}; final error: {second_err}"
                        );
                        Err(second_err)
                    }
                }
            }
        }
    }

    fn is_deferred(&self) -> bool {
        self.writer.is_none() && self.deferred_log_file_info.is_some()
    }

    fn enter_recovery_mode(&mut self, err: &IoError) {
        let message = err.to_string();
        if self.last_logged_error.as_ref() != Some(&message) {
            error!(
                "rollout writer failed for {}; buffered rollout items will be retried: {err}; \
                 error_kind={:?}; raw_os_error={:?}",
                self.rollout_path.display(),
                err.kind(),
                err.raw_os_error()
            );
        }
        self.last_logged_error = Some(message);
        self.writer = None;
    }

    async fn ensure_writer_open(&mut self) -> std::io::Result<()> {
        if self.writer.is_some() {
            return Ok(());
        }

        let path = self
            .deferred_log_file_info
            .as_ref()
            .map(|info| info.path.as_path())
            .unwrap_or(self.rollout_path.as_path());
        let file = open_log_file(path)?;
        self.writer = Some(JsonlWriter {
            file: tokio::fs::File::from_std(file),
        });
        self.deferred_log_file_info = None;
        Ok(())
    }

    async fn write_session_meta_if_needed(&mut self) -> std::io::Result<()> {
        let Some(session_meta) = self.meta.as_ref().cloned() else {
            return Ok(());
        };
        write_session_meta(self.writer.as_mut(), session_meta, &self.cwd).await?;
        self.meta = None;
        Ok(())
    }

    async fn write_pending_once(&mut self) -> std::io::Result<()> {
        self.ensure_writer_open().await?;
        self.write_session_meta_if_needed().await?;
        self.write_pending_items_once().await?;
        if let Some(writer) = self.writer.as_mut() {
            writer.file.flush().await?;
        }
        Ok(())
    }

    async fn write_pending_items_once(&mut self) -> std::io::Result<()> {
        let Some(writer) = self.writer.as_mut() else {
            return Err(IoError::other("rollout writer is not open"));
        };

        let mut written_count = 0usize;
        let mut write_result = Ok(());
        for item in &self.pending_items {
            if let Err(err) = writer.write_rollout_item(item).await {
                write_result = Err(err);
                break;
            }
            written_count += 1;
        }

        if written_count > 0 {
            self.pending_items.drain(..written_count);
        }

        write_result
    }
}

/// Test-only handle to the private writer-state machine, exposing just the
/// constructor + `add_items`/`flush` the recovery-retry test exercises.
#[cfg(test)]
pub(crate) struct RolloutWriterStateForTest(RolloutWriterState);

#[cfg(test)]
impl RolloutWriterStateForTest {
    pub(crate) fn new(
        file: Option<tokio::fs::File>,
        deferred_log_file_info: Option<LogFileInfo>,
        meta: Option<SessionMeta>,
        cwd: PathBuf,
        rollout_path: PathBuf,
    ) -> Self {
        Self(RolloutWriterState::new(
            file,
            deferred_log_file_info,
            meta,
            cwd,
            rollout_path,
        ))
    }

    pub(crate) fn add_items(&mut self, items: Vec<RolloutItem>) {
        self.0.add_items(items);
    }

    pub(crate) async fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush().await
    }
}

async fn rollout_writer(
    file: Option<tokio::fs::File>,
    deferred_log_file_info: Option<LogFileInfo>,
    mut rx: mpsc::Receiver<RolloutCmd>,
    meta: Option<SessionMeta>,
    cwd: PathBuf,
    rollout_path: PathBuf,
) -> std::io::Result<()> {
    let mut state = RolloutWriterState::new(file, deferred_log_file_info, meta, cwd, rollout_path);

    while let Some(cmd) = rx.recv().await {
        match cmd {
            RolloutCmd::AddItems(items) => {
                state.add_items(items);
                state.flush_if_materialized().await;
            }
            RolloutCmd::Persist { ack } => {
                let _ = ack.send(state.persist().await);
            }
            RolloutCmd::Flush { ack } => {
                let _ = ack.send(state.flush().await);
            }
            RolloutCmd::Shutdown { ack } => match state.shutdown().await {
                Ok(()) => {
                    let _ = ack.send(Ok(()));
                    break;
                }
                Err(err) => {
                    let _ = ack.send(Err(err));
                }
            },
        }
    }

    Ok(())
}

async fn write_session_meta(
    mut writer: Option<&mut JsonlWriter>,
    session_meta: SessionMeta,
    cwd: &Path,
) -> std::io::Result<()> {
    // Git collection is omitted in LingXi (no git-utils dep); the field shape
    // is preserved (`Option<GitInfo>`), always None here.
    let _ = cwd;
    let git_info: Option<GitInfo> = None;
    let session_meta_line = SessionMetaLine {
        meta: session_meta,
        git: git_info,
    };

    let rollout_item = RolloutItem::SessionMeta(session_meta_line);
    if let Some(writer) = writer.as_mut() {
        writer.write_rollout_item(&rollout_item).await?;
    }
    Ok(())
}

/// Append one already-filtered rollout item to an existing rollout JSONL file.
///
/// For metadata updates to unloaded threads. Live sessions should use
/// [`RolloutRecorder::record_canonical_items`] so writes stay ordered.
pub async fn append_rollout_item_to_path(
    rollout_path: &Path,
    item: &RolloutItem,
) -> std::io::Result<()> {
    let rollout_path = materialize_rollout_for_append(rollout_path).await?;
    let file = tokio::fs::OpenOptions::new()
        .append(true)
        .open(rollout_path)
        .await?;
    let mut writer = JsonlWriter { file };
    writer.write_rollout_item(item).await
}

struct JsonlWriter {
    file: tokio::fs::File,
}

#[derive(serde::Serialize)]
struct RolloutLineRef<'a> {
    timestamp: String,
    #[serde(flatten)]
    item: &'a RolloutItem,
}

impl JsonlWriter {
    async fn write_rollout_item(&mut self, rollout_item: &RolloutItem) -> std::io::Result<()> {
        let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let line = RolloutLineRef {
            timestamp,
            item: rollout_item,
        };
        self.write_line(&line).await
    }

    async fn write_line(&mut self, item: &impl serde::Serialize) -> std::io::Result<()> {
        let mut json = serde_json::to_string(item)?;
        json.push('\n');
        self.file.write_all(json.as_bytes()).await?;
        self.file.flush().await?;
        Ok(())
    }
}
