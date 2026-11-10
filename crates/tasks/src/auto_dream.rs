//! Claude Code 2.1.319 automatic memory consolidation.
//!
//! `IXs` runs at an idle turn boundary. It checks the feature/setting gates,
//! the last consolidation time, a ten-minute scan throttle and the number of
//! other sessions touched since that time, then claims `.consolidate-lock`.
//! A failed or cancelled fork gives its prior timestamp back for a later retry.

use crate::registry::TaskRegistry;
use crate::state::TaskStatus;
use crate::task_trait::{DreamPromptContext, TaskSpawnInput};
use crate::TaskType;
use async_trait::async_trait;
use lingxi_core::host::{BackgroundTaskHandle, Clock, RuntimeSpawner, SubagentSpawnRequest};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

pub const CONSOLIDATION_LOCK_NAME: &str = ".consolidate-lock";
pub const LOCK_LIVE_WINDOW_MS: f64 = 3_600_000.0;
pub const SCAN_THROTTLE_MS: f64 = 600_000.0;

// Native terminal sessions have distinct PIDs. Embedded conversations share
// one PID, so serialize their read/write/verify sequence within this process.
static LOCAL_LOCK_MUTATION: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Host facts corresponding to native `he`, `sgn` and `Tvt`.
#[derive(Debug, Clone, Default)]
pub struct AutoDreamGates {
    pub server_enabled: bool,
    pub server_available: bool,
    pub setting: Option<bool>,
    pub auto_memory_enabled: bool,
    pub is_subagent: bool,
    pub is_sdk: bool,
    pub memory_read_only: bool,
}

impl AutoDreamGates {
    #[must_use]
    pub fn enabled(&self) -> bool {
        !self.is_subagent
            && !self.is_sdk
            && self.auto_memory_enabled
            && !self.memory_read_only
            && (self.server_available || self.server_enabled)
            && self.setting.unwrap_or(self.server_enabled)
    }

    /// Native feature flags are strict booleans, not truthy JSON values.
    pub fn with_flag(mut self, flag: &serde_json::Value) -> Self {
        self.server_enabled =
            flag.get("enabled").and_then(serde_json::Value::as_bool) == Some(true);
        self.server_available =
            flag.get("available").and_then(serde_json::Value::as_bool) == Some(true);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AutoDreamConfig {
    pub min_hours: f64,
    pub min_sessions: f64,
}

impl Default for AutoDreamConfig {
    fn default() -> Self {
        Self {
            min_hours: 24.0,
            min_sessions: 5.0,
        }
    }
}

impl AutoDreamConfig {
    #[must_use]
    pub fn from_flag(flag: &serde_json::Value) -> Self {
        let valid = |key: &str, fallback| {
            flag.get(key)
                .and_then(serde_json::Value::as_f64)
                .filter(|n| n.is_finite() && *n > 0.0)
                .unwrap_or(fallback)
        };
        let defaults = Self::default();
        Self {
            min_hours: valid("minHours", defaults.min_hours),
            min_sessions: valid("minSessions", defaults.min_sessions),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AutoDreamRunContext {
    pub current_session_id: String,
    pub prompt_context: DreamPromptContext,
    pub gates: AutoDreamGates,
    pub config: AutoDreamConfig,
    /// The host's ordinary memory Write/Edit permission check. A denied rule
    /// skips the run before a lock is claimed; the fork also enforces its own
    /// memory-only tool policy.
    pub writes_permitted: bool,
    /// Fresh parent model, provider, prompt and conversation prefix. Native
    /// dreams are forks of this context, not unrelated general-purpose runs.
    pub fork_request: Option<Box<SubagentSpawnRequest>>,
    /// Exact provider-facing system blocks, tools and prior message prefix.
    pub fork_wire: Option<Arc<agent::memory_fork::MemoryForkProjection>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoDreamScheduleOutcome {
    Disabled,
    Cancelled,
    Running(String),
    TooRecent,
    Throttled,
    NotEnoughSessions(usize),
    WritesDenied,
    LockHeld,
    Scheduled(String),
}

#[derive(Debug, thiserror::Error)]
pub enum AutoDreamError {
    #[error("auto-dream storage: {0}")]
    Storage(String),
    #[error(transparent)]
    Task(#[from] crate::task_trait::TaskError),
    #[error(transparent)]
    Runtime(#[from] lingxi_core::host::RuntimeError),
}

/// A verified claim, including the timestamp to restore on cancellation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoDreamLease {
    pub prior_mtime: Option<SystemTime>,
    pub acquired_mtime: SystemTime,
    pub holder_pid: u32,
}

/// Injectable local/VFS storage. Implementations keep a lock scoped to the
/// selected memory directory and list the selected project's session files.
#[async_trait]
pub trait AutoDreamStorage: Send + Sync {
    async fn last_consolidated_at(
        &self,
        memory_dir: &Path,
    ) -> Result<Option<SystemTime>, AutoDreamError>;
    async fn sessions_touched_since(
        &self,
        transcript_dir: &Path,
        since: SystemTime,
    ) -> Result<Vec<String>, AutoDreamError>;
    async fn acquire(
        &self,
        memory_dir: &Path,
        now: SystemTime,
    ) -> Result<Option<AutoDreamLease>, AutoDreamError>;
    async fn rollback(
        &self,
        memory_dir: &Path,
        lease: &AutoDreamLease,
    ) -> Result<(), AutoDreamError>;
}

/// Real filesystem adapter. Filesystem traversal runs on blocking workers;
/// process liveness comes from the platform's existing PID implementation.
pub struct LocalAutoDreamStorage {
    pid_alive: Arc<dyn Fn(u32) -> bool + Send + Sync>,
}

impl LocalAutoDreamStorage {
    #[must_use]
    pub fn new(pid_alive: Arc<dyn Fn(u32) -> bool + Send + Sync>) -> Self {
        Self { pid_alive }
    }
}

fn storage_error(error: impl std::fmt::Display) -> AutoDreamError {
    AutoDreamError::Storage(error.to_string())
}

fn millis(time: SystemTime) -> f64 {
    // Match Node's stat.mtimeMs projection without first rounding a large
    // seconds value to f64 (which can move an exact millisecond down by one
    // when Date.getTime subsequently truncates it).
    let duration_ms = |duration: Duration| {
        duration.as_secs() as f64 * 1_000.0 + f64::from(duration.subsec_nanos()) / 1_000_000.0
    };
    match time.duration_since(UNIX_EPOCH) {
        Ok(elapsed) => duration_ms(elapsed),
        Err(error) => -duration_ms(error.duration()),
    }
}

#[async_trait]
impl AutoDreamStorage for LocalAutoDreamStorage {
    async fn last_consolidated_at(
        &self,
        memory_dir: &Path,
    ) -> Result<Option<SystemTime>, AutoDreamError> {
        let path = memory_dir.join(CONSOLIDATION_LOCK_NAME);
        tokio::task::spawn_blocking(move || match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_file() => {
                metadata.modified().map(Some).map_err(storage_error)
            }
            Ok(_) => Err(storage_error("consolidation lock is not a regular file")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            // Native sCt soft-skips failed stat calls as an uninitialized lock.
            Err(_) => Ok(None),
        })
        .await
        .map_err(storage_error)?
    }

    async fn sessions_touched_since(
        &self,
        transcript_dir: &Path,
        since: SystemTime,
    ) -> Result<Vec<String>, AutoDreamError> {
        let root = transcript_dir.to_owned();
        tokio::task::spawn_blocking(move || {
            let entries = match std::fs::read_dir(root) {
                Ok(entries) => entries,
                Err(_) => return Ok(Vec::new()),
            };
            let mut sessions = Vec::new();
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|ext| ext != "jsonl") {
                    continue;
                }
                let Some(stem) = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .filter(|stem| is_session_id(stem))
                else {
                    continue;
                };
                let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) else {
                    continue;
                };
                // Ejo projects stat.mtime through Date.getTime(), while the
                // consolidation lock uses stat.mtimeMs without truncating.
                if millis(modified).trunc() <= millis(since) {
                    continue;
                }
                // Agent transcripts are in child directories. Only UUID names
                // accepted by native e1 participate in the session count.
                sessions.push(stem.to_owned());
            }
            Ok(sessions)
        })
        .await
        .map_err(storage_error)?
    }

    async fn acquire(
        &self,
        memory_dir: &Path,
        now: SystemTime,
    ) -> Result<Option<AutoDreamLease>, AutoDreamError> {
        let root = memory_dir.to_owned();
        let pid_alive = self.pid_alive.clone();
        tokio::task::spawn_blocking(move || {
            let _mutation = LOCAL_LOCK_MUTATION
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::fs::create_dir_all(&root).map_err(storage_error)?;
            let relative = Path::new(CONSOLIDATION_LOCK_NAME);
            let path = root.join(relative);
            let prior_mtime = std::fs::symlink_metadata(&path)
                .ok()
                .and_then(|meta| meta.modified().ok());
            if let Some(prior) = prior_mtime {
                if millis(now).trunc() - millis(prior) < LOCK_LIVE_WINDOW_MS {
                    if let Ok(contents) =
                        lingxi_core::host::rooted_fs::read_to_string_limited(&root, relative, 256)
                    {
                        if parse_pid(&contents).is_some_and(|pid| pid_alive(pid)) {
                            return Ok(None);
                        }
                    }
                }
            }
            // Rooted publishing prevents a model-created link from redirecting
            // a host-owned lock write outside this project memory directory.
            let pin = lingxi_core::host::rooted_fs::root_identity(&root).map_err(storage_error)?;
            lingxi_core::host::rooted_fs::atomic_write(
                &root,
                relative,
                std::process::id().to_string().as_bytes(),
                lingxi_core::host::rooted_fs::AtomicWriteOptions {
                    overwrite: true,
                    create_parents: false,
                    dir_mode: 0o700,
                    file_mode: 0o600,
                },
            )
            .map_err(storage_error)?;
            let file = lingxi_core::host::rooted_fs::open_existing_file_for_update_pinned(
                &root, relative, &pin,
            )
            .map_err(storage_error)?;
            file.set_modified(now).map_err(storage_error)?;
            let acquired_mtime = file
                .metadata()
                .and_then(|meta| meta.modified())
                .map_err(storage_error)?;
            let owner = lingxi_core::host::rooted_fs::read_to_string_limited(&root, relative, 256)
                .map_err(storage_error)?;
            if parse_pid(&owner) != Some(std::process::id()) {
                return Ok(None);
            }
            Ok(Some(AutoDreamLease {
                prior_mtime,
                acquired_mtime,
                holder_pid: std::process::id(),
            }))
        })
        .await
        .map_err(storage_error)?
    }

    async fn rollback(
        &self,
        memory_dir: &Path,
        lease: &AutoDreamLease,
    ) -> Result<(), AutoDreamError> {
        let root = memory_dir.to_owned();
        let lease = lease.clone();
        tokio::task::spawn_blocking(move || {
            let _mutation = LOCAL_LOCK_MUTATION
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let relative = Path::new(CONSOLIDATION_LOCK_NAME);
            let pin = match lingxi_core::host::rooted_fs::root_identity(&root) {
                Ok(pin) => pin,
                Err(_) => return Ok(()),
            };
            let file = match lingxi_core::host::rooted_fs::open_existing_file_for_update_pinned(
                &root, relative, &pin,
            ) {
                Ok(file) => file,
                Err(_) => return Ok(()),
            };
            lingxi_core::host::rooted_fs::validate_open_file_single_link(&file)
                .map_err(storage_error)?;
            let modified = file
                .metadata()
                .and_then(|meta| meta.modified())
                .map_err(storage_error)?;
            let owner = lingxi_core::host::rooted_fs::read_to_string_limited(&root, relative, 256)
                .map_err(storage_error)?;
            if modified != lease.acquired_mtime || parse_pid(&owner) != Some(lease.holder_pid) {
                return Ok(());
            }
            if let Some(prior) = lease.prior_mtime.filter(|at| *at != UNIX_EPOCH) {
                file.set_len(0).map_err(storage_error)?;
                file.set_modified(prior).map_err(storage_error)?;
            } else {
                lingxi_core::host::rooted_fs::remove_file_pinned(&root, relative, &pin)
                    .map_err(storage_error)?;
            }
            Ok(())
        })
        .await
        .map_err(storage_error)?
    }
}

fn parse_pid(text: &str) -> Option<u32> {
    // Native parseInt accepts a decimal prefix (including a leading '+').
    let value = text.trim_start();
    let value = value.strip_prefix('+').unwrap_or(value);
    let digits: String = value.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn is_session_id(stem: &str) -> bool {
    stem.len() == 36
        && stem.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

#[derive(Clone)]
struct ActiveDream {
    task_id: String,
    lease: AutoDreamLease,
    writes: Arc<std::sync::Mutex<Vec<PathBuf>>>,
    monitor: Option<BackgroundTaskHandle>,
}

/// Successful tool writes awaiting the parent's ambient memory-update drain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoDreamUpdate {
    pub summary: String,
    pub paths: Vec<PathBuf>,
}

#[derive(Default)]
struct SchedulerState {
    last_scan_ms: f64,
    active: Option<ActiveDream>,
}

#[derive(Default)]
struct AdmissionState {
    generation: u64,
    running: bool,
    stopping: bool,
    handle: Option<BackgroundTaskHandle>,
    done: Option<tokio::sync::oneshot::Receiver<()>>,
}

#[derive(Default)]
struct KnownTasks {
    ids: HashSet<String>,
    order: VecDeque<String>,
}

/// A host-owned scheduler; keep the same instance across that host's sessions.
pub struct AutoDreamScheduler {
    storage: Arc<dyn AutoDreamStorage>,
    registry: Arc<TaskRegistry>,
    clock: Arc<dyn Clock>,
    runtime: Arc<dyn RuntimeSpawner>,
    states: Mutex<HashMap<PathBuf, SchedulerState>>,
    updates: Mutex<Vec<AutoDreamUpdate>>,
    admission: Mutex<AdmissionState>,
    shutdown_gate: Mutex<Option<tokio::sync::watch::Receiver<bool>>>,
    known_tasks: std::sync::Mutex<KnownTasks>,
}

impl AutoDreamScheduler {
    #[must_use]
    pub fn new(
        storage: Arc<dyn AutoDreamStorage>,
        registry: Arc<TaskRegistry>,
        clock: Arc<dyn Clock>,
        runtime: Arc<dyn RuntimeSpawner>,
    ) -> Self {
        Self {
            storage,
            registry,
            clock,
            runtime,
            states: Mutex::new(HashMap::new()),
            updates: Mutex::new(Vec::new()),
            admission: Mutex::new(AdmissionState::default()),
            shutdown_gate: Mutex::new(None),
            known_tasks: std::sync::Mutex::new(KnownTasks::default()),
        }
    }

    pub async fn maybe_schedule(
        self: &Arc<Self>,
        context: AutoDreamRunContext,
    ) -> Result<AutoDreamScheduleOutcome, AutoDreamError> {
        let admission = self.admission.lock().await;
        if admission.stopping {
            return Ok(AutoDreamScheduleOutcome::Cancelled);
        }
        let generation = admission.generation;
        drop(admission);
        self.maybe_schedule_generation(context, generation).await
    }

    /// The main turn only enqueues a bounded background scan. Multiple turn
    /// boundaries during the same scan coalesce into that pending admission.
    pub async fn enqueue(
        self: &Arc<Self>,
        context: AutoDreamRunContext,
    ) -> Result<(), AutoDreamError> {
        if !context.gates.enabled() {
            return Ok(());
        }
        let mut admission = self.admission.lock().await;
        if admission.running || admission.stopping {
            return Ok(());
        }
        let generation = admission.generation;
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        admission.running = true;
        let scheduler = self.clone();
        let handle = self
            .runtime
            .spawn(
                "auto-dream-admission",
                Box::pin(async move {
                    let result = scheduler
                        .maybe_schedule_generation(context, generation)
                        .await;
                    if let Err(error) = result {
                        tracing::debug!(%error, "auto-dream admission skipped");
                    }
                    let mut admission = scheduler.admission.lock().await;
                    if admission.generation == generation {
                        admission.running = false;
                        admission.handle = None;
                        admission.done = None;
                    }
                    drop(admission);
                    let _ = done_tx.send(());
                }),
            )
            .await;
        match handle {
            Ok(handle) => {
                admission.handle = Some(handle);
                admission.done = Some(done_rx);
                Ok(())
            }
            Err(error) => {
                admission.running = false;
                Err(error.into())
            }
        }
    }

    async fn maybe_schedule_generation(
        self: &Arc<Self>,
        context: AutoDreamRunContext,
        generation: u64,
    ) -> Result<AutoDreamScheduleOutcome, AutoDreamError> {
        {
            let admission = self.admission.lock().await;
            if admission.stopping || admission.generation != generation {
                return Ok(AutoDreamScheduleOutcome::Cancelled);
            }
        }
        if !context.gates.enabled() {
            return Ok(AutoDreamScheduleOutcome::Disabled);
        }
        let memory_dir = PathBuf::from(&context.prompt_context.memory_directory);
        let mut states = self.states.lock().await;
        let state = states.entry(memory_dir.clone()).or_default();
        if let Some(active) = &state.active {
            return Ok(AutoDreamScheduleOutcome::Running(active.task_id.clone()));
        }
        let prior = self
            .storage
            .last_consolidated_at(&memory_dir)
            .await?
            .unwrap_or(UNIX_EPOCH);
        let now_ms = millis(self.clock.now()).trunc();
        if (now_ms - millis(prior)) / 3_600_000.0 < context.config.min_hours {
            return Ok(AutoDreamScheduleOutcome::TooRecent);
        }
        let scan_ms = millis(self.clock.now()).trunc();
        if scan_ms - state.last_scan_ms < SCAN_THROTTLE_MS {
            return Ok(AutoDreamScheduleOutcome::Throttled);
        }
        state.last_scan_ms = scan_ms;
        let sessions = self
            .storage
            .sessions_touched_since(
                Path::new(&context.prompt_context.transcript_directory),
                prior,
            )
            .await?
            .into_iter()
            .filter(|id| id != &context.current_session_id)
            .collect::<Vec<_>>();
        if (sessions.len() as f64) < context.config.min_sessions {
            return Ok(AutoDreamScheduleOutcome::NotEnoughSessions(sessions.len()));
        }
        if !context.writes_permitted {
            return Ok(AutoDreamScheduleOutcome::WritesDenied);
        }
        let Some(lease) = self.storage.acquire(&memory_dir, self.clock.now()).await? else {
            return Ok(AutoDreamScheduleOutcome::LockHeld);
        };
        // Do not cancel the future during a blocking lock write. A clear or
        // resume invalidates its generation; the finished claim is then given
        // back before shutdown returns. Hold the final fence through registry
        // admission so an old snapshot cannot spawn after clear.
        let admission_fence = self.admission.lock().await;
        if admission_fence.stopping || admission_fence.generation != generation {
            drop(admission_fence);
            self.storage.rollback(&memory_dir, &lease).await?;
            return Ok(AutoDreamScheduleOutcome::Cancelled);
        }
        let mut prompt_context = context.prompt_context;
        let native_context = scheduler_additional_context(&sessions);
        prompt_context.additional_context = Some(match prompt_context.additional_context {
            Some(extra) if !extra.is_empty() => format!("{native_context}\n\n{extra}"),
            _ => native_context,
        });
        // Subscribe before admission: instant completion cannot outrun the
        // terminal observer that restores a failed run's timestamp.
        let notifications = self.registry.subscribe_task_notifications();
        let writes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let task_id = match self
            .registry
            .spawn(
                TaskType::Dream,
                TaskSpawnInput::Dream {
                    prompt: String::new(),
                    context: Some(prompt_context),
                    max_iterations: None,
                    fork_request: context.fork_request,
                    fork_wire: context.fork_wire,
                    writes: Some(writes.clone()),
                },
                "dreaming".into(),
            )
            .await
        {
            Ok(id) => id,
            Err(error) => {
                self.storage.rollback(&memory_dir, &lease).await?;
                return Err(error.into());
            }
        };
        self.remember_task(&task_id);
        state.active = Some(ActiveDream {
            task_id: task_id.clone(),
            lease: lease.clone(),
            writes,
            monitor: None,
        });
        drop(admission_fence);
        drop(states);
        let scheduler = self.clone();
        let watch_task_id = task_id.clone();
        let watch_dir = memory_dir.clone();
        let monitor = self
            .runtime
            .spawn(
                "auto-dream-completion",
                Box::pin(async move {
                    scheduler
                        .watch_terminal(watch_dir, watch_task_id, notifications)
                        .await;
                }),
            )
            .await;
        match monitor {
            Err(error) => {
                let _ = self.registry.kill(&task_id).await;
                let status = self
                    .registry
                    .get(&task_id)
                    .await
                    .map(|state| state.base().status);
                let mut states = self.states.lock().await;
                let active = states.get_mut(&memory_dir).and_then(|state| {
                    if state
                        .active
                        .as_ref()
                        .is_some_and(|active| active.task_id == task_id)
                    {
                        state.active.take()
                    } else {
                        None
                    }
                });
                // A concurrent shutdown can already own this run. Only the
                // owner consumes its status and releases its registry hold.
                if let Some(active) = active {
                    self.finish_active(&memory_dir, active, status).await;
                }
                // Keep terminal ownership serialized until settlement has
                // finished: shutdown must join publication/rollback before
                // it can observe this run as absent and reset the session.
                drop(states);
                return Err(error.into());
            }
            Ok(handle) => {
                if let Some(active) = self
                    .states
                    .lock()
                    .await
                    .get_mut(&memory_dir)
                    .and_then(|state| state.active.as_mut())
                    .filter(|active| active.task_id == task_id)
                {
                    active.monitor = Some(handle);
                }
            }
        }
        Ok(AutoDreamScheduleOutcome::Scheduled(task_id))
    }

    async fn watch_terminal(
        &self,
        memory_dir: PathBuf,
        task_id: String,
        mut notifications: tokio::sync::watch::Receiver<u64>,
    ) {
        loop {
            let status = self
                .registry
                .get(&task_id)
                .await
                .map(|state| state.base().status);
            if status.is_none_or(|status| status.is_terminal()) {
                let mut states = self.states.lock().await;
                let active = states.get_mut(&memory_dir).and_then(|state| {
                    if state
                        .active
                        .as_ref()
                        .is_some_and(|active| active.task_id == task_id)
                    {
                        state.active.take()
                    } else {
                        None
                    }
                });
                if let Some(active) = active {
                    self.finish_active(&memory_dir, active, status).await;
                }
                // The ownership guard is also shutdown's finishing barrier.
                // Releasing it before this await would let clear/resume
                // return while an old watcher could still publish or roll
                // its consolidation lease back in the new session.
                drop(states);
                return;
            }
            // Watch notifications cover terminal transitions; bounded sleep
            // covers a host that suppresses ambient terminal notification.
            tokio::select! {
                result = notifications.changed() => { if result.is_err() { return; } },
                () = self.runtime.sleep(Duration::from_millis(250)) => {},
            }
        }
    }

    /// Consume the actual terminal row before allowing the ordinary
    /// notification drain to evict this pre-notified ambient task. Every path
    /// reaching here owns `active`, so shutdown and the watcher cannot both
    /// publish an update or roll the same lease back.
    async fn finish_active(
        &self,
        memory_dir: &Path,
        active: ActiveDream,
        status: Option<TaskStatus>,
    ) {
        if status == Some(TaskStatus::Completed) {
            let paths = active
                .writes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if !paths.is_empty() {
                self.updates.lock().await.push(AutoDreamUpdate {
                    summary: format!(
                        "consolidated {} memory {}",
                        paths.len(),
                        if paths.len() == 1 { "file" } else { "files" }
                    ),
                    paths,
                });
            }
        } else if let Err(error) = self.storage.rollback(memory_dir, &active.lease).await {
            tracing::warn!(%error,"auto-dream rollback failed; next trigger waits for minHours");
        }
        self.registry.release_memory_completion(&active.task_id);
    }

    /// Teardown has an independent owner: timing out or dropping this waiter
    /// leaves the same receipt available to the next shutdown attempt.
    pub async fn shutdown(self: &Arc<Self>) {
        let mut done = {
            let mut owner = self.shutdown_gate.lock().await;
            if let Some(done) = owner.as_ref().filter(|done| !*done.borrow()) {
                done.clone()
            } else {
                let (settled, done) = tokio::sync::watch::channel(false);
                *owner = Some(done.clone());
                let scheduler = self.clone();
                // This lifecycle owner must outlive the RuntimeSpawner jobs
                // it cancels and any caller deadline. Publishing its receipt
                // and spawning it contain no intervening cancellation point.
                tokio::spawn(async move {
                    scheduler.shutdown_owned().await;
                    settled.send_replace(true);
                });
                done
            }
        };
        while !*done.borrow_and_update() {
            done.changed()
                .await
                .expect("auto-dream shutdown owner ended before settlement");
        }
    }

    async fn shutdown_owned(&self) {
        let (pending, done) = {
            let mut admission = self.admission.lock().await;
            admission.stopping = true;
            admission.generation = admission.generation.wrapping_add(1);
            admission.running = false;
            (admission.handle.take(), admission.done.take())
        };
        // Generation cancellation is cooperative. Waiting for the worker's
        // acknowledgement also joins any off-thread lock acquisition before
        // rollback; dropping a spawn_blocking future would leave it running.
        if let Some(done) = done {
            let _ = done.await;
        }
        if let Some(pending) = pending {
            let _ = self.runtime.cancel(&pending).await;
        }
        let active = {
            let mut states = self.states.lock().await;
            states
                .iter_mut()
                .filter_map(|(path, state)| {
                    state.active.take().map(|active| (path.clone(), active))
                })
                .collect::<Vec<_>>()
        };
        for (path, active) in active {
            if let Some(monitor) = active.monitor.as_ref() {
                let _ = self.runtime.cancel(monitor).await;
            }
            let _ = self.registry.kill(&active.task_id).await;
            let status = self
                .registry
                .get(&active.task_id)
                .await
                .map(|state| state.base().status);
            self.finish_active(&path, active, status).await;
        }
        self.admission.lock().await.stopping = false;
    }

    /// Consume once; an empty or failed dream never produces an update.
    pub async fn drain_updates(&self) -> Vec<AutoDreamUpdate> {
        std::mem::take(&mut *self.updates.lock().await)
    }

    /// Automatic dream rows still have normal UI lifecycle events. Their
    /// results are ambient memory updates, so callers exclude them from the
    /// legacy model-facing task-notification and mtime-rescan paths.
    #[must_use]
    pub fn owns_task(&self, task_id: &str) -> bool {
        self.known_tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ids
            .contains(task_id)
    }

    fn remember_task(&self, task_id: &str) {
        const RETAINED_TASK_IDS: usize = 256;
        let mut tasks = self
            .known_tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if tasks.ids.insert(task_id.to_owned()) {
            tasks.order.push_back(task_id.to_owned());
        }
        while tasks.order.len() > RETAINED_TASK_IDS {
            if let Some(old) = tasks.order.pop_front() {
                tasks.ids.remove(&old);
            }
        }
    }
}

/// Exact native `IXs` additional-context string, including its two leading
/// newlines. The dream renderer wraps this in its Additional context section.
#[must_use]
pub fn scheduler_additional_context(sessions: &[String]) -> String {
    format!("\n\n**Tool constraints for this run:** Shell access is restricted to read-only commands (`ls`, `find`, `grep`, `cat`, `stat`, `wc`, `head`, `tail`, and similar) plus deleting `.md` files inside the memory directory (outside protected subdirectories like `.git` or `agents`; `rm` takes no flags except `-f`). Anything else that writes, redirects to a file, or modifies state will be denied. Plan your exploration with this in mind.\n\nSessions since last consolidation ({}):\n{}", sessions.len(), sessions.iter().map(|id| format!("- {id}")).collect::<Vec<_>>().join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output_manager::TaskOutputManager;
    use crate::task_trait::{Task, TaskContext, TaskError, TaskHandle};
    use lingxi_core::host::FileSystem;
    use serde_json::Value;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex as StdMutex;
    use test_harness::mocks::MockRuntimeSpawner;
    mod spool_fs {
        include!("../tests/support/auto_dream_fs.rs");
    }
    use spool_fs::InMemoryFs;

    fn fixture() -> Value {
        serde_json::from_str(include_str!(
            "../../../scripts/tests/fixtures/auto_dream_319.json"
        ))
        .unwrap()
    }

    fn time(ms: f64) -> SystemTime {
        let whole_ms = ms.trunc();
        let fractional_ns = ((ms - whole_ms) * 1_000_000.0).round() as u64;
        UNIX_EPOCH + Duration::from_millis(whole_ms as u64) + Duration::from_nanos(fractional_ns)
    }

    #[test]
    fn native_feature_gates_and_fractional_thresholds() {
        let fixture = fixture();
        for case in fixture["session_id_cases"].as_array().unwrap() {
            assert_eq!(
                is_session_id(case["input"].as_str().unwrap()),
                case["expected"].is_string(),
                "{case}"
            );
        }
        for case in fixture["gate_cases"].as_array().unwrap() {
            let g = &case["gates"];
            let gates = AutoDreamGates {
                server_enabled: false,
                server_available: false,
                setting: g["setting"].as_bool(),
                auto_memory_enabled: g["auto_memory_enabled"].as_bool().unwrap(),
                is_subagent: g["is_subagent"].as_bool().unwrap(),
                is_sdk: g["is_sdk"].as_bool().unwrap(),
                memory_read_only: g["memory_read_only"].as_bool().unwrap(),
            }
            .with_flag(&case["flag"]);
            assert_eq!(
                gates.enabled(),
                case["expected"].as_bool().unwrap(),
                "{case}"
            );
        }
        for case in fixture["threshold_cases"].as_array().unwrap() {
            let expected = &case["expected"];
            assert_eq!(
                AutoDreamConfig::from_flag(&case["flag"]),
                AutoDreamConfig {
                    min_hours: expected["min_hours"].as_f64().unwrap(),
                    min_sessions: expected["min_sessions"].as_f64().unwrap(),
                }
            );
        }
        let context = &fixture["additional_context"];
        let sessions = serde_json::from_value::<Vec<String>>(context["sessions"].clone()).unwrap();
        assert_eq!(
            scheduler_additional_context(&sessions),
            context["expected"].as_str().unwrap()
        );
    }

    #[tokio::test]
    async fn native_session_catalog_fixture() {
        let fixture = fixture();
        let case = &fixture["session_scan"];
        let root = tempfile::tempdir().unwrap();
        for file in case["input"]["files"].as_array().unwrap() {
            let Some(mtime) = file["mtime_ms"].as_f64() else {
                continue;
            };
            let path = root.path().join(file["name"].as_str().unwrap());
            std::fs::write(&path, "{}").unwrap();
            std::fs::File::open(path)
                .unwrap()
                .set_modified(time(mtime))
                .unwrap();
        }
        let store = LocalAutoDreamStorage::new(Arc::new(|_| false));
        let got = store
            .sessions_touched_since(
                root.path(),
                time(case["input"]["since_ms"].as_f64().unwrap()),
            )
            .await
            .unwrap();
        assert_eq!(
            got,
            serde_json::from_value::<Vec<String>>(case["expected"].clone()).unwrap()
        );
    }

    #[tokio::test]
    async fn embedded_conversations_with_the_same_pid_share_one_local_claim() {
        let dir = tempfile::tempdir().unwrap();
        let first = LocalAutoDreamStorage::new(Arc::new(|pid| pid == std::process::id()));
        let second = LocalAutoDreamStorage::new(Arc::new(|pid| pid == std::process::id()));
        let now = time(864_000_000.0);
        let (a, b) = tokio::join!(
            first.acquire(dir.path(), now),
            second.acquire(dir.path(), now)
        );
        assert_eq!(
            usize::from(a.unwrap().is_some()) + usize::from(b.unwrap().is_some()),
            1
        );
    }

    #[tokio::test]
    async fn local_lock_rejects_live_owner_expires_and_restores_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalAutoDreamStorage::new(Arc::new(|pid| pid == std::process::id()));
        let now = time(864_000_000.0);
        let first = store.acquire(dir.path(), now).await.unwrap().unwrap();
        let path = dir.path().join(CONSOLIDATION_LOCK_NAME);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            std::process::id().to_string()
        );
        assert!(store
            .acquire(dir.path(), now + Duration::from_secs(3_599))
            .await
            .unwrap()
            .is_none());
        let next = store
            .acquire(dir.path(), now + Duration::from_secs(3_600))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(next.prior_mtime, Some(first.acquired_mtime));
        // An older monitor cannot undo its successor's lock.
        store.rollback(dir.path(), &first).await.unwrap();
        assert_eq!(
            store.last_consolidated_at(dir.path()).await.unwrap(),
            Some(next.acquired_mtime)
        );
        store.rollback(dir.path(), &next).await.unwrap();
        assert_eq!(
            store.last_consolidated_at(dir.path()).await.unwrap(),
            Some(first.acquired_mtime)
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "");
    }

    #[tokio::test]
    async fn first_run_failure_removes_lock_and_session_scan_uses_strict_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalAutoDreamStorage::new(Arc::new(|_| false));
        let now = time(864_000_000.0);
        let lease = store.acquire(dir.path(), now).await.unwrap().unwrap();
        assert!(lease.prior_mtime.is_none());
        store.rollback(dir.path(), &lease).await.unwrap();
        assert!(!dir.path().join(CONSOLIDATION_LOCK_NAME).exists());
        for (name, mtime) in [
            ("00000000-0000-0000-0000-000000000001.jsonl", now),
            (
                "00000000-0000-0000-0000-000000000002.jsonl",
                now + Duration::from_secs(1),
            ),
            // Native Date.getTime truncates this sub-millisecond advance.
            (
                "00000000-0000-0000-0000-000000000003.jsonl",
                now + Duration::from_nanos(500_000),
            ),
            ("notes.md", now + Duration::from_secs(2)),
            ("not-a-session.jsonl", now + Duration::from_secs(2)),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, "{}").unwrap();
            std::fs::File::open(path)
                .unwrap()
                .set_modified(mtime)
                .unwrap();
        }
        std::fs::create_dir(dir.path().join("nested.jsonl")).unwrap();
        assert_eq!(
            store.sessions_touched_since(dir.path(), now).await.unwrap(),
            ["00000000-0000-0000-0000-000000000002"]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rollback_does_not_follow_replaced_lock_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalAutoDreamStorage::new(Arc::new(|_| false));
        let lease = store
            .acquire(dir.path(), time(864_000_000.0))
            .await
            .unwrap()
            .unwrap();
        let victim = dir.path().join("victim");
        std::fs::write(&victim, "preserve").unwrap();
        let lock = dir.path().join(CONSOLIDATION_LOCK_NAME);
        std::fs::remove_file(&lock).unwrap();
        std::os::unix::fs::symlink(&victim, &lock).unwrap();
        store.rollback(dir.path(), &lease).await.unwrap();
        assert_eq!(std::fs::read_to_string(victim).unwrap(), "preserve");
    }

    struct TestClock(AtomicU64);
    impl Clock for TestClock {
        fn now(&self) -> SystemTime {
            time(self.0.load(Ordering::SeqCst) as f64)
        }
    }

    struct RecordingStore {
        prior: StdMutex<Option<SystemTime>>,
        sessions: Vec<String>,
        locked: bool,
        trace: StdMutex<Vec<String>>,
    }
    #[async_trait]
    impl AutoDreamStorage for RecordingStore {
        async fn last_consolidated_at(
            &self,
            _memory: &Path,
        ) -> Result<Option<SystemTime>, AutoDreamError> {
            self.trace.lock().unwrap().push("read_lock".into());
            Ok(*self.prior.lock().unwrap())
        }
        async fn sessions_touched_since(
            &self,
            _transcripts: &Path,
            _since: SystemTime,
        ) -> Result<Vec<String>, AutoDreamError> {
            self.trace.lock().unwrap().push("scan_sessions".into());
            Ok(self.sessions.clone())
        }
        async fn acquire(
            &self,
            _memory: &Path,
            now: SystemTime,
        ) -> Result<Option<AutoDreamLease>, AutoDreamError> {
            self.trace.lock().unwrap().push("acquire_lock".into());
            if self.locked {
                return Ok(None);
            }
            let prior = self.prior.lock().unwrap().replace(now);
            Ok(Some(AutoDreamLease {
                prior_mtime: prior,
                acquired_mtime: now,
                holder_pid: 42,
            }))
        }
        async fn rollback(
            &self,
            _memory: &Path,
            lease: &AutoDreamLease,
        ) -> Result<(), AutoDreamError> {
            self.trace.lock().unwrap().push("rollback".into());
            *self.prior.lock().unwrap() = lease.prior_mtime;
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingTask {
        seq: AtomicU64,
        writes: StdMutex<Option<Arc<StdMutex<Vec<PathBuf>>>>>,
    }
    #[async_trait]
    impl Task for RecordingTask {
        fn name(&self) -> &str {
            "recording_dream"
        }
        fn task_type(&self) -> TaskType {
            TaskType::Dream
        }
        async fn spawn(
            &self,
            input: TaskSpawnInput,
            _ctx: TaskContext,
        ) -> Result<TaskHandle, TaskError> {
            let TaskSpawnInput::Dream { writes, .. } = input else {
                return Err(TaskError::Unsupported);
            };
            *self.writes.lock().unwrap() = writes;
            Ok(TaskHandle::new(
                format!("d{:08}", self.seq.fetch_add(1, Ordering::SeqCst)),
                None,
            ))
        }
        async fn kill(&self, _id: &str, _ctx: TaskContext) -> Result<(), TaskError> {
            Ok(())
        }
    }

    fn setup(
        input: &Value,
    ) -> (
        Arc<AutoDreamScheduler>,
        Arc<RecordingStore>,
        Arc<TaskRegistry>,
        Arc<RecordingTask>,
        Arc<TestClock>,
        AutoDreamRunContext,
    ) {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let runtime = Arc::new(MockRuntimeSpawner::default());
        let mut registry = TaskRegistry::new(
            runtime.clone(),
            fs.clone(),
            Arc::new(TaskOutputManager::new(PathBuf::from("/task-output"), fs)),
        );
        let handler = Arc::new(RecordingTask::default());
        registry.register_handler(TaskType::Dream, handler.clone());
        let registry = Arc::new(registry);
        let store = Arc::new(RecordingStore {
            prior: StdMutex::new(Some(time(input["prior_ms"].as_f64().unwrap()))),
            sessions: serde_json::from_value(input["sessions"].clone()).unwrap(),
            locked: input["lock_held"] == true,
            trace: StdMutex::new(Vec::new()),
        });
        let clock = Arc::new(TestClock(AtomicU64::new(input["now_ms"].as_u64().unwrap())));
        let scheduler = Arc::new(AutoDreamScheduler::new(
            store.clone(),
            registry.clone(),
            clock.clone(),
            runtime,
        ));
        let flag = input
            .get("flag")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({"enabled":true}));
        let context = AutoDreamRunContext {
            current_session_id: "current".into(),
            prompt_context: DreamPromptContext {
                memory_directory: "/memory".into(),
                transcript_directory: "/transcripts".into(),
                ..Default::default()
            },
            gates: AutoDreamGates {
                auto_memory_enabled: true,
                ..Default::default()
            }
            .with_flag(&flag),
            config: AutoDreamConfig::from_flag(&flag),
            writes_permitted: input["writes_denied"] != true,
            fork_request: None,
            fork_wire: None,
        };
        (scheduler, store, registry, handler, clock, context)
    }

    async fn settle_monitor(scheduler: &AutoDreamScheduler) {
        for _ in 0..20 {
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_millis(250)).await;
            if scheduler
                .states
                .lock()
                .await
                .values()
                .all(|state| state.active.is_none())
            {
                return;
            }
        }
        panic!("auto-dream monitor did not settle");
    }

    #[tokio::test(start_paused = true)]
    async fn scheduler_decisions_match_native_runner_cases() {
        let f = fixture();
        for case in f["runner_cases"].as_array().unwrap() {
            let input = &case["input"];
            let (scheduler, store, registry, _handler, clock, context) = setup(input);
            let outcome = scheduler.maybe_schedule(context.clone()).await.unwrap();
            let native_trace: Vec<String> =
                serde_json::from_value(case["expected"].clone()).unwrap();
            if let AutoDreamScheduleOutcome::Scheduled(id) = &outcome {
                assert!(native_trace.iter().any(|event| event == "fork"), "{case}");
                registry
                    .set_status(
                        id,
                        if input["fork_fails"] == true {
                            TaskStatus::Failed
                        } else {
                            TaskStatus::Completed
                        },
                    )
                    .await
                    .unwrap();
                settle_monitor(&scheduler).await;
            } else {
                assert!(!native_trace.iter().any(|event| event == "fork"), "{case}");
            }
            let filter = |trace: Vec<String>| {
                trace
                    .into_iter()
                    .filter(|event| {
                        ["read_lock", "scan_sessions", "acquire_lock", "rollback"]
                            .contains(&event.as_str())
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(*store.trace.lock().unwrap(), filter(native_trace), "{case}");
            if let Some(delta) = input["second_call_delta_ms"].as_u64() {
                clock.0.fetch_add(delta, Ordering::SeqCst);
                store.trace.lock().unwrap().clear();
                scheduler.maybe_schedule(context).await.unwrap();
                let second: Vec<String> =
                    serde_json::from_value(case["second_expected"].clone()).unwrap();
                assert_eq!(*store.trace.lock().unwrap(), filter(second), "{case}");
            }
            scheduler.shutdown().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn successful_write_update_is_consumed_once_and_kill_returns_lock() {
        let input = serde_json::json!({"now_ms":864000000,"prior_ms":777600000,"sessions":["current","a","b","c","d","e"]});
        let (scheduler, store, registry, handler, _clock, context) = setup(&input);
        let AutoDreamScheduleOutcome::Scheduled(id) =
            scheduler.maybe_schedule(context.clone()).await.unwrap()
        else {
            panic!()
        };
        assert!(scheduler.owns_task(&id));
        handler
            .writes
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .extend([PathBuf::from("/memory/a.md"), PathBuf::from("/memory/b.md")]);
        registry
            .set_status(&id, TaskStatus::Completed)
            .await
            .unwrap();
        settle_monitor(&scheduler).await;
        assert_eq!(
            scheduler.drain_updates().await,
            [AutoDreamUpdate {
                summary: "consolidated 2 memory files".into(),
                paths: vec!["/memory/a.md".into(), "/memory/b.md".into()]
            }]
        );
        assert!(scheduler.drain_updates().await.is_empty());
        assert!(!store
            .trace
            .lock()
            .unwrap()
            .iter()
            .any(|event| event == "rollback"));
        let (scheduler, store, _registry, _handler, _clock, context) = setup(&input);
        assert!(matches!(
            scheduler.maybe_schedule(context).await.unwrap(),
            AutoDreamScheduleOutcome::Scheduled(_)
        ));
        scheduler.shutdown().await;
        assert_eq!(store.prior.lock().unwrap().unwrap(), time(777600000.0));
        assert!(scheduler.drain_updates().await.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn main_notification_drain_cannot_evict_unconsumed_dream_completion() {
        let input = serde_json::json!({"now_ms":864000000,"prior_ms":777600000,"sessions":["current","a","b","c","d","e"]});
        let (scheduler, store, registry, handler, _clock, context) = setup(&input);
        let AutoDreamScheduleOutcome::Scheduled(id) =
            scheduler.maybe_schedule(context).await.unwrap()
        else {
            panic!()
        };
        handler
            .writes
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .push("/memory/durable.md".into());
        // Make the main reminder drain win deterministically. The watcher
        // cannot take its active record until this guard is released.
        let ownership = scheduler.states.lock().await;
        registry
            .set_status(&id, TaskStatus::Completed)
            .await
            .unwrap();
        let notices = registry
            .take_pending_task_notifications(
                lingxi_core::host::task_delivery::TaskDeliveryPolicy::default(),
            )
            .await;
        assert!(
            notices.is_empty(),
            "automatic dreams never create a model task-notification"
        );
        assert_eq!(
            registry.get(&id).await.unwrap().base().status,
            TaskStatus::Completed,
            "ambient row stays available until its owner consumes the terminal status"
        );
        drop(ownership);
        settle_monitor(&scheduler).await;
        assert_eq!(
            scheduler.drain_updates().await,
            [AutoDreamUpdate {
                summary: "consolidated 1 memory file".into(),
                paths: vec!["/memory/durable.md".into()]
            }]
        );
        assert!(scheduler.drain_updates().await.is_empty());
        assert!(
            !store
                .trace
                .lock()
                .unwrap()
                .iter()
                .any(|event| event == "rollback"),
            "observed success retains the consolidation timestamp"
        );
        assert!(registry
            .take_pending_task_notifications(
                lingxi_core::host::task_delivery::TaskDeliveryPolicy::default()
            )
            .await
            .is_empty());
        scheduler.shutdown().await;
    }

    /// The publication mutex is held by the caller. Under the fixed code the
    /// watcher keeps `states` locked while waiting to publish; under the old
    /// code its active record was already gone. Either condition proves the
    /// watcher has reached terminal settlement before shutdown is polled.
    async fn wait_for_blocked_publication(scheduler: &AutoDreamScheduler) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let finishing = match scheduler.states.try_lock() {
                    Ok(states) => states.values().all(|state| state.active.is_none()),
                    Err(_) => true,
                };
                if finishing {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("terminal watcher did not reach the publication barrier");
    }

    #[tokio::test]
    async fn shutdown_joins_a_watcher_blocked_on_success_publication() {
        let input = serde_json::json!({"now_ms":864000000,"prior_ms":777600000,"sessions":["current","a","b","c","d","e"]});
        let (scheduler, store, registry, handler, _clock, context) = setup(&input);
        let AutoDreamScheduleOutcome::Scheduled(id) =
            scheduler.maybe_schedule(context).await.unwrap()
        else {
            panic!()
        };
        handler
            .writes
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .push("/memory/settled.md".into());
        // An explicit queue barrier forces completion to pause precisely
        // between claiming terminal ownership and publishing its update.
        let publication = scheduler.updates.lock().await;
        registry
            .set_status(&id, TaskStatus::Completed)
            .await
            .unwrap();
        wait_for_blocked_publication(&scheduler).await;

        let mut shutdown = Box::pin(scheduler.shutdown());
        std::future::poll_fn(
            |cx| match std::future::Future::poll(shutdown.as_mut(), cx) {
                std::task::Poll::Pending => std::task::Poll::Ready(()),
                std::task::Poll::Ready(()) => {
                    panic!("shutdown retired a watcher before its publication completed")
                }
            },
        )
        .await;
        drop(publication);
        shutdown.await;

        // This is the clear/resume drain: everything from the old session is
        // present now, and no old watcher can append after this drain.
        assert_eq!(
            scheduler.drain_updates().await,
            [AutoDreamUpdate {
                summary: "consolidated 1 memory file".into(),
                paths: vec!["/memory/settled.md".into()]
            }]
        );
        tokio::task::yield_now().await;
        assert!(scheduler.drain_updates().await.is_empty());
        assert_eq!(
            *store.prior.lock().unwrap(),
            Some(time(864000000.0)),
            "completed dreams keep the successful consolidation time"
        );
        assert!(!store
            .trace
            .lock()
            .unwrap()
            .iter()
            .any(|event| event == "rollback"));
    }

    struct BlockingRollback {
        inner: Arc<RecordingStore>,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }
    #[async_trait]
    impl AutoDreamStorage for BlockingRollback {
        async fn last_consolidated_at(
            &self,
            root: &Path,
        ) -> Result<Option<SystemTime>, AutoDreamError> {
            self.inner.last_consolidated_at(root).await
        }
        async fn sessions_touched_since(
            &self,
            root: &Path,
            since: SystemTime,
        ) -> Result<Vec<String>, AutoDreamError> {
            self.inner.sessions_touched_since(root, since).await
        }
        async fn acquire(
            &self,
            root: &Path,
            now: SystemTime,
        ) -> Result<Option<AutoDreamLease>, AutoDreamError> {
            self.inner.acquire(root, now).await
        }
        async fn rollback(
            &self,
            root: &Path,
            lease: &AutoDreamLease,
        ) -> Result<(), AutoDreamError> {
            self.entered.notify_one();
            self.release.notified().await;
            self.inner.rollback(root, lease).await
        }
    }

    #[tokio::test]
    async fn shutdown_joins_a_watcher_blocked_on_failed_run_rollback() {
        let input = serde_json::json!({"now_ms":864000000,"prior_ms":777600000,"sessions":["current","a","b","c","d","e"]});
        let (initial, store, registry, _handler, clock, context) = setup(&input);
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let scheduler = Arc::new(AutoDreamScheduler::new(
            Arc::new(BlockingRollback {
                inner: store.clone(),
                entered: entered.clone(),
                release: release.clone(),
            }),
            registry.clone(),
            clock,
            initial.runtime.clone(),
        ));
        let AutoDreamScheduleOutcome::Scheduled(id) =
            scheduler.maybe_schedule(context).await.unwrap()
        else {
            panic!()
        };
        registry.set_status(&id, TaskStatus::Failed).await.unwrap();
        entered.notified().await;
        assert_eq!(*store.prior.lock().unwrap(), Some(time(864000000.0)));

        let mut shutdown = Box::pin(scheduler.shutdown());
        std::future::poll_fn(
            |cx| match std::future::Future::poll(shutdown.as_mut(), cx) {
                std::task::Poll::Pending => std::task::Poll::Ready(()),
                std::task::Poll::Ready(()) => {
                    panic!("shutdown returned before its finishing watcher's lease rollback")
                }
            },
        )
        .await;
        release.notify_one();
        shutdown.await;
        assert_eq!(
            *store.prior.lock().unwrap(),
            Some(time(777600000.0)),
            "rollback is complete before clear can resume a new session"
        );
        assert_eq!(
            store
                .trace
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.as_str() == "rollback")
                .count(),
            1
        );
        assert!(scheduler.drain_updates().await.is_empty());
    }

    struct BlockingScan {
        inner: Arc<RecordingStore>,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[tokio::test]
    async fn cancelled_shutdown_waiter_retains_rollback_until_a_retry_joins_it() {
        let input = serde_json::json!({"now_ms":864000000,"prior_ms":777600000,"sessions":["current","a","b","c","d","e"]});
        let (initial, store, registry, _handler, clock, context) = setup(&input);
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let scheduler = Arc::new(AutoDreamScheduler::new(
            Arc::new(BlockingRollback {
                inner: store.clone(),
                entered: entered.clone(),
                release: release.clone(),
            }),
            registry,
            clock,
            initial.runtime.clone(),
        ));
        assert!(matches!(
            scheduler.maybe_schedule(context).await.unwrap(),
            AutoDreamScheduleOutcome::Scheduled(_)
        ));
        let first = tokio::spawn({
            let scheduler = scheduler.clone();
            async move {
                scheduler.shutdown().await;
            }
        });
        entered.notified().await;
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        let retry = scheduler.shutdown();
        tokio::pin!(retry);
        std::future::poll_fn(|cx| {
            assert!(
                std::future::Future::poll(retry.as_mut(), cx).is_pending(),
                "retry lost the retained settlement owner"
            );
            std::task::Poll::Ready(())
        })
        .await;
        assert_eq!(*store.prior.lock().unwrap(), Some(time(864000000.0)));
        release.notify_one();
        retry.await;
        assert_eq!(*store.prior.lock().unwrap(), Some(time(777600000.0)));
        assert_eq!(
            store
                .trace
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.as_str() == "rollback")
                .count(),
            1
        );
        assert!(scheduler.drain_updates().await.is_empty());
        assert!(!scheduler.admission.lock().await.stopping);
    }
    #[async_trait]
    impl AutoDreamStorage for BlockingScan {
        async fn last_consolidated_at(
            &self,
            root: &Path,
        ) -> Result<Option<SystemTime>, AutoDreamError> {
            self.inner.last_consolidated_at(root).await
        }
        async fn sessions_touched_since(
            &self,
            root: &Path,
            since: SystemTime,
        ) -> Result<Vec<String>, AutoDreamError> {
            self.entered.notify_one();
            self.release.notified().await;
            self.inner.sessions_touched_since(root, since).await
        }
        async fn acquire(
            &self,
            root: &Path,
            now: SystemTime,
        ) -> Result<Option<AutoDreamLease>, AutoDreamError> {
            self.inner.acquire(root, now).await
        }
        async fn rollback(
            &self,
            root: &Path,
            lease: &AutoDreamLease,
        ) -> Result<(), AutoDreamError> {
            self.inner.rollback(root, lease).await
        }
    }

    #[tokio::test]
    async fn clear_joins_pending_scan_and_fences_old_session_admission() {
        let input = serde_json::json!({"now_ms":864000000,"prior_ms":777600000,"sessions":["current","a","b","c","d","e"]});
        let (initial, store, registry, _handler, clock, context) = setup(&input);
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let scheduler = Arc::new(AutoDreamScheduler::new(
            Arc::new(BlockingScan {
                inner: store.clone(),
                entered: entered.clone(),
                release: release.clone(),
            }),
            registry.clone(),
            clock.clone(),
            initial.runtime.clone(),
        ));
        scheduler.enqueue(context.clone()).await.unwrap();
        entered.notified().await;
        // Further turn boundaries cannot pile up extra background scans.
        scheduler.enqueue(context.clone()).await.unwrap();
        let shutdown = tokio::spawn({
            let scheduler = scheduler.clone();
            async move { scheduler.shutdown().await }
        });
        loop {
            if scheduler.admission.lock().await.stopping {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(!shutdown.is_finished(), "clear joins a pending file scan");
        release.notify_one();
        shutdown.await.unwrap();
        assert!(
            registry.list().await.is_empty(),
            "old snapshot cannot register a dream after clear"
        );
        assert_eq!(store.prior.lock().unwrap().unwrap(), time(777600000.0));
        assert_eq!(
            store
                .trace
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.as_str() == "scan_sessions")
                .count(),
            1
        );
        // The same scheduler can be reused after clear/resume.
        clock.0.fetch_add(600000, Ordering::SeqCst);
        release.notify_one();
        scheduler.enqueue(context).await.unwrap();
        for _ in 0..50 {
            if !registry.list().await.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(registry.list().await.len(), 1);
        scheduler.shutdown().await;
    }
}
