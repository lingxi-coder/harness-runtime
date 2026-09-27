//! Cron scheduler tick loop wired to [`tasks::TaskRegistry`].
//!
//! The scheduler ticks once per second, finds jobs whose deterministic
//! jittered fire time has arrived, applies project-leader / creator ownership,
//! and delivers raw scheduled prompts through the host conversation queue.

use crate::schedule::{parse_cron, CronExpression};
use platform_api::task_registry::TaskRegistryHandle;
use platform_api::{Clock, FileSystem, FsError, RuntimeSpawner};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex as StdMutex, Weak};
use std::time::{Duration, SystemTime};
use tasks::registry::TaskRegistry;
use tasks::TaskSpawnInput;
#[cfg(test)]
use tasks::TaskType;
use tokio::sync::{Mutex, RwLock};

/// A raw cron fire routed to the owning conversation, not a new agent.
#[derive(Clone, Debug)]
pub struct SessionCronFire {
    /// Stable scheduled job ID.
    pub id: String,
    /// Original cron expression.
    pub cron: String,
    /// Unresolved prompt, including loop sentinel values.
    pub prompt: String,
    /// Teammate owner, when the session task was created by one.
    pub owner: Option<String>,
}

/// Host integration for Claude-compatible Later/meta scheduled input.
#[async_trait::async_trait]
pub trait SessionCronDelivery: Send + Sync {
    /// A busy conversation leaves due jobs pending without claiming them.
    async fn is_loading(&self) -> bool;
    /// Enqueue the raw fire into the owning conversation.
    async fn enqueue(&self, fire: SessionCronFire) -> Result<(), String>;
    /// Discard unconsumed fixed fires before switching conversation identity.
    async fn clear_queued(&self) {}
    /// The loop state of the conversation this delivery writes into.
    ///
    /// PARITY 2.1.270 `U(o)` (`src_197155721.js`): the no-op fold's blocking
    /// predicate is `subtype === "scheduled_task_fire" || subtype ===
    /// "compact_boundary"` — it matches EVERY scheduled fire, not only the
    /// loop's own. A fixed task firing into the same transcript is therefore a
    /// disturbance, and upstream refuses to fold rather than collapsing that
    /// notice out of sight. Hosts that cannot reach the state return `None` and
    /// keep the previous behaviour.
    fn loop_runtime(&self) -> Option<std::sync::Arc<crate::autonomous_loop::LoopRuntime>> {
        None
    }
}

#[cfg(test)]
struct RegistryTestDelivery(Arc<TaskRegistry>);
#[cfg(test)]
#[async_trait::async_trait]
impl SessionCronDelivery for RegistryTestDelivery {
    async fn is_loading(&self) -> bool {
        false
    }
    async fn enqueue(&self, fire: SessionCronFire) -> Result<(), String> {
        self.0
            .spawn(
                TaskType::Dream,
                TaskSpawnInput::Dream {
                    prompt: fire.prompt,
                    max_iterations: None,
                },
                format!("cron: {}", fire.id),
            )
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

/// The session-scoped portion of a cron job. These records never touch the
/// project tasks file; they live for exactly as long as the scheduler attached
/// to the session's task registry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionCronTask {
    /// Stable job identifier.
    pub id: String,
    /// Original five-field expression, retained for `CronList`.
    pub cron: String,
    /// Prompt submitted when the schedule fires.
    pub prompt: String,
    /// Creation time used for due detection and recurring expiry.
    pub created_at: SystemTime,
    /// Last successful claim, when recurring.
    pub last_fired_at: Option<SystemTime>,
    /// Whether the task repeats.
    pub recurring: bool,
    /// Calling teammate identity. `None` denotes the main thread.
    pub owner: Option<String>,
}

/// Failure to find the live scheduler belonging to a tool context.
#[derive(Debug, thiserror::Error)]
#[error("no active cron scheduler for this session")]
pub struct NoActiveCronScheduler;

static LIVE_SCHEDULERS: LazyLock<StdMutex<HashMap<usize, Weak<CronScheduler>>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

/// Stable identity for an `Arc` allocation, preserved when coercing a concrete
/// task registry to its trait-object handle.
#[must_use]
pub fn task_registry_identity<T: ?Sized>(registry: &Arc<T>) -> usize {
    Arc::as_ptr(registry).cast::<()>() as usize
}

fn scheduler_for(
    registry: &Arc<dyn TaskRegistryHandle>,
) -> Result<Arc<CronScheduler>, NoActiveCronScheduler> {
    let key = task_registry_identity(registry);
    let scheduler = LIVE_SCHEDULERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
        .and_then(Weak::upgrade);
    scheduler.ok_or(NoActiveCronScheduler)
}

/// Register a newly-created tool job with the already-running scheduler.
/// Durable jobs are still authoritative on disk; session jobs are also added
/// to the scheduler-owned in-memory store used by list/delete.
pub async fn register_live_job(
    registry: &Arc<dyn TaskRegistryHandle>,
    task: SessionCronTask,
    durable: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    scheduler_for(registry)?
        .register_tool_job(task, durable)
        .await
}

/// Snapshot session-only jobs for `CronList` and stop-hook consumers.
pub async fn session_jobs(
    registry: &Arc<dyn TaskRegistryHandle>,
) -> Result<Vec<SessionCronTask>, NoActiveCronScheduler> {
    Ok(scheduler_for(registry)?.session_jobs().await)
}

/// Remove a runtime job. For a durable job this only removes the scheduler's
/// live mirror; the caller remains responsible for its locked file mutation.
pub async fn unregister_live_job(
    registry: &Arc<dyn TaskRegistryHandle>,
    id: &str,
    owner: Option<&str>,
) -> Result<bool, NoActiveCronScheduler> {
    Ok(scheduler_for(registry)?.unregister_job(id, owner).await)
}

/// PARITY `M()` first-sight diagnostics: `[ScheduledTasks] scheduled ${id} for
/// ${iso | "never"}`; an unresolvable schedule also counts `next_fire_unresolvable`.
fn log_scheduled_at(id: &str, at: Option<SystemTime>, recurring: bool) {
    match at {
        Some(at) => {
            tracing::info!(
                "[ScheduledTasks] scheduled {id} for {}",
                crate::schedule::iso_8601_utc(unix_epoch_ms(at))
            );
        }
        None => {
            tracing::info!(
                event = "cron_task_fire",
                outcome = "next_fire_unresolvable",
                recurring,
            );
            tracing::info!("[ScheduledTasks] scheduled {id} for never");
        }
    }
}

/// Resolve a job's next fire for the FIRST time (registration, or a tick that
/// misses the cache) and log it. The value is then cached in
/// [`CronScheduler::next_fire`] and reused until the job fires — the oracle
/// only computes a schedule on a cache miss (`M()`: `if(x===void 0){…}`).
fn schedule_fire(def: &CronTaskDef) -> Option<SystemTime> {
    let at = next_fire_time(def);
    log_scheduled_at(&def.id, at, def.recurring);
    at
}

/// The inputs [`next_fire_time`] reads, so a cached fire can tell "still the
/// same schedule" from "this record was edited".
///
/// The oracle keys its cache on the task id alone and recomputes only after a
/// fire, because in claude-code nothing but the scheduler mutates a live task.
/// LingXi's tasks file is also written by the desktop UI and the cron tools
/// while the tick loop runs, so an edited record must recompute instead of
/// firing on the schedule it had when it was first seen.
#[derive(Clone, PartialEq, Eq)]
struct FireFingerprint {
    cron: String,
    last_run: Option<SystemTime>,
    created_at: SystemTime,
    recurring: bool,
}

impl FireFingerprint {
    fn of(def: &CronTaskDef) -> Self {
        Self {
            cron: def.schedule.raw.clone(),
            last_run: def.last_run,
            created_at: def.created_at,
            recurring: def.recurring,
        }
    }
}

/// PARITY `I` (the oracle's `Map<taskId, nextFireMs>`): one job's cached next
/// fire. `at: None` is the oracle's `Infinity` — an unresolvable schedule that
/// never fires and is not recomputed every second.
struct CachedFire {
    fingerprint: FireFingerprint,
    at: Option<SystemTime>,
}

/// The tasks-file generation a scheduler has applied.
///
/// PARITY the chokidar watcher the oracle opens on `rJ(dir)`: durable state is
/// re-read into the live schedule when the document CHANGES, not once a second.
/// LingXi compares the bytes rather than subscribing to
/// [`FileSystem::watch`](platform_api::FileSystem::watch) because a platform
/// whose watcher yields nothing would silently stop picking up peer edits;
/// and rather than an mtime/size stamp, which cannot tell a same-second
/// rewrite of equal length from no write at all. Writes go through
/// `write_file_rooted_atomic`, so the unlocked comparison read always observes
/// one whole generation of the file.
#[derive(Clone, PartialEq, Eq)]
enum TasksFileSnapshot {
    /// The file does not exist (the watcher's `unlink`).
    Absent,
    /// The exact body last applied.
    Body(String),
}

/// PARITY `be(tasks)`: the prompt that surfaces missed durable one-shots.
#[must_use]
pub fn missed_one_shots_prompt(missed: &[crate::tasks_file::CronTask]) -> String {
    let plural = missed.len() > 1;
    let head = format!(
        "The following one-shot scheduled task{} missed while Claude was not running. {} already been removed from .claude/scheduled_tasks.json.\n\nDo NOT execute {} yet. First use the AskUserQuestion tool to ask whether to run {} now. Only execute if the user confirms.",
        if plural { "s were" } else { " was" },
        if plural { "They have" } else { "It has" },
        if plural { "these prompts" } else { "this prompt" },
        if plural { "each one" } else { "it" },
    );
    let entries: Vec<String> = missed
        .iter()
        .map(|t| {
            format!(
                "[{}, created {}]\n{}",
                crate::schedule::human_schedule(&t.cron),
                crate::schedule::local_date_time_string(t.created_at),
                t.prompt
            )
        })
        .collect();
    format!("{head}\n\n{}", entries.join("\n\n"))
}

/// Claude Code 2.1.270 default lifespan for recurring cron jobs.
/// Task-center v2 automations use their separate explicit expiration policy.
pub const DEFAULT_RECURRING_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Claude Code's recurring jitter is at most half of the schedule interval.
pub const DEFAULT_RECURRING_JITTER_FRACTION: f64 = 0.5;
/// Absolute cap for recurring jitter, even on daily/weekly schedules.
pub const DEFAULT_RECURRING_JITTER_CAP: Duration = Duration::from_secs(30 * 60);
/// Maximum amount a one-shot scheduled on a half-hour boundary fires early.
pub const DEFAULT_ONE_SHOT_JITTER_MAX: Duration = Duration::from_secs(90);
/// One-shots only receive early jitter on minute 0 or 30.
pub const DEFAULT_ONE_SHOT_MINUTE_MOD: u32 = 30;
/// Five-minute recurring schedules run this far before the cache boundary.
pub const DEFAULT_CACHE_LEAD: Duration = Duration::from_secs(15);

const CACHE_TTL: Duration = Duration::from_secs(5 * 60);

/// How long `stop` waits for proof that the tick future and its captured
/// owners were destroyed. Host shutdown awaits this, so it cannot be
/// unbounded.
const TICK_DESTRUCTION_BUDGET: Duration = Duration::from_secs(2);
const SCHEDULER_TICK_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug)]
struct CronJitterConfig {
    recurring_fraction: f64,
    recurring_cap: Duration,
    one_shot_floor: Duration,
    one_shot_max: Duration,
    one_shot_minute_mod: u32,
    cache_lead: Duration,
}

impl Default for CronJitterConfig {
    fn default() -> Self {
        Self {
            recurring_fraction: DEFAULT_RECURRING_JITTER_FRACTION,
            recurring_cap: DEFAULT_RECURRING_JITTER_CAP,
            one_shot_floor: Duration::ZERO,
            one_shot_max: DEFAULT_ONE_SHOT_JITTER_MAX,
            one_shot_minute_mod: DEFAULT_ONE_SHOT_MINUTE_MOD,
            cache_lead: DEFAULT_CACHE_LEAD,
        }
    }
}

/// One registered cron job: its schedule, prompt, and last-fire bookkeeping.
#[derive(Clone)]
pub struct CronTaskDef {
    /// Stable job identifier (used for lock filenames and logs).
    pub id: String,
    /// Parsed cron schedule.
    pub schedule: CronExpression,
    /// Initial prompt handed to the spawned Dream task.
    pub prompt: String,
    /// Optional agent type hint for the spawned task.
    pub agent_type: Option<String>,
    /// Last time this job fired (used to suppress same-minute duplicates).
    pub last_run: Option<SystemTime>,
    /// When `false`, the scheduler skips this job entirely.
    pub enabled: bool,
    /// When the job was created (the persisted `createdAt`, epoch ms on disk).
    /// The anchor for recurring auto-expiry ([`is_recurring_task_aged`]).
    pub created_at: SystemTime,
    /// `true` = fire on every schedule match until deleted or auto-expired;
    /// `false` = one-shot. Only recurring jobs are subject to max-age expiry.
    pub recurring: bool,
}

/// Is a RECURRING cron job past its auto-expiry age? 1:1 with claude-code
/// `isRecurringTaskAged` (`cronScheduler.ts:59`,
/// `recurring && nowMs - createdAt >= maxAgeMs`). A one-shot (`!recurring`) job
/// is never aged-out here (it auto-deletes after firing instead);
/// `max_age == None` disables expiry entirely (unlimited); and a `created_at` in
/// the future (clock skew) is treated as not-yet-aged.
///
#[must_use]
pub(crate) fn is_recurring_task_aged(
    now: SystemTime,
    created_at: SystemTime,
    recurring: bool,
    max_age: Option<Duration>,
) -> bool {
    let Some(max_age) = max_age else {
        return false;
    };
    !max_age.is_zero()
        && recurring
        && now
            .duration_since(created_at)
            .is_ok_and(|age| age >= max_age)
}

/// Post-fire bookkeeping for a job that JUST fired. A ONE-SHOT (`!recurring`)
/// job is REMOVED from `tasks` and `true` is returned so the caller also deletes
/// its persisted descriptor — 1:1 with claude-code `recurring: false` ("fire
/// once at the next match, then auto-delete", `schedule_cron.rs` schema). A
/// recurring job records `last_run` in place and returns `false`. A missing id
/// is a no-op (`false`).
pub(crate) fn finalize_fired_job(
    tasks: &mut HashMap<String, CronTaskDef>,
    id: &str,
    now: SystemTime,
) -> bool {
    let recurring = match tasks.get(id) {
        Some(t) => t.recurring,
        None => return false,
    };
    if recurring {
        if let Some(t) = tasks.get_mut(id) {
            t.last_run = Some(now);
        }
        false
    } else {
        tasks.remove(id);
        true
    }
}

/// Set `lastFiredAt` (epoch **milliseconds**) on the task with `id` inside a
/// parsed single-file `{ "tasks": [...] }` document body, returning the
/// re-serialized file (pretty + trailing newline, 1:1 with claude-code). Returns
/// `None` if the body has no such id (so the caller leaves the file untouched).
/// Recording the last-fire time is what makes missed-run CATCH-UP safe ACROSS
/// RESTARTS — without it a reloaded job would re-fire a run it already fired in
/// a prior session (claude-code persists `lastFiredAt` for the same reason).
pub(crate) fn tasks_file_with_last_fired(
    body: &str,
    id: &str,
    last_fired_at_ms: u64,
) -> Option<String> {
    let mut doc = crate::tasks_file::parse_tasks(body);
    let task = doc.tasks.iter_mut().find(|t| t.id == id)?;
    task.last_fired_at = Some(last_fired_at_ms);
    Some(crate::tasks_file::serialize_tasks(&doc))
}

/// Remove the task with `id` from a parsed single-file `{ "tasks": [...] }`
/// document body, returning the re-serialized file. Returns `None` if the id is
/// absent (caller leaves the file untouched). Used when a one-shot job has fired
/// (auto-delete) and when a recurring job ages out.
pub(crate) fn tasks_file_without(body: &str, id: &str) -> Option<String> {
    let mut doc = crate::tasks_file::parse_tasks(body);
    let before = doc.tasks.len();
    doc.tasks.retain(|t| t.id != id);
    if doc.tasks.len() == before {
        return None;
    }
    Some(crate::tasks_file::serialize_tasks(&doc))
}

/// Convert the first eight hexadecimal digits of a job id to the stable
/// `[0, 1)` fraction used by Claude Code's `parseInt(id.slice(0, 8), 16) /
/// 2**32`. `parseInt` accepts a leading run of hex digits, so preserve that
/// behavior for legacy/non-generated ids instead of requiring all eight.
fn deterministic_id_fraction(id: &str) -> f64 {
    let prefix = id
        .as_bytes()
        .iter()
        .copied()
        .take(8)
        .take_while(u8::is_ascii_hexdigit)
        .collect::<Vec<_>>();
    if prefix.is_empty() {
        return 0.0;
    }
    let Ok(prefix) = std::str::from_utf8(&prefix) else {
        return 0.0;
    };
    u32::from_str_radix(prefix, 16).map_or(0.0, |value| f64::from(value) / 4_294_967_296.0)
}

fn scale_duration(duration: Duration, factor: f64) -> Duration {
    Duration::from_secs_f64(duration.as_secs_f64() * factor)
}

fn is_exact_every_n_minutes(expression: &str) -> bool {
    let Some(rest) = expression.strip_prefix("*/") else {
        return false;
    };
    let Some(step) = rest.strip_suffix(" * * * *") else {
        return false;
    };
    !step.is_empty() && step.bytes().all(|byte| byte.is_ascii_digit())
}

fn local_minute_with<F: Fn(u64) -> i64>(at: SystemTime, offset_for: &F) -> Option<u32> {
    let unix_secs = at.duration_since(SystemTime::UNIX_EPOCH).ok()?.as_secs();
    let local_secs = i128::from(unix_secs).saturating_add(i128::from(offset_for(unix_secs)));
    u32::try_from(local_secs.rem_euclid(3_600) / 60).ok()
}

fn next_fire_with<F: Fn(u64) -> i64>(
    task: &CronTaskDef,
    offset_for: &F,
    config: CronJitterConfig,
) -> Option<SystemTime> {
    // PARITY: recurring fires anchor on `lastFiredAt ?? createdAt` (`eXe`);
    // a one-shot is always computed from `createdAt` (`Jbt(cron, createdAt)`).
    let anchor = if task.recurring {
        task.last_run.unwrap_or(task.created_at)
    } else {
        task.created_at
    };
    let next = task.schedule.next_match_after_with(anchor, offset_for)?;

    if !task.recurring {
        let minute = local_minute_with(next, offset_for)?;
        if minute % config.one_shot_minute_mod != 0 {
            return Some(next);
        }
        let range = config
            .one_shot_max
            .checked_sub(config.one_shot_floor)
            .unwrap_or(Duration::ZERO);
        let early =
            config.one_shot_floor + scale_duration(range, deterministic_id_fraction(&task.id));
        return Some(next.checked_sub(early).unwrap_or(anchor).max(anchor));
    }

    let following = task.schedule.next_match_after_with(next, offset_for);
    let Some(interval) = following.and_then(|following| following.duration_since(next).ok()) else {
        return Some(next);
    };

    // Claude Code's cache-lead exception is intentionally narrow: only a raw
    // `*/N * * * *` expression whose interval is exactly the five-minute cache
    // TTL is shifted to `anchor + interval - 15s`.
    if is_exact_every_n_minutes(&task.schedule.raw)
        && config.cache_lead > Duration::ZERO
        && config.cache_lead < interval
        && interval >= CACHE_TTL
        && interval - config.cache_lead < CACHE_TTL
    {
        return anchor.checked_add(interval - config.cache_lead);
    }

    let jitter = scale_duration(
        interval,
        deterministic_id_fraction(&task.id) * config.recurring_fraction,
    )
    .min(config.recurring_cap);
    next.checked_add(jitter)
}

/// The deterministic next fire time for a job, including Claude Code's
/// recurring/one-shot jitter and five-minute cache-lead exception.
pub(crate) fn next_fire_time(task: &CronTaskDef) -> Option<SystemTime> {
    next_fire_with(
        task,
        &|secs| crate::schedule::local_offset_seconds(i64::try_from(secs).unwrap_or(0)),
        CronJitterConfig::default(),
    )
}

/// Is a job due at `now`? The next jittered fire after the last fire (or
/// creation) must have arrived. An overdue run is caught up once; persisting
/// `last_run = now` advances the anchor so it cannot be repeated after restart.
pub(crate) fn is_job_due(task: &CronTaskDef, now: SystemTime) -> bool {
    task.enabled && next_fire_time(task).is_some_and(|next| next <= now)
}

/// Offset-parameterized twin of [`is_job_due`] for deterministic, host-timezone
/// independent tests: `offset_for` supplies each candidate's UTC offset (use
/// `|_| 0` for UTC). The live [`is_job_due`] resolves the offset per-instant via
/// the system timezone, so its 09:00-LOCAL behavior can't be asserted against
/// fixed UTC timestamps directly.
#[cfg(test)]
fn is_job_due_with<F: Fn(u64) -> i64>(task: &CronTaskDef, now: SystemTime, offset_for: F) -> bool {
    task.enabled
        && next_fire_with(task, &offset_for, CronJitterConfig::default())
            .is_some_and(|next| next <= now)
}

/// Owner of the cron tick loop. Constructed with platform trait objects and
/// the shared [`TaskRegistry`].
pub struct CronScheduler {
    session_id: StdMutex<Option<String>>,
    session_cron_enabled: bool,
    pending_missed: Mutex<Vec<crate::tasks_file::CronTask>>,
    session_delivery: RwLock<Option<Arc<dyn SessionCronDelivery>>>,
    fallback_identity: String,
    project_leader: AtomicBool,
    tasks: Arc<RwLock<HashMap<String, CronTaskDef>>>,
    session_tasks: Arc<RwLock<HashMap<String, SessionCronTask>>>,
    /// Durable identities must never fall back to session execution after disk deletion.
    durable_ids: RwLock<HashSet<String>>,
    /// PARITY `I`: each job's next fire, computed once per schedule instead of
    /// once per tick. Locked AFTER `tasks` wherever both are held.
    next_fire: RwLock<HashMap<String, CachedFire>>,
    /// The tasks-file generation currently mirrored into `tasks`; `None` before
    /// the first reload. Gates the locked re-read (PARITY the file watcher).
    applied_snapshot: Mutex<Option<TasksFileSnapshot>>,
    automation_firer: RwLock<Option<Arc<dyn crate::CronJobFirer>>>,
    automation_runs: Mutex<HashMap<String, AutomationFlight>>,
    task_registry: Arc<TaskRegistry>,
    fs: Arc<dyn FileSystem>,
    clock: Arc<dyn Clock>,
    runtime: Arc<dyn RuntimeSpawner>,
    /// The single project tasks file
    /// (`<project_root>/.lingxi/scheduled_tasks.json`) the scheduler loads from
    /// and writes `lastFiredAt` back to. 1:1 with claude-code `cronTasks.ts`.
    tasks_file: PathBuf,
    /// Auto-expiry age for RECURRING jobs; `None` disables expiry (unlimited).
    /// Defaults to seven days for Claude-compatible recurring jobs.
    recurring_max_age: Option<Duration>,
    tick_handle: Mutex<Option<TickHandle>>,
}

struct AutomationFlight {
    request: crate::AutomationRunRequest,
    handle: Option<TickHandle>,
    commit: Arc<StdMutex<AutomationCommit>>,
    firer: Arc<dyn crate::CronJobFirer>,
}

#[derive(Clone)]
enum AutomationCommit {
    AwaitingOutcome,
    Pending {
        result: Result<crate::AutomationRunResult, String>,
        finished_at: u64,
    },
    Settled,
}

impl AutomationFlight {
    fn is_running(&mut self) -> bool {
        self.handle.as_mut().is_some_and(|handle| {
            matches!(
                handle.completed.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            )
        })
    }
}

async fn persist_automation_outcome(
    fs: &dyn FileSystem,
    root: &Path,
    request: &crate::AutomationRunRequest,
    commit: &StdMutex<AutomationCommit>,
) -> Result<(), String> {
    let outcome = commit
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    match outcome {
        AutomationCommit::Settled => return Ok(()),
        AutomationCommit::AwaitingOutcome => {
            return Err("Execution has not released its outcome".into())
        }
        AutomationCommit::Pending {
            result,
            finished_at,
        } => {
            crate::finish_automation_run_checked(fs, root, request, &result, finished_at).await?;
        }
    }
    *commit
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = AutomationCommit::Settled;
    Ok(())
}

struct TickHandle {
    runtime_handle: platform_api::BackgroundTaskHandle,
    completed: tokio::sync::oneshot::Receiver<()>,
}

/// Field order is intentional: drop the entire producer future (including its
/// scheduler/registry owners) before closing the completion channel. This also
/// works when the runtime cancels the task before its first poll.
struct TickFuture {
    future: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
    _completed: tokio::sync::oneshot::Sender<()>,
}

impl std::future::Future for TickFuture {
    type Output = ();

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        self.get_mut().future.as_mut().poll(cx)
    }
}

struct ClaimedCronJob {
    spawn_input: TaskSpawnInput,
    rollback: ClaimRollback,
    /// Captured AT CLAIM TIME: claiming a one-shot (or an aged-out recurring
    /// job) removes it from `self.tasks`, so the fire diagnostics cannot read it
    /// back out of the map afterwards.
    recurring: bool,
}

enum ClaimRollback {
    Session {
        before_task: CronTaskDef,
        before_session_task: Option<SessionCronTask>,
        claimed_task: Option<CronTaskDef>,
        claimed_session_task: Option<SessionCronTask>,
    },
    Durable {
        project_root: PathBuf,
        authoritative: CronTaskDef,
        before_body: String,
        claimed_body: String,
    },
}

impl ClaimedCronJob {
    /// The prompt this claim will spawn — read BEFORE [`Self::into_parts`]
    /// consumes it.
    fn prompt(&self) -> &str {
        match &self.spawn_input {
            TaskSpawnInput::Dream { prompt, .. } => prompt.as_str(),
            _ => "",
        }
    }

    fn into_parts(self) -> (TaskSpawnInput, ClaimRollback) {
        (self.spawn_input, self.rollback)
    }
}

fn is_dynamic_loop_prompt(prompt: &str) -> bool {
    prompt == crate::AUTONOMOUS_LOOP_DYNAMIC_SENTINEL || prompt == crate::LOOP_FILE_DYNAMIC_SENTINEL
}

#[async_trait::async_trait]
impl platform_api::LoopUsageProvider for CronScheduler {
    async fn usage_rows(&self) -> Vec<platform_api::LoopUsageRow> {
        self.loop_usage_rows().await
    }
}

impl CronScheduler {
    /// Construct a new scheduler over the single project tasks file
    /// `<project_root>/.lingxi/scheduled_tasks.json` (1:1 with claude-code
    /// `cronTasks.ts`). The per-job A9 lock files live in that file's parent
    /// directory (`<project_root>/.claude`). The tick loop is not started until
    /// [`Self::start`] is invoked.
    #[must_use]
    pub fn new(
        task_registry: Arc<TaskRegistry>,
        fs: Arc<dyn FileSystem>,
        clock: Arc<dyn Clock>,
        runtime: Arc<dyn RuntimeSpawner>,
        tasks_file: PathBuf,
    ) -> Self {
        let fallback_identity = format!(
            "cron-{}-{}",
            std::process::id(),
            task_registry_identity(&task_registry)
        );
        Self {
            tasks: Arc::new(RwLock::new(HashMap::new())),
            session_tasks: Arc::new(RwLock::new(HashMap::new())),
            durable_ids: RwLock::new(HashSet::new()),
            next_fire: RwLock::new(HashMap::new()),
            applied_snapshot: Mutex::new(None),
            automation_firer: RwLock::new(None),
            automation_runs: Mutex::new(HashMap::new()),
            task_registry,
            fs,
            clock,
            runtime,
            tasks_file,
            fallback_identity,
            project_leader: AtomicBool::new(false),
            session_id: StdMutex::new(None),
            session_cron_enabled: true,
            pending_missed: Mutex::new(Vec::new()),
            session_delivery: RwLock::new(None),
            recurring_max_age: Some(DEFAULT_RECURRING_MAX_AGE),
            tick_handle: Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn with_test_registry_delivery(mut self) -> Self {
        self.session_delivery = RwLock::new(Some(Arc::new(RegistryTestDelivery(
            self.task_registry.clone(),
        ))));
        self
    }

    /// Disable session cron in the independent task-center controller.
    #[must_use]
    pub fn with_session_cron(mut self, enabled: bool) -> Self {
        self.session_cron_enabled = enabled;
        self
    }

    /// Bind the conversation queue before session cron fires are claimed.
    pub async fn set_session_delivery(&self, delivery: Arc<dyn SessionCronDelivery>) {
        *self.session_delivery.write().await = Some(delivery);
        self.load_persisted().await;
    }

    /// Bind creator-owned durable cron jobs to their conversation session.
    #[must_use]
    pub fn with_session_id(mut self, session_id: String) -> Self {
        self.session_id = StdMutex::new(Some(session_id));
        self
    }

    /// Install the host session executor before starting the scheduler.
    pub async fn set_automation_firer(&self, firer: Arc<dyn crate::CronJobFirer>) {
        *self.automation_firer.write().await = Some(firer);
    }

    fn current_session_id(&self) -> Option<String> {
        self.session_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Switch sessions after joining the old tick and releasing its lease.
    pub async fn set_session_id(
        self: &Arc<Self>,
        id: String,
    ) -> Result<(), platform_api::RuntimeError> {
        if self.current_session_id().as_deref() == Some(id.as_str()) {
            return Ok(());
        }
        self.stop().await?;
        if let Some(delivery) = self.session_delivery.read().await.clone() {
            delivery.clear_queued().await;
        }
        *self
            .session_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(id);
        self.load_persisted().await;
        self.clone().start().await
    }

    fn lease_identity(&self) -> String {
        self.current_session_id()
            .unwrap_or_else(|| self.fallback_identity.clone())
    }

    async fn refresh_project_leader(&self) {
        let Some(root) = crate::tasks_file::project_root_from_tasks_path(&self.tasks_file) else {
            return;
        };
        let leader = crate::lock::acquire_scheduler_lease(
            self.fs.as_ref(),
            root,
            &self.lease_identity(),
            unix_epoch_ms(self.clock.now()),
        )
        .await
        .unwrap_or(false);
        self.project_leader.store(leader, Ordering::SeqCst);
    }

    fn owns_durable_task(&self, creator: &crate::tasks_file::CronTaskCreator) -> bool {
        if creator
            .created_by_session_id
            .as_deref()
            .is_some_and(|id| Some(id) == self.current_session_id().as_deref())
        {
            return true;
        }
        self.project_leader.load(Ordering::SeqCst)
            && creator.can_run_for(self.current_session_id().as_deref())
    }

    async fn refresh_creator_process(&self) {
        let Some(session_id) = self.current_session_id() else {
            return;
        };
        let Some(root) = crate::tasks_file::project_root_from_tasks_path(&self.tasks_file) else {
            return;
        };
        let _process_guard = crate::lock_cron_file().await;
        let Ok(_file_guard) = crate::tasks_file::lock_scheduled_tasks(self.fs.as_ref(), root).await
        else {
            return;
        };
        let Ok(body) = crate::tasks_file::read_tasks_body(self.fs.as_ref(), root).await else {
            return;
        };
        let Ok(mut doc) = crate::tasks_file::parse_tasks_strict(&body) else {
            return;
        };
        let pid = std::process::id();
        let mut changed = false;
        for task in &mut doc.tasks {
            if task.automation.is_none()
                && task.creator.created_by_session_id.as_deref() == Some(session_id.as_str())
                && task.creator.created_by_pid != Some(pid)
            {
                task.creator.created_by_pid = Some(pid);
                task.creator.created_by_proc_start =
                    platform_api::live_sessions::process_start_identity(pid);
                changed = true;
            }
        }
        if changed {
            let _ = crate::tasks_file::write_tasks_body(
                self.fs.as_ref(),
                root,
                &crate::tasks_file::serialize_tasks(&doc),
            )
            .await;
        }
    }

    /// Load every persisted (durable) job from the single project tasks file,
    /// registering each into the in-memory schedule. A missing / unparseable
    /// file registers nothing. `createdAt` / `lastFiredAt` are read in epoch
    /// **milliseconds** (claude-code on-disk units). Invalid cron strings are
    /// skipped with a warning. Call once after construction, before
    /// [`Self::start`].
    pub async fn load_persisted(&self) {
        if !self.session_cron_enabled {
            return;
        }
        self.refresh_creator_process().await;
        self.refresh_project_leader().await;
        let Some(project_root) = crate::tasks_file::project_root_from_tasks_path(&self.tasks_file)
        else {
            tracing::error!(path = %self.tasks_file.display(), "cron: invalid tasks-file path");
            return;
        };
        let Ok(body) = crate::tasks_file::read_tasks_body(self.fs.as_ref(), project_root).await
        else {
            return; // file absent → nothing to load
        };
        let doc = match crate::tasks_file::parse_tasks_strict(&body) {
            Ok(doc) => doc,
            Err(error) => {
                // `parse_tasks` intentionally treats corrupt user-facing reads
                // as empty, but scheduler startup must not silently reinterpret
                // corrupt durable state as an empty valid task document.
                tracing::warn!(
                    path = %self.tasks_file.display(),
                    "cron tasks file has invalid authoritative state: {error}; skipping"
                );
                return;
            }
        };
        let now = self.clock.now();
        let mut missed: Vec<crate::tasks_file::CronTask> = Vec::new();
        for t in doc.tasks {
            if t.automation.is_some() || !self.owns_durable_task(&t.creator) {
                continue;
            }
            // Anchor expiry/catch-up off the persisted ms timestamps. A
            // missing/zero `createdAt` falls back to "now" (a fresh window):
            // the parser preserves an explicit `0`, and anchoring that at the
            // epoch makes every one-shot "missed" and ages every recurring job
            // out of `recurring_max_age` on its first load.
            let created_at = if t.created_at > 0 {
                SystemTime::UNIX_EPOCH + Duration::from_millis(t.created_at)
            } else {
                self.clock.now()
            };
            let recurring = t.recurring.unwrap_or(false);
            // PARITY `zQn`: a durable ONE-SHOT whose fire time (from `createdAt`)
            // already passed was missed while nothing was running. It is never
            // fired: it is removed from the file and surfaced to the user for
            // confirmation (`onMissed` / the `be()` prompt).
            //
            // Upstream Amr scores missed one-shots from createdAt. A one-shot
            // that ALREADY FIRED is still not a miss: `lastFiredAt` is persisted
            // before the launch and the delete that follows it can lose the race
            // (crash, failed write, a peer holding the lock), so the record can
            // outlive its own run. Offering it as "missed" would ask the user to
            // authorise work that already happened.
            if !recurring && t.last_fired_at.is_none_or(|ms| ms == 0) {
                let passed = parse_cron(&t.cron)
                    .ok()
                    .and_then(|schedule| schedule.next_match_after(created_at));
                if passed.is_some_and(|next| next < now) {
                    missed.push(t);
                    continue;
                }
            }
            let last_run = t
                .last_fired_at
                .map(|ms| SystemTime::UNIX_EPOCH + Duration::from_millis(ms));
            if let Err(e) = self
                .register_with_meta(
                    &t.id, &t.cron, &t.prompt, None, created_at, recurring, last_run,
                )
                .await
            {
                tracing::warn!("cron: skipping job {} with invalid schedule: {e}", t.id);
            } else {
                self.durable_ids.write().await.insert(t.id);
            }
        }
        if !missed.is_empty() {
            // Lifecycle setters may hold the host transition mutex. Publish
            // catch-up only from the background tick after those setters return.
            *self.pending_missed.lock().await = missed;
        }
    }

    /// PARITY `V(true)` missed-task branch: remove the missed one-shots from the
    /// tasks file (`SK(ids)`), emit `tengu_scheduled_task_missed`, and hand the
    /// user the `be()` prompt — which tells the model NOT to run the prompts
    /// before asking via AskUserQuestion — as one scheduled turn.
    async fn surface_missed_one_shots(
        &self,
        project_root: &Path,
        missed: Vec<crate::tasks_file::CronTask>,
    ) {
        let delivery = self.session_delivery.read().await.clone();
        if delivery.is_none() || delivery.as_ref().unwrap().is_loading().await {
            *self.pending_missed.lock().await = missed;
            return;
        }
        let delivery = delivery.unwrap();
        let ids: Vec<String> = missed.iter().map(|t| t.id.clone()).collect();
        tracing::info!(
            event = "tengu_scheduled_task_missed",
            count = missed.len(),
            task_ids = %ids.join(","),
        );
        // Remove FIRST. The prompt states as fact that these tasks "have already
        // been removed from .lingxi/scheduled_tasks.json", and a task still on
        // disk is re-registered by the next tick's `refresh_durable_tasks` and
        // fired — the exact unconfirmed run this branch exists to prevent. So
        // the removal must land before the claim is made, not after it.
        let removed = self.remove_missed_from_file(project_root, &ids).await;
        if !removed {
            // Say nothing rather than assert a removal that did not happen; the
            // tasks stay on disk and are re-surfaced by the next startup.
            //
            // Park them back in `pending_missed` first. `tick` reached here via
            // `std::mem::take`, so dropping them empties the set that
            // `process_due_ids` uses to suppress a normal fire — and the still-
            // on-disk task is re-registered by the very next
            // `refresh_durable_tasks` and fired with its RAW prompt, which is
            // exactly the unconfirmed run this branch exists to prevent. The
            // loading-delivery early return above parks them for the same reason.
            *self.pending_missed.lock().await = missed;
            tracing::error!(
                "[ScheduledTasks] could not remove {} missed one-shot task(s); \
                 not surfacing them this run",
                ids.len()
            );
            return;
        }
        let prompt = missed_one_shots_prompt(&missed);
        if let Err(e) = delivery
            .enqueue(SessionCronFire {
                id: ids.join(","),
                cron: String::new(),
                prompt,
                owner: None,
            })
            .await
        {
            // The binary removes the missed tasks unconditionally (`SK(ids)`
            // runs after `onFire`), so a delivery failure never resurrects them
            // as a silent fire on the next reload.
            tracing::error!("[ScheduledTasks] failed to surface missed tasks: {e}");
        }
        tracing::info!(
            "[ScheduledTasks] surfaced {} missed one-shot task(s)",
            ids.len()
        );
    }

    /// Drop `ids` from the tasks file under both cron locks. `false` means the
    /// file still lists at least one of them.
    async fn remove_missed_from_file(&self, project_root: &Path, ids: &[String]) -> bool {
        let _process_guard = crate::lock_cron_file().await;
        let Ok(_file_guard) =
            crate::tasks_file::lock_scheduled_tasks(self.fs.as_ref(), project_root).await
        else {
            tracing::warn!("[ScheduledTasks] failed to remove missed tasks: lock unavailable");
            return false;
        };
        let Ok(mut body) = crate::tasks_file::read_tasks_body(self.fs.as_ref(), project_root).await
        else {
            tracing::warn!("[ScheduledTasks] failed to remove missed tasks: unreadable file");
            return false;
        };
        for id in ids {
            if let Some(updated) = tasks_file_without(&body, id) {
                body = updated;
            }
        }
        if let Err(e) =
            crate::tasks_file::write_tasks_body(self.fs.as_ref(), project_root, &body).await
        {
            tracing::warn!("[ScheduledTasks] failed to remove missed tasks: {e}");
            return false;
        }
        true
    }

    /// Override the recurring auto-expiry age (`None` = unlimited / never
    /// expire). Defaults to seven days.
    #[must_use]
    pub fn with_recurring_max_age(mut self, age: Option<Duration>) -> Self {
        self.recurring_max_age = age;
        self
    }

    /// Register a new cron job. The schedule string is parsed eagerly; an
    /// invalid expression is returned as a boxed error.
    pub async fn register(
        &self,
        id: &str,
        schedule_str: &str,
        prompt: &str,
        agent_type: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.register_with_meta(
            id,
            schedule_str,
            prompt,
            agent_type,
            self.clock.now(),
            true,
            None,
        )
        .await
    }

    /// Like [`Self::register`] but with the persisted `created_at` (the expiry
    /// anchor), `recurring` flag, and `last_run` (the restored last-fire time, or
    /// `None` if never fired) — used by the on-disk job loader so a job created
    /// days ago is aged correctly AND its catch-up anchor survives a restart
    /// (preventing a re-fire of an already-fired run). `register` is the
    /// convenience form (created now, recurring, never fired).
    pub async fn register_with_meta(
        &self,
        id: &str,
        schedule_str: &str,
        prompt: &str,
        agent_type: Option<String>,
        created_at: SystemTime,
        recurring: bool,
        last_run: Option<SystemTime>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let schedule = parse_cron(schedule_str)?;
        let def = CronTaskDef {
            id: id.into(),
            schedule,
            prompt: prompt.into(),
            agent_type,
            last_run,
            enabled: true,
            created_at,
            recurring,
        };
        let at = schedule_fire(&def);
        let fingerprint = FireFingerprint::of(&def);
        let mut tasks = self.tasks.write().await;
        tasks.insert(id.to_string(), def);
        self.next_fire
            .write()
            .await
            .insert(id.to_string(), CachedFire { fingerprint, at });
        drop(tasks);
        Ok(())
    }

    async fn register_tool_job(
        &self,
        task: SessionCronTask,
        durable: bool,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let schedule = parse_cron(&task.cron)?;
        let mut tasks = self.tasks.write().await;
        if !tasks.contains_key(&task.id) && tasks.len() >= 50 {
            return Err(std::io::Error::other("too many scheduled jobs (max 50)").into());
        }
        let def = CronTaskDef {
            id: task.id.clone(),
            schedule,
            prompt: task.prompt.clone(),
            agent_type: None,
            last_run: task.last_fired_at,
            enabled: true,
            created_at: task.created_at,
            recurring: task.recurring,
        };
        let at = schedule_fire(&def);
        let fingerprint = FireFingerprint::of(&def);
        tasks.insert(task.id.clone(), def);
        self.next_fire
            .write()
            .await
            .insert(task.id.clone(), CachedFire { fingerprint, at });
        drop(tasks);
        if durable {
            self.durable_ids.write().await.insert(task.id.clone());
        }
        if !durable {
            self.session_tasks
                .write()
                .await
                .insert(task.id.clone(), task);
        }
        Ok(())
    }

    async fn session_jobs(&self) -> Vec<SessionCronTask> {
        let mut jobs: Vec<_> = self.session_tasks.read().await.values().cloned().collect();
        jobs.sort_by(|a, b| a.id.cmp(&b.id));
        jobs
    }

    /// `/usage` Loops rows for every live job (session + durable).
    pub async fn loop_usage_rows(&self) -> Vec<platform_api::LoopUsageRow> {
        let now = self.clock.now();
        let tasks = self.tasks.read().await;
        let mut rows: Vec<_> = tasks
            .values()
            .map(|task| {
                crate::schedule::loop_usage_row(
                    &task.prompt,
                    &task.schedule.raw,
                    is_dynamic_loop_prompt(&task.prompt),
                    task.last_run,
                    now,
                )
            })
            .collect();
        rows.sort_by(|a, b| {
            b.tokens
                .cmp(&a.tokens)
                .then_with(|| a.prompt.cmp(&b.prompt))
        });
        rows
    }

    async fn unregister_job(&self, id: &str, owner: Option<&str>) -> bool {
        let session_match = {
            let tasks = self.session_tasks.read().await;
            tasks.get(id).is_some_and(|task| match owner {
                Some(owner) => task.owner.as_deref() == Some(owner),
                None => true,
            })
        };
        let is_session = self.session_tasks.read().await.contains_key(id);
        if is_session && !session_match {
            return false;
        }
        self.session_tasks.write().await.remove(id);
        self.durable_ids.write().await.remove(id);
        let removed = self.tasks.write().await.remove(id).is_some();
        self.next_fire.write().await.remove(id);
        removed
    }

    /// Spawn the tick loop on the configured [`RuntimeSpawner`]. Safe to call
    /// repeatedly; an already-owned loop is never replaced or orphaned.
    pub async fn start(self: Arc<Self>) -> Result<(), platform_api::RuntimeError> {
        // PARITY `Y()` → `ie(dir)`: keep the scheduler's runtime files out of
        // `git status` before the first lock/tick.
        if let Some(project_root) =
            crate::tasks_file::project_root_from_tasks_path(&self.tasks_file)
        {
            crate::tasks_file::ensure_runtime_files_excluded(project_root);
        }
        let mut tick_handle = self.tick_handle.lock().await;
        if tick_handle.is_some() {
            return Ok(());
        }
        let registry_key = task_registry_identity(&self.task_registry);
        LIVE_SCHEDULERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(registry_key, Arc::downgrade(&self));
        let me = self.clone();
        let (completed_tx, completed) = tokio::sync::oneshot::channel();
        let handle = match self
            .runtime
            .spawn(
                "cron-tick",
                Box::pin(TickFuture {
                    future: Box::pin(async move {
                        loop {
                            me.tick().await;
                            me.runtime.sleep(SCHEDULER_TICK_INTERVAL).await;
                        }
                    }),
                    _completed: completed_tx,
                }),
            )
            .await
        {
            Ok(handle) => handle,
            Err(error) => {
                LIVE_SCHEDULERS
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&registry_key);
                return Err(error);
            }
        };
        *tick_handle = Some(TickHandle {
            runtime_handle: handle,
            completed,
        });
        Ok(())
    }

    /// PARITY `W()` fire diagnostics: `[ScheduledTasks] firing ${id}${" (recurring)"}`
    /// + `tengu_scheduled_task_fire{recurring, taskId, autonomousLoopDefault}`.
    ///
    /// `recurring` / `prompt` come from the CLAIM, not from `self.tasks`: the
    /// claim has already removed a one-shot (and an aged-out recurring job) from
    /// that map, so reading it back would report every one-shot fire with an
    /// empty prompt and log a recurring job's final run as a one-shot.
    fn log_fire(id: &str, recurring: bool, prompt: &str) {
        tracing::info!(
            "[ScheduledTasks] firing {id}{}",
            if recurring { " (recurring)" } else { "" }
        );
        tracing::info!(
            event = "tengu_scheduled_task_fire",
            recurring,
            task_id = %id,
            autonomous_loop_default = crate::autonomous_loop::is_loop_default_sentinel(prompt),
        );
    }

    async fn process_due_ids(&self, now: SystemTime, due_ids: Vec<String>) {
        if !self.session_cron_enabled {
            return;
        }
        let Some(delivery) = self.session_delivery.read().await.clone() else {
            return;
        };
        if delivery.is_loading().await {
            return;
        }
        self.refresh_project_leader().await;
        for id in due_ids {
            if self
                .pending_missed
                .lock()
                .await
                .iter()
                .any(|task| task.id == id)
            {
                continue;
            }
            if delivery.is_loading().await {
                break;
            }
            let Some(cron) = self
                .tasks
                .read()
                .await
                .get(&id)
                .map(|task| task.schedule.raw.clone())
            else {
                continue;
            };
            let owner = self
                .session_tasks
                .read()
                .await
                .get(&id)
                .and_then(|task| task.owner.clone());
            if let Some(owner) = owner.as_deref() {
                if self
                    .task_registry
                    .get(owner)
                    .await
                    .is_some_and(|task| task.is_terminated())
                {
                    self.unregister_job(&id, None).await;
                    continue;
                }
            }
            let claimed = if self.session_tasks.read().await.contains_key(&id) {
                self.claim_in_memory_due_job(&id, now).await
            } else {
                self.claim_due_job_if_still_due(&id, now).await
            };
            if let Some(claimed_job) = claimed {
                Self::log_fire(&id, claimed_job.recurring, claimed_job.prompt());
                let fire = SessionCronFire {
                    id: id.clone(),
                    cron,
                    prompt: claimed_job.prompt().to_owned(),
                    owner,
                };
                let (_, rollback) = claimed_job.into_parts();
                let result = if let Some(owner) = fire.owner.as_deref() {
                    match platform_api::team_spawn::TeamSpawnSeam::send_message(
                        self.task_registry.as_ref(),
                        owner,
                        fire.prompt,
                    )
                    .await
                    {
                        Ok(()) => Ok(()),
                        Err(
                            platform_api::team_spawn::TeamSpawnError::Terminated
                            | platform_api::team_spawn::TeamSpawnError::StoppedByUser(_),
                        ) => {
                            self.unregister_job(&id, None).await;
                            continue;
                        }
                        Err(error) => Err(error.to_string()),
                    }
                } else {
                    // PARITY `A()`'s two `U(e)` arms. A fire that lands INSIDE
                    // the tick's span vetoes that tick's fold; one that lands
                    // between fires breaks the settled streak, which is the
                    // backward `blocking_system_before_anchor` scan. The loop's
                    // own fire is `I(e)` and ends that scan, so it is excluded.
                    if !crate::autonomous_loop::is_loop_default_sentinel(&fire.prompt) {
                        if let Some(loop_runtime) = delivery.loop_runtime() {
                            if loop_runtime.in_flight_prompt().is_some() {
                                loop_runtime.veto_tick(
                                    crate::autonomous_loop::LoopFoldVeto::BlockingSystemInSpan,
                                );
                            } else {
                                loop_runtime.invalidate_noop_streak();
                            }
                        }
                    }
                    delivery.enqueue(fire).await
                };
                if let Err(error) = result {
                    tracing::error!("cron task {id} enqueue failed: {error}");
                    self.rollback_claim(&id, rollback).await;
                }
            }
        }
    }

    /// Refresh shared durable state before using schedules to select due jobs.
    /// Keep direct registrations and session-only tasks independent of the file.
    ///
    /// PARITY the chokidar watcher (`O.on("add"|"change", () => V(false))`): the
    /// re-read is CHANGE-gated. A tick over an unchanged document costs one
    /// unlocked read instead of the process-global cron lock, the tasks-file
    /// flock, a full parse and a rebuild of the live map — a per-second
    /// exclusive lock that the cron tools and the desktop UI have to queue
    /// behind to write.
    async fn refresh_durable_tasks(&self) {
        let Some(project_root) = crate::tasks_file::project_root_from_tasks_path(&self.tasks_file)
        else {
            return;
        };
        let seen = match crate::tasks_file::read_tasks_body(self.fs.as_ref(), project_root).await {
            Ok(body) => Some(TasksFileSnapshot::Body(body)),
            Err(FsError::NotFound(_)) => Some(TasksFileSnapshot::Absent),
            // Unreadable for some other reason: take the locked path rather
            // than read an unknown state as "unchanged".
            Err(_) => None,
        };
        if let Some(current) = seen.as_ref() {
            if self.applied_snapshot.lock().await.as_ref() == Some(current) {
                return;
            }
        }
        let _process_guard = crate::lock_cron_file().await;
        let Ok(_file_guard) =
            crate::tasks_file::lock_scheduled_tasks(self.fs.as_ref(), project_root).await
        else {
            return;
        };
        // Re-read under the lock: `seen` was taken before it, so a peer write
        // may have landed in between. Recording THIS body as the applied
        // generation is what makes the gate above safe — a write that lands
        // after it must also change the bytes.
        let (snapshot, doc) =
            match crate::tasks_file::read_tasks_body(self.fs.as_ref(), project_root).await {
                Ok(body) => match crate::tasks_file::parse_tasks_strict(&body) {
                    Ok(doc) => (TasksFileSnapshot::Body(body), doc),
                    Err(error) => {
                        tracing::warn!("cron: cannot refresh invalid durable state: {error}");
                        // Remember the bad generation so this warns once per broken
                        // document instead of once per second, and re-reads as soon
                        // as anything rewrites the file.
                        *self.applied_snapshot.lock().await = Some(TasksFileSnapshot::Body(body));
                        return;
                    }
                },
                Err(FsError::NotFound(_)) => (
                    TasksFileSnapshot::Absent,
                    crate::tasks_file::ScheduledTasks::default(),
                ),
                Err(_) => return,
            };
        // Keep identity reconciliation atomic with live registration. Registration
        // releases the tasks lock before taking this lock, so this order is safe.
        let mut durable_ids = self.durable_ids.write().await;
        let previous = durable_ids.clone();
        let session_ids: HashSet<_> = self.session_tasks.read().await.keys().cloned().collect();
        let mut current = HashSet::new();
        let mut tasks = self.tasks.write().await;
        for task in doc.tasks {
            if task.automation.is_some() {
                continue;
            }
            if session_ids.contains(&task.id)
                || (tasks.contains_key(&task.id) && !previous.contains(&task.id))
            {
                continue;
            }
            let Ok(schedule) = parse_cron(&task.cron) else {
                continue;
            };
            let local = tasks.get(&task.id);
            // Same epoch-zero guard as `load_persisted`: an explicit `createdAt:
            // 0` survives the parser, and anchoring it at 1970 ages the task out
            // of `recurring_max_age` the first time it is refreshed.
            let created_at = if task.created_at > 0 {
                SystemTime::UNIX_EPOCH + Duration::from_millis(task.created_at)
            } else {
                local.map_or_else(|| self.clock.now(), |local| local.created_at)
            };
            let definition = CronTaskDef {
                id: task.id.clone(),
                schedule,
                prompt: task.prompt,
                agent_type: local.and_then(|local| local.agent_type.clone()),
                last_run: task
                    .last_fired_at
                    .map(|ms| SystemTime::UNIX_EPOCH + Duration::from_millis(ms)),
                enabled: local.is_none_or(|local| local.enabled),
                created_at,
                recurring: task.recurring.unwrap_or(false),
            };
            current.insert(task.id.clone());
            tasks.insert(task.id, definition);
        }
        for id in previous.difference(&current) {
            tasks.remove(id);
        }
        drop(tasks);
        *durable_ids = current;
        drop(durable_ids);
        *self.applied_snapshot.lock().await = Some(snapshot);
    }

    /// PARITY `M()`'s cache probe: a job's next fire is computed on first sight
    /// (and logged there), then reused until it fires — only the comparison
    /// against `now` runs every tick, not a cron walk plus a host-timezone
    /// lookup per job. Due-detection itself is unchanged, including missed-run
    /// CATCH-UP: a fire time that has passed stays passed, so a run missed
    /// while the scheduler was down is caught up once, and advancing and
    /// persisting the anchor prevents cross-restart re-fires.
    ///
    /// The trailing sweep is the oracle's
    /// `for(let e of I.keys()) if(!h.has(e)) I.delete(e)`.
    async fn due_ids(&self, now: SystemTime) -> Vec<String> {
        let tasks = self.tasks.read().await;
        let mut cache = self.next_fire.write().await;
        let mut due = Vec::new();
        let mut seen: HashSet<&str> = HashSet::with_capacity(tasks.len());
        for task in tasks.values() {
            if !task.enabled {
                continue;
            }
            seen.insert(task.id.as_str());
            let fingerprint = FireFingerprint::of(task);
            let cached = cache
                .get(&task.id)
                .filter(|cached| cached.fingerprint == fingerprint)
                .map(|cached| cached.at);
            let at = match cached {
                Some(at) => at,
                None => {
                    let at = schedule_fire(task);
                    cache.insert(task.id.clone(), CachedFire { fingerprint, at });
                    at
                }
            };
            if at.is_some_and(|at| at <= now) {
                due.push(task.id.clone());
            }
        }
        cache.retain(|id, _| seen.contains(id.as_str()));
        due
    }

    /// PARITY `M()`'s post-fire branch: recompute a fired job's next fire from
    /// the record the claim left behind — a recurring job's advanced
    /// `lastFiredAt`, or the removal of a one-shot — WITHOUT re-logging, since
    /// the oracle logs a schedule only on a cache miss. A job that was not
    /// claimed (a peer held its lock, or the authoritative record was no longer
    /// due) recomputes to the same instant and stays due for the next tick.
    async fn resync_fired(&self, ids: &[String]) {
        let tasks = self.tasks.read().await;
        let mut cache = self.next_fire.write().await;
        for id in ids {
            match tasks.get(id) {
                Some(task) => {
                    let entry = CachedFire {
                        fingerprint: FireFingerprint::of(task),
                        at: next_fire_time(task),
                    };
                    cache.insert(id.clone(), entry);
                }
                None => {
                    cache.remove(id);
                }
            }
        }
    }

    async fn settle_automation_flight(
        &self,
        root: &Path,
        flight: &AutomationFlight,
    ) -> Result<(), String> {
        let needs_shutdown = matches!(
            *flight
                .commit
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            AutomationCommit::AwaitingOutcome
        );
        if needs_shutdown {
            // A dropped/panicked producer may have a supervised native runtime.
            // Confirm it has released writers before committing Interrupted.
            flight.firer.cancel_run(&flight.request.run_id).await?;
            *flight
                .commit
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = AutomationCommit::Pending {
                result: Err(format!(
                    "{}Execution ended before its result was confirmed",
                    crate::AUTOMATION_INTERRUPTED_PREFIX
                )),
                finished_at: unix_epoch_ms(self.clock.now()),
            };
        }
        persist_automation_outcome(self.fs.as_ref(), root, &flight.request, &flight.commit).await
    }

    async fn dispatch_automations(&self) {
        let Some(firer) = self.automation_firer.read().await.clone() else {
            return;
        };
        let Some(root) = crate::project_root_from_tasks_path(&self.tasks_file) else {
            return;
        };
        // Stop takes this lock before cancelling the tick, so every successful
        // spawn has a tracked destruction barrier before shutdown can begin.
        let mut flights = self.automation_runs.lock().await;
        let mut settled = Vec::new();
        for (id, flight) in flights.iter_mut() {
            if !flight.is_running() {
                match self.settle_automation_flight(root, flight).await {
                    Ok(()) => settled.push(id.clone()),
                    Err(error) => {
                        tracing::warn!(run_id = %id, %error, "Retaining scheduled outcome for persistence retry")
                    }
                }
            }
        }
        for id in settled {
            flights.remove(&id);
        }
        let Ok(body) = crate::tasks_file::read_automation_tasks_body(self.fs.as_ref(), root).await
        else {
            return;
        };
        for task in crate::parse_tasks(&body).tasks {
            if task.automation.is_none() {
                continue;
            }
            let Some(request) = crate::claim_automation_run(
                self.fs.as_ref(),
                root,
                &task.id,
                self.clock.now(),
                None,
            )
            .await
            else {
                continue;
            };
            let fs = self.fs.clone();
            let clock = self.clock.clone();
            let root_owned = root.to_path_buf();
            let executor = firer.clone();
            let claimed = request.clone();
            let (completed_tx, completed) = tokio::sync::oneshot::channel();
            let commit = Arc::new(StdMutex::new(AutomationCommit::AwaitingOutcome));
            let produced_commit = commit.clone();
            let future = TickFuture {
                future: Box::pin(async move {
                    let result = executor.fire_automation(&claimed).await;
                    *produced_commit
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        AutomationCommit::Pending {
                            result,
                            finished_at: unix_epoch_ms(clock.now()),
                        };
                    if let Err(error) = persist_automation_outcome(
                        fs.as_ref(),
                        &root_owned,
                        &claimed,
                        &produced_commit,
                    )
                    .await
                    {
                        tracing::warn!(run_id = %claimed.run_id, %error, "Scheduled result needs persistence retry");
                    }
                }),
                _completed: completed_tx,
            };
            match self
                .runtime
                .spawn("cron-automation", Box::pin(future))
                .await
            {
                Ok(runtime_handle) => {
                    flights.insert(
                        request.run_id.clone(),
                        AutomationFlight {
                            request,
                            handle: Some(TickHandle {
                                runtime_handle,
                                completed,
                            }),
                            commit,
                            firer: firer.clone(),
                        },
                    );
                }
                Err(error) => {
                    *commit
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        AutomationCommit::Pending {
                            result: Err(format!(
                                "{}Could not start execution: {error}",
                                crate::AUTOMATION_INTERRUPTED_PREFIX
                            )),
                            finished_at: unix_epoch_ms(self.clock.now()),
                        };
                    let flight = AutomationFlight {
                        request,
                        handle: None,
                        commit,
                        firer: firer.clone(),
                    };
                    if self.settle_automation_flight(root, &flight).await.is_err() {
                        flights.insert(flight.request.run_id.clone(), flight);
                    }
                }
            }
        }
    }

    async fn prune_orphan_session_jobs(&self) {
        let Some(delivery) = self.session_delivery.read().await.clone() else {
            return;
        };
        if delivery.is_loading().await {
            return;
        }
        let jobs = self.session_jobs().await;
        for job in jobs {
            if let Some(owner) = job.owner.as_deref() {
                if self
                    .task_registry
                    .get(owner)
                    .await
                    .is_none_or(|task| task.is_terminated())
                {
                    self.unregister_job(&job.id, None).await;
                }
            }
        }
    }

    async fn tick(&self) {
        self.dispatch_automations().await;
        if !self.session_cron_enabled {
            return;
        }
        let pending = std::mem::take(&mut *self.pending_missed.lock().await);
        if !pending.is_empty() {
            if let Some(root) = crate::tasks_file::project_root_from_tasks_path(&self.tasks_file) {
                self.surface_missed_one_shots(root, pending).await;
            }
        }
        self.refresh_durable_tasks().await;
        self.prune_orphan_session_jobs().await;
        let now = self.clock.now();
        let due_ids = self.due_ids(now).await;
        if due_ids.is_empty() {
            return;
        }
        self.process_due_ids(now, due_ids.clone()).await;
        self.resync_fired(&due_ids).await;
    }

    /// Cancel and join the tick future, if running. The owned completion
    /// barrier remains available if this waiter is cancelled or cancellation
    /// fails; concurrent start/stop calls cannot bypass it.
    pub async fn stop(&self) -> Result<(), platform_api::RuntimeError> {
        let mut tick_handle = self.tick_handle.lock().await;
        let mut flights =
            tokio::time::timeout(TICK_DESTRUCTION_BUDGET, self.automation_runs.lock())
                .await
                .map_err(|_| {
                    platform_api::RuntimeError::Internal(
                        "scheduled dispatch did not finish within the shutdown budget".into(),
                    )
                })?;
        if let Some(tick) = tick_handle.as_mut() {
            self.runtime.cancel(&tick.runtime_handle).await?;
            // Channel closure, not a sent value, proves TickFuture and all its
            // captured owners were destroyed. Retain the receiver across await.
            //
            // Bounded, because host shutdown awaits this: a spawner whose
            // `cancel` acknowledges without dropping the future would park the
            // whole barrier. On expiry the handle is retained so a retry can
            // join again; reporting a stop that did not happen would be worse
            // than reporting the timeout.
            if tokio::time::timeout(TICK_DESTRUCTION_BUDGET, &mut tick.completed)
                .await
                .is_err()
            {
                return Err(platform_api::RuntimeError::Internal(
                    "cron tick did not release its owners within the shutdown budget".into(),
                ));
            }
        }
        for flight in flights.values_mut() {
            if let Some(handle) = flight.handle.as_mut() {
                if matches!(
                    handle.completed.try_recv(),
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty)
                ) {
                    if let Err(error) = self.runtime.cancel(&handle.runtime_handle).await {
                        if matches!(
                            handle.completed.try_recv(),
                            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
                        ) {
                            return Err(error);
                        }
                    }
                    if tokio::time::timeout(TICK_DESTRUCTION_BUDGET, &mut handle.completed)
                        .await
                        .is_err()
                    {
                        return Err(platform_api::RuntimeError::Internal(
                            "scheduled execution did not release its owners within the shutdown budget".into()));
                    }
                }
            }
            if let Some(root) = crate::project_root_from_tasks_path(&self.tasks_file) {
                self.settle_automation_flight(root, flight)
                    .await
                    .map_err(|error| {
                        platform_api::RuntimeError::Internal(format!(
                            "Scheduled result was not safely persisted: {error}"
                        ))
                    })?;
            }
        }
        flights.clear();
        *tick_handle = None;
        let key = task_registry_identity(&self.task_registry);
        LIVE_SCHEDULERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&key);
        if let Some(root) = crate::tasks_file::project_root_from_tasks_path(&self.tasks_file) {
            crate::lock::release_scheduler_lease(self.fs.as_ref(), root, &self.lease_identity())
                .await;
        }
        self.project_leader.store(false, Ordering::SeqCst);
        self.pending_missed.lock().await.clear();
        self.session_tasks.write().await.clear();
        self.durable_ids.write().await.clear();
        self.tasks.write().await.clear();
        self.next_fire.write().await.clear();
        *self.applied_snapshot.lock().await = None;
        // `main` returned the cancel result from here; this branch propagates
        // the same error at the `cancel` call above and additionally waits for
        // the tick future to be destroyed, so reaching this line means both
        // succeeded.
        Ok(())
    }

    async fn claim_due_job_if_still_due(
        &self,
        id: &str,
        now: SystemTime,
    ) -> Option<ClaimedCronJob> {
        let local = { self.tasks.read().await.get(id).cloned()? };

        let Some(project_root) = crate::tasks_file::project_root_from_tasks_path(&self.tasks_file)
        else {
            return self.claim_in_memory_due_job(id, now).await;
        };

        let _process_guard = crate::lock_cron_file().await;
        let Ok(_file_guard) =
            crate::tasks_file::lock_scheduled_tasks(self.fs.as_ref(), project_root).await
        else {
            tracing::warn!("cron job {id} failed to lock scheduled tasks file; skipping");
            return None;
        };

        let body = match crate::tasks_file::read_tasks_body(self.fs.as_ref(), project_root).await {
            Ok(body) => body,
            Err(FsError::NotFound(_)) => return self.claim_missing_disk_job(id, now).await,
            Err(error) => {
                // An unreadable durable state file is not evidence that this is
                // an in-memory-only job. Fail closed or a transient I/O error
                // can make multiple schedulers execute the same stale record.
                tracing::warn!(
                    "cron job {id} could not read authoritative state: {error}; skipping"
                );
                return None;
            }
        };

        let doc = match crate::tasks_file::parse_tasks_strict(&body) {
            Ok(doc) => doc,
            Err(error) => {
                // User-facing listing deliberately treats malformed JSON as an
                // empty document. Execution claims cannot: doing so would make
                // a stale durable job look in-memory-only and execute it.
                tracing::warn!("cron job {id} has invalid authoritative state: {error}; skipping");
                return None;
            }
        };
        let Some(on_disk) = doc.tasks.into_iter().find(|task| task.id == id) else {
            return self.claim_missing_disk_job(id, now).await;
        };

        if on_disk.automation.is_some() || !self.owns_durable_task(&on_disk.creator) {
            return None;
        }
        if on_disk
            .expires_at
            .is_some_and(|expiry| expiry <= unix_epoch_ms(now))
        {
            if let Some(updated) = tasks_file_without(&body, id) {
                if crate::tasks_file::write_tasks_body(self.fs.as_ref(), project_root, &updated)
                    .await
                    .is_ok()
                {
                    self.unregister_job(id, None).await;
                }
            }
            return None;
        }

        let authoritative = match parse_cron(&on_disk.cron) {
            Ok(schedule) => CronTaskDef {
                id: on_disk.id,
                schedule,
                prompt: on_disk.prompt,
                agent_type: local.agent_type,
                last_run: on_disk
                    .last_fired_at
                    .map(|ms| SystemTime::UNIX_EPOCH + Duration::from_millis(ms)),
                enabled: local.enabled,
                created_at: SystemTime::UNIX_EPOCH + Duration::from_millis(on_disk.created_at),
                recurring: on_disk.recurring.unwrap_or(false),
            },
            Err(e) => {
                tracing::warn!("cron job {id} has invalid persisted schedule during claim: {e}");
                return None;
            }
        };

        if !is_job_due(&authoritative, now) {
            self.sync_task_from_authoritative(&authoritative).await;
            return None;
        }

        // Claude Code checks recurringMaxAge only after a due job has entered
        // the fire path. An aged job therefore gets one final fire; an aged job
        // whose next run has not arrived remains registered.
        let expires_after_fire = on_disk.permanent != Some(true)
            && is_recurring_task_aged(
                now,
                authoritative.created_at,
                authoritative.recurring,
                self.recurring_max_age,
            );

        let updated = if authoritative.recurring && !expires_after_fire {
            tasks_file_with_last_fired(&body, id, unix_epoch_ms(now))
        } else {
            tasks_file_without(&body, id)
        };
        let Some(updated) = updated else {
            return None;
        };
        if let Err(e) =
            crate::tasks_file::write_tasks_body(self.fs.as_ref(), project_root, &updated).await
        {
            tracing::warn!("cron job {id} failed to persist claimed run: {e}");
            return None;
        }

        self.apply_claimed_task_state(&authoritative, now, expires_after_fire)
            .await
            .map(|spawn_input| ClaimedCronJob {
                spawn_input,
                recurring: authoritative.recurring,
                rollback: ClaimRollback::Durable {
                    project_root: project_root.to_path_buf(),
                    authoritative,
                    before_body: body,
                    claimed_body: updated,
                },
            })
    }

    async fn claim_missing_disk_job(&self, id: &str, now: SystemTime) -> Option<ClaimedCronJob> {
        let was_durable = self.durable_ids.write().await.remove(id);
        if was_durable {
            self.tasks.write().await.remove(id);
            return None;
        }
        self.claim_in_memory_due_job(id, now).await
    }

    async fn claim_in_memory_due_job(&self, id: &str, now: SystemTime) -> Option<ClaimedCronJob> {
        let mut tasks = self.tasks.write().await;
        let original_task = tasks.get(id).cloned()?;
        let prompt = {
            let task = tasks.get(id)?;
            if !is_job_due(task, now) {
                return None;
            }
            task.prompt.clone()
        };
        let expires_after_fire = tasks.get(id).is_some_and(|task| {
            is_recurring_task_aged(now, task.created_at, task.recurring, self.recurring_max_age)
        });
        let remove_after_fire = finalize_fired_job(&mut tasks, id, now) || expires_after_fire;
        if expires_after_fire {
            tasks.remove(id);
        }
        let mut session_tasks = self.session_tasks.write().await;
        let original_session_task = session_tasks.get(id).cloned();
        if remove_after_fire {
            session_tasks.remove(id);
        } else if let Some(task) = session_tasks.get_mut(id) {
            task.last_fired_at = Some(now);
        }
        let claimed_task = tasks.get(id).cloned();
        let claimed_session_task = session_tasks.get(id).cloned();
        if expires_after_fire {
            tracing::info!(
                event = "tengu_scheduled_task_expired",
                cron_id = %id,
                "cron job fired its final run after exceeding the recurring max age"
            );
        }
        Some(ClaimedCronJob {
            spawn_input: TaskSpawnInput::Dream {
                prompt,
                max_iterations: None,
            },
            recurring: original_task.recurring,
            rollback: ClaimRollback::Session {
                before_task: original_task,
                before_session_task: original_session_task,
                claimed_task,
                claimed_session_task,
            },
        })
    }

    async fn sync_task_from_authoritative(&self, authoritative: &CronTaskDef) {
        let mut tasks = self.tasks.write().await;
        if let Some(task) = tasks.get_mut(&authoritative.id) {
            *task = authoritative.clone();
        }
    }

    async fn apply_claimed_task_state(
        &self,
        authoritative: &CronTaskDef,
        now: SystemTime,
        expires_after_fire: bool,
    ) -> Option<TaskSpawnInput> {
        let mut tasks = self.tasks.write().await;
        tasks.insert(authoritative.id.clone(), authoritative.clone());
        let remove_after_fire =
            finalize_fired_job(&mut tasks, &authoritative.id, now) || expires_after_fire;
        if expires_after_fire {
            tasks.remove(&authoritative.id);
        }
        if !remove_after_fire {
            if let Some(task) = tasks.get_mut(&authoritative.id) {
                *task = authoritative.clone();
                task.last_run = Some(now);
            }
        }
        drop(tasks);
        if remove_after_fire {
            self.durable_ids.write().await.remove(&authoritative.id);
        }
        if expires_after_fire {
            tracing::info!(
                event = "tengu_scheduled_task_expired",
                cron_id = %authoritative.id,
                "cron job fired its final run after exceeding the recurring max age"
            );
        }
        Some(TaskSpawnInput::Dream {
            prompt: authoritative.prompt.clone(),
            max_iterations: None,
        })
    }

    async fn rollback_claim(&self, id: &str, rollback: ClaimRollback) {
        match rollback {
            ClaimRollback::Session {
                before_task,
                before_session_task,
                claimed_task,
                claimed_session_task,
            } => {
                self.restore_session_claim(
                    id,
                    before_task,
                    before_session_task,
                    claimed_task,
                    claimed_session_task,
                )
                .await;
            }
            ClaimRollback::Durable {
                project_root,
                authoritative,
                before_body,
                claimed_body,
            } => {
                self.rollback_durable_claim(
                    id,
                    &project_root,
                    authoritative,
                    &before_body,
                    &claimed_body,
                )
                .await;
            }
        }
    }

    async fn restore_session_claim(
        &self,
        id: &str,
        before_task: CronTaskDef,
        before_session_task: Option<SessionCronTask>,
        claimed_task: Option<CronTaskDef>,
        claimed_session_task: Option<SessionCronTask>,
    ) {
        let mut tasks = self.tasks.write().await;
        let mut session_tasks = self.session_tasks.write().await;

        let task_unchanged = cron_task_option_matches(tasks.get(id), claimed_task.as_ref());
        let session_unchanged = session_tasks.get(id) == claimed_session_task.as_ref();
        if !task_unchanged || !session_unchanged {
            tracing::warn!("cron job {id} skip session rollback because state changed after claim");
            return;
        }

        tasks.insert(id.to_string(), before_task);
        if let Some(session_task) = before_session_task {
            session_tasks.insert(id.to_string(), session_task);
        } else {
            session_tasks.remove(id);
        }
    }

    async fn rollback_durable_claim(
        &self,
        id: &str,
        project_root: &Path,
        authoritative: CronTaskDef,
        before_body: &str,
        claimed_body: &str,
    ) {
        let _process_guard = crate::lock_cron_file().await;
        let Ok(_file_guard) =
            crate::tasks_file::lock_scheduled_tasks(self.fs.as_ref(), project_root).await
        else {
            tracing::warn!("cron job {id} failed to re-lock scheduled tasks during rollback");
            return;
        };

        let current_body =
            match crate::tasks_file::read_tasks_body(self.fs.as_ref(), project_root).await {
                Ok(body) => body,
                Err(error) => {
                    tracing::warn!(
                        "cron job {id} failed to read scheduled tasks during rollback: {error}"
                    );
                    return;
                }
            };
        if current_body != claimed_body {
            tracing::warn!(
                "cron job {id} skip rollback because authoritative state changed after claim"
            );
            return;
        }
        if let Err(error) =
            crate::tasks_file::write_tasks_body(self.fs.as_ref(), project_root, before_body).await
        {
            tracing::warn!(
                "cron job {id} failed to restore scheduled tasks after spawn error: {error}"
            );
            return;
        }

        self.restore_authoritative_task(authoritative).await;
    }

    async fn restore_authoritative_task(&self, authoritative: CronTaskDef) {
        self.durable_ids
            .write()
            .await
            .insert(authoritative.id.clone());
        self.tasks
            .write()
            .await
            .insert(authoritative.id.clone(), authoritative);
    }
}

fn cron_task_option_matches(left: Option<&CronTaskDef>, right: Option<&CronTaskDef>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            left.id == right.id
                && left.schedule.raw == right.schedule.raw
                && left.prompt == right.prompt
                && left.agent_type == right.agent_type
                && left.last_run == right.last_run
                && left.enabled == right.enabled
                && left.created_at == right.created_at
                && left.recurring == right.recurring
        }
        _ => false,
    }
}

fn unix_epoch_ms(at: SystemTime) -> u64 {
    at.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Detect whether `pid` corresponds to a live process. Used by
/// [`crate::lock::try_acquire_lock`] to override stale lock files left behind
/// by crashed schedulers (A9).
#[cfg(unix)]
#[allow(unsafe_code)]
#[must_use]
pub fn pid_alive_check(pid: u32) -> bool {
    // `kill(0, 0)` addresses the caller's whole process group, not a process, so
    // a recorded pid of 0 is never a live owner. PID 1 IS a real process — the
    // engine is PID 1 in a container — so it must not be lumped in here, or the
    // scheduler lease and creator ownership both read a live leader as dead.
    if pid == 0 {
        return false;
    }
    // SAFETY: kill(pid, 0) tests existence without delivering a signal.
    // The libc function takes only POD args; there are no aliasing concerns,
    // and any failure is treated as not alive (upstream gi catches all errors).
    #[allow(clippy::cast_possible_wrap)]
    unsafe {
        libc::kill(pid as libc::pid_t, 0) == 0
    }
}

/// Detect whether `pid` corresponds to a live process (Windows variant).
#[cfg(windows)]
#[allow(unsafe_code)]
#[must_use]
pub fn pid_alive_check(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    // SAFETY: OpenProcess returns a HANDLE (pointer-sized) we immediately
    // close. We do not dereference or share the handle; passing FALSE (0)
    // for bInheritHandle and a known constant for access flags is well-defined.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            false
        } else {
            CloseHandle(handle);
            true
        }
    }
}

#[cfg(test)]
mod expiry_tests {
    use super::{
        deterministic_id_fraction, is_recurring_task_aged, next_fire_with, CronJitterConfig,
        DEFAULT_RECURRING_MAX_AGE,
    };
    use std::time::{Duration, SystemTime};

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    /// `n` days after the UNIX epoch.
    fn at(days: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(days * 24 * 60 * 60)
    }

    #[test]
    fn explicit_seven_day_age_preset() {
        assert_eq!(DEFAULT_RECURRING_MAX_AGE, DAY * 7);
    }

    #[test]
    fn recurring_job_past_max_age_is_aged() {
        // created day 0, now day 31, max 30d → aged.
        assert!(is_recurring_task_aged(at(31), at(0), true, Some(DAY * 30)));
    }

    #[test]
    fn recurring_job_at_exact_boundary_is_aged() {
        // age == max_age → aged (`>=`, matching claude-code's `nowMs - createdAt >= maxAgeMs`).
        assert!(is_recurring_task_aged(at(30), at(0), true, Some(DAY * 30)));
    }

    #[test]
    fn fresh_recurring_job_is_not_aged() {
        assert!(!is_recurring_task_aged(at(5), at(0), true, Some(DAY * 30)));
    }

    #[test]
    fn one_shot_job_is_never_aged_by_max_age() {
        // !recurring → never aged out here (one-shot auto-deletes after firing).
        assert!(!is_recurring_task_aged(
            at(100),
            at(0),
            false,
            Some(DAY * 30)
        ));
    }

    #[test]
    fn none_max_age_disables_expiry() {
        assert!(!is_recurring_task_aged(at(10_000), at(0), true, None));
    }

    #[test]
    fn zero_max_age_disables_expiry_like_2_1_270() {
        assert!(!is_recurring_task_aged(
            at(10_000),
            at(0),
            true,
            Some(std::time::Duration::ZERO)
        ));
    }

    #[test]
    fn future_created_at_is_not_aged() {
        // Clock skew: created_at after now → duration_since errs → treated as not aged.
        assert!(!is_recurring_task_aged(at(0), at(5), true, Some(DAY * 30)));
    }

    #[test]
    fn finalize_one_shot_removes_recurring_keeps() {
        use super::{finalize_fired_job, CronTaskDef};
        use crate::schedule::parse_cron;
        use std::collections::HashMap;

        let mk = |id: &str, recurring: bool| CronTaskDef {
            id: id.into(),
            schedule: parse_cron("* * * * *").unwrap(),
            prompt: "p".into(),
            agent_type: None,
            last_run: None,
            enabled: true,
            created_at: at(0),
            recurring,
        };
        let mut tasks = HashMap::new();
        tasks.insert("rec".to_string(), mk("rec", true));
        tasks.insert("once".to_string(), mk("once", false));

        // One-shot: removed, returns true (caller deletes the descriptor).
        assert!(finalize_fired_job(&mut tasks, "once", at(1)));
        assert!(!tasks.contains_key("once"));

        // Recurring: kept, last_run recorded, returns false.
        assert!(!finalize_fired_job(&mut tasks, "rec", at(1)));
        assert_eq!(tasks.get("rec").unwrap().last_run, Some(at(1)));

        // Missing id: no-op false.
        assert!(!finalize_fired_job(&mut tasks, "ghost", at(1)));
    }

    #[test]
    fn tasks_file_with_last_fired_sets_ms_and_preserves_other_tasks() {
        use super::tasks_file_with_last_fired;
        // Two tasks; set lastFiredAt (ms) on the second only.
        let orig = r#"{"tasks":[
            {"id":"a","cron":"0 9 * * *","prompt":"pa","createdAt":100,"recurring":true},
            {"id":"j","cron":"0 9 * * *","prompt":"p","createdAt":200,"recurring":true,"permanent":true}
        ]}"#;
        let updated = tasks_file_with_last_fired(orig, "j", 555_000).unwrap();
        let doc = crate::tasks_file::parse_tasks(&updated);
        let a = doc.tasks.iter().find(|t| t.id == "a").unwrap();
        let j = doc.tasks.iter().find(|t| t.id == "j").unwrap();
        // The targeted task gets lastFiredAt in MS; the other is untouched.
        assert_eq!(j.last_fired_at, Some(555_000));
        assert_eq!(a.last_fired_at, None);
        // Unrelated fields preserved (incl. permanent).
        assert_eq!(j.created_at, 200);
        assert_eq!(j.permanent, Some(true));
        // Serialized form is camelCase with a trailing newline.
        assert!(updated.contains("\"lastFiredAt\": 555000"));
        assert!(updated.ends_with("}\n"));
        // Missing id → None (caller leaves the file untouched).
        assert!(tasks_file_with_last_fired(orig, "ghost", 1).is_none());
        assert!(tasks_file_with_last_fired("not json", "j", 1).is_none());
    }

    #[test]
    fn tasks_file_without_drops_only_the_target() {
        use super::tasks_file_without;
        let orig = r#"{"tasks":[
            {"id":"a","cron":"* * * * *","prompt":"pa","createdAt":1},
            {"id":"b","cron":"* * * * *","prompt":"pb","createdAt":2}
        ]}"#;
        let updated = tasks_file_without(orig, "a").unwrap();
        let doc = crate::tasks_file::parse_tasks(&updated);
        assert_eq!(doc.tasks.len(), 1);
        assert_eq!(doc.tasks[0].id, "b");
        // Missing id → None.
        assert!(tasks_file_without(orig, "ghost").is_none());
        assert!(tasks_file_without("not json", "a").is_none());
    }

    #[test]
    fn is_job_due_fires_live_catches_up_and_never_double_fires() {
        use super::{is_job_due_with, CronTaskDef};
        use crate::schedule::parse_cron;

        let sec = |s: u64| SystemTime::UNIX_EPOCH + Duration::from_secs(s);
        let nine_am = sec(1_700_038_800); // 2023-11-15 09:00 UTC
        let eight_am = sec(1_700_035_200); // same day 08:00
        let ten_am = sec(1_700_042_400); // same day 10:00
        let yest_nine = sec(1_700_038_800 - 86_400); // 2023-11-14 09:00

        let mk =
            |last_run: Option<SystemTime>, created_at: SystemTime, enabled: bool| CronTaskDef {
                id: "j".into(),
                schedule: parse_cron("0 9 * * *").unwrap(),
                prompt: "p".into(),
                agent_type: None,
                last_run,
                enabled,
                created_at,
                recurring: true,
            };

        // UTC (offset 0) for deterministic assertions; the live `is_job_due`
        // resolves the offset per-instant via the system timezone.
        let utc = |_: u64| 0_i64;
        // LIVE: last fired yesterday 09:00, now today 09:00 → due.
        assert!(is_job_due_with(
            &mk(Some(yest_nine), eight_am, true),
            nine_am,
            utc
        ));
        // Before the scheduled minute (now 08:00) → not due.
        assert!(!is_job_due_with(
            &mk(Some(yest_nine), eight_am, true),
            eight_am,
            utc
        ));
        // CATCH-UP: never fired, created 08:00, now 10:00 (missed 09:00) → due.
        assert!(is_job_due_with(&mk(None, eight_am, true), ten_am, utc));
        // NO DOUBLE-FIRE: just fired at 09:00, still 09:00 → next run tomorrow → not due.
        assert!(!is_job_due_with(
            &mk(Some(nine_am), eight_am, true),
            nine_am,
            utc
        ));
        // Per-task disabled → never due.
        assert!(!is_job_due_with(
            &mk(Some(yest_nine), eight_am, false),
            nine_am,
            utc
        ));
    }

    #[test]
    fn deterministic_fraction_matches_js_parse_int_prefix() {
        let close = |actual: f64, expected: f64| (actual - expected).abs() < f64::EPSILON;
        assert!(close(deterministic_id_fraction("00000000"), 0.0));
        assert!(close(deterministic_id_fraction("80000000"), 0.5));
        assert!(close(deterministic_id_fraction("80000000suffix"), 0.5));
        assert!(close(deterministic_id_fraction("not-hex"), 0.0));
        assert!(
            close(
                deterministic_id_fraction("aZ-not-all-hex"),
                10.0 / 4_294_967_296.0,
            ),
            "JavaScript parseInt stops at the first non-hex digit"
        );
    }

    #[test]
    fn recurring_jitter_is_deterministic_and_capped_at_30_minutes() {
        use super::CronTaskDef;
        use crate::schedule::parse_cron;

        let eight_am = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_035_200);
        let nine_thirty = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_040_600);
        let task = CronTaskDef {
            id: "80000000".into(),
            schedule: parse_cron("0 9 * * *").unwrap(),
            prompt: "p".into(),
            agent_type: None,
            last_run: None,
            enabled: true,
            created_at: eight_am,
            recurring: true,
        };

        // Hash fraction 0.5 × recurring fraction 0.5 × 24h = 6h, capped to 30m.
        assert_eq!(
            next_fire_with(&task, &|_| 0, CronJitterConfig::default()),
            Some(nine_thirty)
        );
        assert_eq!(
            next_fire_with(&task, &|_| 0, CronJitterConfig::default()),
            Some(nine_thirty),
            "the same id and schedule always produce the same fire time"
        );
    }

    #[test]
    fn recurring_five_minute_schedule_uses_15_second_cache_lead() {
        use super::CronTaskDef;
        use crate::schedule::parse_cron;

        let anchor = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_035_200);
        let task = CronTaskDef {
            id: "ffffffff".into(),
            schedule: parse_cron("*/5 * * * *").unwrap(),
            prompt: "p".into(),
            agent_type: None,
            last_run: None,
            enabled: true,
            created_at: anchor,
            recurring: true,
        };

        assert_eq!(
            next_fire_with(&task, &|_| 0, CronJitterConfig::default()),
            anchor.checked_add(Duration::from_secs(4 * 60 + 45)),
            "cache lead takes precedence over ordinary recurring jitter"
        );
    }

    #[test]
    fn one_shot_half_hour_jitter_is_early_and_clamped_to_creation() {
        use super::CronTaskDef;
        use crate::schedule::parse_cron;

        let eight_am = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_035_200);
        let eight_fifty_nine_fifteen = eight_am + Duration::from_secs(59 * 60 + 15);
        let mut task = CronTaskDef {
            id: "80000000".into(),
            schedule: parse_cron("0 * * * *").unwrap(),
            prompt: "p".into(),
            agent_type: None,
            last_run: None,
            enabled: true,
            created_at: eight_am,
            recurring: false,
        };

        assert_eq!(
            next_fire_with(&task, &|_| 0, CronJitterConfig::default()),
            Some(eight_fifty_nine_fifteen),
            "hash fraction 0.5 pulls a half-hour-boundary one-shot forward 45s"
        );

        let created_too_late = eight_am + Duration::from_secs(59 * 60 + 40);
        task.created_at = created_too_late;
        assert_eq!(
            next_fire_with(&task, &|_| 0, CronJitterConfig::default()),
            Some(created_too_late),
            "early jitter never schedules before the task was created"
        );
    }

    #[test]
    fn one_shot_non_half_hour_minute_is_not_jittered() {
        use super::CronTaskDef;
        use crate::schedule::parse_cron;

        let eight_oh_six = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_035_560);
        let nine_oh_five = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_039_100);
        let task = CronTaskDef {
            id: "ffffffff".into(),
            schedule: parse_cron("5 * * * *").unwrap(),
            prompt: "p".into(),
            agent_type: None,
            last_run: None,
            enabled: true,
            created_at: eight_oh_six,
            recurring: false,
        };

        assert_eq!(
            next_fire_with(&task, &|_| 0, CronJitterConfig::default()),
            Some(nine_oh_five)
        );
    }
}

#[cfg(test)]
mod scheduler_tick_tests {
    use super::{CronScheduler, SessionCronTask};
    use async_trait::async_trait;
    use futures::Stream;
    use platform_api::filesystem::{FileContent, FileEvent, FlockGuard, FsError};
    use platform_api::{BackgroundTaskHandle, Clock, FileSystem, RuntimeError, RuntimeSpawner};
    use std::collections::HashMap;
    use std::future::Future;
    use std::path::{Path, PathBuf};
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};
    use tasks::output_manager::TaskOutputManager;
    use tasks::registry::TaskRegistry;
    use tasks::task_trait::{Task, TaskContext, TaskError, TaskHandle};
    use tasks::TaskSpawnInput;
    #[cfg(test)]
    use tasks::TaskType;

    const NOW: u64 = 1_700_000_000;
    const TASKS_PATH: &str = "/proj/.claude/scheduled_tasks.json";
    const AUTOMATION_PATH: &str = "/proj/.lingxi/scheduled_tasks.json";
    const OUTPUT_DIR: &str = "/proj/task-output";

    struct MemFs {
        files: tokio::sync::Mutex<HashMap<String, String>>,
        fail_writes: AtomicUsize,
    }

    struct MemFlockGuard(String);

    impl FlockGuard for MemFlockGuard {
        fn path(&self) -> &str {
            &self.0
        }
    }

    impl MemFs {
        fn with(path: &str, body: &str) -> Arc<Self> {
            let mut files = HashMap::new();
            files.insert(path.to_string(), body.to_string());
            Arc::new(Self {
                files: tokio::sync::Mutex::new(files),
                fail_writes: AtomicUsize::new(0),
            })
        }

        async fn get(&self, path: &str) -> Option<String> {
            self.files.lock().await.get(path).cloned()
        }
    }

    #[async_trait]
    impl FileSystem for MemFs {
        async fn read_file(
            &self,
            path: &str,
            _offset: Option<u64>,
            _limit: Option<u64>,
        ) -> Result<FileContent, FsError> {
            match self.files.lock().await.get(path) {
                Some(content) => Ok(FileContent {
                    content: content.clone(),
                    truncated: false,
                    total_lines: content.lines().count() as u64,
                }),
                None => Err(FsError::NotFound(path.to_string())),
            }
        }

        async fn write_file(&self, path: &str, content: &str) -> Result<(), FsError> {
            if path == AUTOMATION_PATH
                && self
                    .fail_writes
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                        remaining.checked_sub(1)
                    })
                    .is_ok()
            {
                return Err(FsError::Io("injected atomic write failure".into()));
            }
            self.files
                .lock()
                .await
                .insert(path.to_string(), content.to_string());
            Ok(())
        }

        fn is_within_workspace(&self, _: &str) -> bool {
            true
        }

        async fn watch(
            &self,
            _: &str,
        ) -> Result<Pin<Box<dyn Stream<Item = FileEvent> + Send>>, FsError> {
            Err(FsError::Io("unsupported".into()))
        }

        async fn append_file(&self, path: &str, content: &str) -> Result<(), FsError> {
            self.files
                .lock()
                .await
                .entry(path.to_string())
                .or_default()
                .push_str(content);
            Ok(())
        }

        async fn truncate(&self, _: &str, _: u64) -> Result<(), FsError> {
            Ok(())
        }

        async fn file_mtime(&self, _: &str) -> Result<SystemTime, FsError> {
            Ok(SystemTime::UNIX_EPOCH)
        }

        async fn file_size(&self, path: &str) -> Result<u64, FsError> {
            Ok(self
                .files
                .lock()
                .await
                .get(path)
                .map_or(0, |content| content.len() as u64))
        }

        async fn delete_file(&self, path: &str) -> Result<(), FsError> {
            self.files.lock().await.remove(path);
            Ok(())
        }

        async fn symlink(&self, _: &str, _: &str) -> Result<(), FsError> {
            Ok(())
        }

        async fn flock_exclusive(&self, _: &str) -> Result<Box<dyn FlockGuard>, FsError> {
            Err(FsError::Io("unsupported".into()))
        }

        async fn flock_exclusive_rooted(
            &self,
            root: &Path,
            relative: &Path,
        ) -> Result<Box<dyn FlockGuard>, FsError> {
            Ok(Box::new(MemFlockGuard(
                root.join(relative).display().to_string(),
            )))
        }

        async fn fsync(&self, _: &str) -> Result<(), FsError> {
            Ok(())
        }
    }

    struct FixedClock(SystemTime);

    impl FixedClock {
        fn at_secs(secs: u64) -> Arc<Self> {
            Arc::new(Self(SystemTime::UNIX_EPOCH + Duration::from_secs(secs)))
        }
    }

    impl Clock for FixedClock {
        fn now(&self) -> SystemTime {
            self.0
        }
    }

    struct UnusedRuntime;

    #[derive(Default)]
    struct DeferredCancelRuntime {
        future: std::sync::Mutex<Option<Pin<Box<dyn Future<Output = ()> + Send>>>>,
        cancelled: tokio::sync::Notify,
    }

    #[async_trait]
    impl RuntimeSpawner for DeferredCancelRuntime {
        async fn spawn(
            &self,
            name: &str,
            task: Pin<Box<dyn Future<Output = ()> + Send>>,
        ) -> Result<BackgroundTaskHandle, RuntimeError> {
            *self.future.lock().unwrap() = Some(task);
            Ok(BackgroundTaskHandle {
                task_name: name.into(),
                task_id: 1,
            })
        }
        async fn sleep(&self, _: Duration) {
            std::future::pending::<()>().await;
        }
        async fn cancel(&self, _: &BackgroundTaskHandle) -> Result<(), RuntimeError> {
            self.cancelled.notify_one();
            Ok(())
        }
    }

    #[tokio::test]
    async fn stop_waits_for_tick_future_drop_even_after_waiter_cancellation() {
        let fs = MemFs::with(TASKS_PATH, r#"{"tasks":[]}"#);
        let runtime = Arc::new(DeferredCancelRuntime::default());
        let scheduler = Arc::new(
            CronScheduler::new(
                registry(fs.clone()),
                fs,
                FixedClock::at_secs(NOW),
                runtime.clone(),
                PathBuf::from(TASKS_PATH),
            )
            .with_test_registry_delivery(),
        );
        scheduler.clone().start().await.unwrap();
        let first_scheduler = scheduler.clone();
        let mut first = tokio::spawn(async move { first_scheduler.stop().await });
        runtime.cancelled.notified().await;
        let returned_early = tokio::time::timeout(Duration::from_millis(30), &mut first)
            .await
            .is_ok();
        if !returned_early {
            first.abort();
            let _ = first.await;
        }
        let second_scheduler = scheduler.clone();
        let mut second = tokio::spawn(async move { second_scheduler.stop().await });
        let retry_returned_early = tokio::time::timeout(Duration::from_millis(30), &mut second)
            .await
            .is_ok();
        let future = runtime.future.lock().unwrap().take();
        drop(future);
        if !retry_returned_early {
            tokio::time::timeout(Duration::from_secs(1), second)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
        assert!(
            !returned_early,
            "cancel acknowledgement does not prove producer exit"
        );
        assert!(
            !retry_returned_early,
            "cancelled waiter must retain the producer barrier"
        );
        scheduler.stop().await.unwrap();
    }

    #[async_trait]
    impl RuntimeSpawner for UnusedRuntime {
        async fn spawn(
            &self,
            _: &str,
            _: Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
        ) -> Result<BackgroundTaskHandle, RuntimeError> {
            panic!("spawn should not be called in scheduler tick tests");
        }

        async fn sleep(&self, duration: Duration) {
            tokio::time::sleep(duration).await;
        }

        async fn cancel(&self, _: &BackgroundTaskHandle) -> Result<(), RuntimeError> {
            panic!("cancel should not be called in scheduler tick tests");
        }
    }

    fn registry(fs: Arc<dyn FileSystem>) -> Arc<TaskRegistry> {
        Arc::new(TaskRegistry::new(
            Arc::new(UnusedRuntime),
            fs.clone(),
            Arc::new(TaskOutputManager::new(PathBuf::from(OUTPUT_DIR), fs)),
        ))
    }

    struct RecordingDreamHandler {
        spawns: AtomicUsize,
        prompts: std::sync::Mutex<Vec<String>>,
    }

    impl RecordingDreamHandler {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                spawns: AtomicUsize::new(0),
                prompts: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn spawn_count(&self) -> usize {
            self.spawns.load(Ordering::SeqCst)
        }

        fn prompts(&self) -> Vec<String> {
            self.prompts.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Task for RecordingDreamHandler {
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
            let TaskSpawnInput::Dream { prompt, .. } = &input else {
                return Err(TaskError::Internal(format!(
                    "unexpected input for dream handler: {input:?}"
                )));
            };
            self.prompts.lock().unwrap().push(prompt.clone());
            let seq = self.spawns.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(TaskHandle::new(format!("d{seq:08}"), None))
        }

        async fn kill(&self, _task_id: &str, _ctx: TaskContext) -> Result<(), TaskError> {
            Ok(())
        }
    }

    fn registry_with_dream_handler(
        fs: Arc<dyn FileSystem>,
    ) -> (Arc<TaskRegistry>, Arc<RecordingDreamHandler>) {
        let mut registry = TaskRegistry::new(
            Arc::new(UnusedRuntime),
            fs.clone(),
            Arc::new(TaskOutputManager::new(PathBuf::from(OUTPUT_DIR), fs)),
        );
        let handler = RecordingDreamHandler::new();
        registry.register_handler(TaskType::Dream, handler.clone());
        (Arc::new(registry), handler)
    }

    struct FailingDreamHandler {
        attempts: AtomicUsize,
    }

    impl FailingDreamHandler {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                attempts: AtomicUsize::new(0),
            })
        }

        fn attempt_count(&self) -> usize {
            self.attempts.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Task for FailingDreamHandler {
        fn name(&self) -> &str {
            "failing_dream"
        }

        fn task_type(&self) -> TaskType {
            TaskType::Dream
        }

        async fn spawn(
            &self,
            input: TaskSpawnInput,
            _ctx: TaskContext,
        ) -> Result<TaskHandle, TaskError> {
            if !matches!(input, TaskSpawnInput::Dream { .. }) {
                return Err(TaskError::Internal(format!(
                    "unexpected input for dream handler: {input:?}"
                )));
            }
            self.attempts.fetch_add(1, Ordering::SeqCst);
            Err(TaskError::Internal("boom".into()))
        }

        async fn kill(&self, _task_id: &str, _ctx: TaskContext) -> Result<(), TaskError> {
            Ok(())
        }
    }

    fn registry_with_failing_dream_handler(
        fs: Arc<dyn FileSystem>,
    ) -> (Arc<TaskRegistry>, Arc<FailingDreamHandler>) {
        let mut registry = TaskRegistry::new(
            Arc::new(UnusedRuntime),
            fs.clone(),
            Arc::new(TaskOutputManager::new(PathBuf::from(OUTPUT_DIR), fs)),
        );
        let handler = FailingDreamHandler::new();
        registry.register_handler(TaskType::Dream, handler.clone());
        (Arc::new(registry), handler)
    }

    fn scheduler(
        registry: Arc<TaskRegistry>,
        fs: Arc<dyn FileSystem>,
        clock: Arc<dyn Clock>,
    ) -> CronScheduler {
        CronScheduler::new(
            registry,
            fs,
            clock,
            Arc::new(UnusedRuntime),
            PathBuf::from(TASKS_PATH),
        )
        .with_test_registry_delivery()
        .with_recurring_max_age(Some(super::DEFAULT_RECURRING_MAX_AGE))
    }

    struct RecordingDelivery {
        loading: std::sync::atomic::AtomicBool,
        fires: tokio::sync::Mutex<Vec<super::SessionCronFire>>,
    }

    struct LoopAwareDelivery {
        fires: tokio::sync::Mutex<Vec<super::SessionCronFire>>,
        loop_runtime: Arc<crate::autonomous_loop::LoopRuntime>,
    }
    #[async_trait]
    impl super::SessionCronDelivery for LoopAwareDelivery {
        async fn is_loading(&self) -> bool {
            false
        }
        async fn enqueue(&self, fire: super::SessionCronFire) -> Result<(), String> {
            self.fires.lock().await.push(fire);
            Ok(())
        }
        fn loop_runtime(&self) -> Option<Arc<crate::autonomous_loop::LoopRuntime>> {
            Some(self.loop_runtime.clone())
        }
    }
    #[async_trait]
    impl super::SessionCronDelivery for RecordingDelivery {
        async fn is_loading(&self) -> bool {
            self.loading.load(Ordering::SeqCst)
        }
        async fn enqueue(&self, fire: super::SessionCronFire) -> Result<(), String> {
            self.fires.lock().await.push(fire);
            Ok(())
        }
        async fn clear_queued(&self) {
            self.fires.lock().await.clear();
        }
    }

    #[tokio::test]
    async fn session_cron_waits_for_binding_and_idle_then_delivers_raw_prompt() {
        let fs = MemFs::with(TASKS_PATH, "{\"tasks\":[]}");
        let clock = FixedClock::at_secs(NOW);
        let (registry, handler) = registry_with_dream_handler(fs.clone());
        let scheduler = CronScheduler::new(
            registry,
            fs,
            clock,
            Arc::new(UnusedRuntime),
            PathBuf::from(TASKS_PATH),
        );
        scheduler
            .register_tool_job(
                super::SessionCronTask {
                    id: "00000000".into(),
                    cron: "* * * * *".into(),
                    prompt: "__loop_sentinel_raw__".into(),
                    created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(NOW - 120),
                    last_fired_at: None,
                    recurring: true,
                    owner: None,
                },
                false,
            )
            .await
            .unwrap();
        scheduler.tick().await;
        assert_eq!(scheduler.session_jobs().await[0].last_fired_at, None);
        let delivery = Arc::new(RecordingDelivery {
            loading: std::sync::atomic::AtomicBool::new(true),
            fires: tokio::sync::Mutex::new(Vec::new()),
        });
        scheduler.set_session_delivery(delivery.clone()).await;
        scheduler.tick().await;
        assert_eq!(scheduler.session_jobs().await[0].last_fired_at, None);
        delivery.loading.store(false, Ordering::SeqCst);
        scheduler.tick().await;
        let fires = delivery.fires.lock().await;
        assert_eq!(fires.len(), 1);
        assert_eq!(
            (&*fires[0].id, &*fires[0].cron, &*fires[0].prompt),
            ("00000000", "* * * * *", "__loop_sentinel_raw__")
        );
        assert_eq!(
            handler.spawn_count(),
            0,
            "production queue delivery never creates a Dream agent"
        );
    }

    /// PARITY 2.1.270 `A()` (`src_197155721.js`). Its blocking predicate is
    /// `U(o) = subtype === "scheduled_task_fire" || subtype ===
    /// "compact_boundary"`, and it matches EVERY scheduled fire — the forward
    /// span starts after the LAST loop fire, so a `U` found there is always
    /// some OTHER task firing into the same transcript. All three hosts only
    /// counted compactions, so a fixed task could fire during a quiet tick and
    /// the next wakeup would fold its notice out of sight.
    #[tokio::test]
    async fn a_fixed_task_firing_into_the_loop_span_vetoes_the_fold() {
        async fn fire_once(
            prompt: &str,
            tick_in_flight: bool,
        ) -> crate::autonomous_loop::LoopFoldOutcome {
            let fs = MemFs::with(TASKS_PATH, "{\"tasks\":[]}");
            let clock = FixedClock::at_secs(NOW);
            let scheduler = CronScheduler::new(
                registry(fs.clone()),
                fs,
                clock,
                Arc::new(UnusedRuntime),
                PathBuf::from(TASKS_PATH),
            );
            let loop_runtime = Arc::new(crate::autonomous_loop::LoopRuntime::default());
            scheduler
                .register_tool_job(
                    super::SessionCronTask {
                        id: "00000000".into(),
                        cron: "* * * * *".into(),
                        prompt: prompt.into(),
                        created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(NOW - 120),
                        last_fired_at: None,
                        recurring: true,
                        owner: None,
                    },
                    false,
                )
                .await
                .unwrap();
            let delivery = Arc::new(LoopAwareDelivery {
                fires: tokio::sync::Mutex::new(Vec::new()),
                loop_runtime: loop_runtime.clone(),
            });
            scheduler.set_session_delivery(delivery.clone()).await;
            if tick_in_flight {
                loop_runtime.begin_tick("<<autonomous-loop>>".into());
            }
            scheduler.tick().await;
            assert_eq!(
                delivery.fires.lock().await.len(),
                1,
                "precondition: the fire must actually be delivered"
            );
            if !tick_in_flight {
                loop_runtime.begin_tick("<<autonomous-loop>>".into());
            }
            loop_runtime.mark_noop_reported(true);
            loop_runtime
                .settle_tick(
                    SystemTime::UNIX_EPOCH + Duration::from_secs(NOW),
                    crate::autonomous_loop::LoopSpanCounts::default(),
                )
                .expect("a tick was in flight")
        }

        // A fixed task firing INSIDE the span is `blocking_system_in_span`.
        assert!(
            matches!(
                fire_once("do the thing", true).await,
                crate::autonomous_loop::LoopFoldOutcome::Vetoed {
                    reason: crate::autonomous_loop::LoopFoldVeto::BlockingSystemInSpan
                }
            ),
            "a fixed fire inside the span must veto the fold"
        );
        // …and one BETWEEN fires breaks the settled streak, which is the
        // backward `blocking_system_before_anchor` scan.
        assert!(
            matches!(
                fire_once("do the thing", false).await,
                crate::autonomous_loop::LoopFoldOutcome::Folded { streak: 1, .. }
            ),
            "a fire between ticks resets the streak rather than extending it"
        );
        // The loop's OWN fire is `I(e)`: it ends that scan, it is not a veto.
        assert!(
            matches!(
                fire_once("<<autonomous-loop>>", true).await,
                crate::autonomous_loop::LoopFoldOutcome::Folded { streak: 1, .. }
            ),
            "the loop's own fire must not veto its own tick"
        );
    }

    #[tokio::test]
    async fn session_switch_discards_memory_jobs_and_queued_fires() {
        let fs = MemFs::with(TASKS_PATH, "{\"tasks\":[]}");
        let scheduler = Arc::new(
            CronScheduler::new(
                registry(fs.clone()),
                fs,
                FixedClock::at_secs(NOW),
                Arc::new(AutomationTestRuntime::default()),
                PathBuf::from(TASKS_PATH),
            )
            .with_session_id("old".into()),
        );
        let delivery = Arc::new(RecordingDelivery {
            loading: std::sync::atomic::AtomicBool::new(false),
            fires: tokio::sync::Mutex::new(Vec::new()),
        });
        scheduler.set_session_delivery(delivery.clone()).await;
        scheduler
            .register_tool_job(
                SessionCronTask {
                    id: "old-job".into(),
                    cron: "* * * * *".into(),
                    prompt: "old".into(),
                    created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(NOW - 120),
                    last_fired_at: None,
                    recurring: true,
                    owner: None,
                },
                false,
            )
            .await
            .unwrap();
        scheduler.tick().await;
        assert_eq!(delivery.fires.lock().await.len(), 1);
        scheduler.set_session_id("new".into()).await.unwrap();
        assert!(scheduler.session_jobs().await.is_empty());
        assert!(delivery.fires.lock().await.is_empty());
        assert_eq!(scheduler.current_session_id().as_deref(), Some("new"));
        scheduler.stop().await.unwrap();
    }

    #[tokio::test]
    async fn claude_session_store_and_task_center_have_independent_envelopes() {
        let center = r#"{"tasks":[{"id":"same","cron":"* * * * *","prompt":"center","createdAt":1,"automation":{"version":2,"model":"model"}}]}"#;
        let fs = MemFs::with(AUTOMATION_PATH, center);
        let root = Path::new("/proj");
        assert!(
            crate::read_tasks_body(fs.as_ref(), root).await.is_err(),
            "no legacy .lingxi fallback"
        );
        let session = r#"{"tasks":[{"id":"same","cron":"* * * * *","prompt":"session","createdAt":1,"recurring":true,"sessionId":"not-upstream","expiresAt":3}]}"#;
        crate::write_tasks_body(fs.as_ref(), root, session)
            .await
            .unwrap();
        let bytes = fs.get(TASKS_PATH).await.unwrap();
        assert_eq!(bytes, "{\n  \"tasks\": [\n    {\n      \"id\": \"same\",\n      \"cron\": \"* * * * *\",\n      \"prompt\": \"session\",\n      \"createdAt\": 1,\n      \"recurring\": true\n    }\n  ]\n}\n");
        assert_eq!(fs.get(AUTOMATION_PATH).await.unwrap(), center);
        crate::tasks_file::write_automation_tasks_body(fs.as_ref(), root, "{\"tasks\":[]}")
            .await
            .unwrap();
        assert_eq!(fs.get(TASKS_PATH).await.unwrap(), bytes);
    }

    #[tokio::test]
    async fn future_orphan_agent_cron_is_removed_without_waiting_until_due() {
        let fs = MemFs::with(TASKS_PATH, "{\"tasks\":[]}");
        let (registry, handler) = registry_with_dream_handler(fs.clone());
        let scheduler = scheduler(registry, fs, FixedClock::at_secs(NOW));
        scheduler
            .register_tool_job(
                SessionCronTask {
                    id: "orphan".into(),
                    cron: "0 0 1 1 *".into(),
                    prompt: "do not run in main".into(),
                    created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(NOW - 120),
                    last_fired_at: None,
                    recurring: true,
                    owner: Some("missing-agent".into()),
                },
                false,
            )
            .await
            .unwrap();
        scheduler.tick().await;
        assert!(scheduler.session_jobs().await.is_empty());
        assert_eq!(handler.spawn_count(), 0);
    }

    #[tokio::test]
    async fn project_leader_lease_blocks_peer_and_releases_on_stop() {
        let fs = MemFs::with(TASKS_PATH, "{\"tasks\":[]}");
        let clock = FixedClock::at_secs(NOW);
        let a = scheduler(registry(fs.clone()), fs.clone(), clock.clone())
            .with_session_id("owner".into());
        let b = scheduler(registry(fs.clone()), fs.clone(), clock).with_session_id("peer".into());
        a.refresh_project_leader().await;
        b.refresh_project_leader().await;
        assert!(a.project_leader.load(Ordering::SeqCst));
        assert!(!b.project_leader.load(Ordering::SeqCst));
        let record = fs.get("/proj/.claude/scheduled_tasks.lock").await.unwrap();
        let record: serde_json::Value = serde_json::from_str(&record).unwrap();
        assert_eq!(record["sessionId"], "owner");
        assert_eq!(record["acquiredAt"], NOW * 1000);
        assert!(record.get("job_id").is_none());
        a.stop().await.unwrap();
        b.refresh_project_leader().await;
        assert!(b.project_leader.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn stale_due_snapshot_from_peer_only_creates_one_task() {
        let created_ms = (NOW - 120) * 1000;
        let body = format!(
            r#"{{"tasks":[{{"id":"d11111111","cron":"* * * * *","prompt":"hello","createdAt":{created_ms},"recurring":true}}]}}"#
        );
        let fs = MemFs::with(TASKS_PATH, &body);
        let clock = FixedClock::at_secs(NOW);
        let (registry_a, handler_a) = registry_with_dream_handler(fs.clone());
        let (registry_b, handler_b) = registry_with_dream_handler(fs.clone());

        let scheduler_a = scheduler(registry_a, fs.clone(), clock.clone());
        let scheduler_b = scheduler(registry_b, fs.clone(), clock.clone());

        scheduler_a.load_persisted().await;
        scheduler_b.load_persisted().await;

        let stale_due_ids = {
            let tasks = scheduler_b.tasks.read().await;
            tasks
                .values()
                .filter(|task| super::is_job_due(task, clock.now()))
                .map(|task| task.id.clone())
                .collect::<Vec<_>>()
        };

        scheduler_a.tick().await;
        scheduler_b
            .process_due_ids(clock.now(), stale_due_ids)
            .await;

        assert_eq!(
            handler_a.spawn_count() + handler_b.spawn_count(),
            1,
            "the stale second scheduler snapshot must not execute a duplicate dream handler"
        );

        let after = crate::tasks_file::parse_tasks(&fs.get(TASKS_PATH).await.unwrap());
        assert_eq!(after.tasks.len(), 1);
        assert_eq!(after.tasks[0].last_fired_at, Some(NOW * 1000));

        let scheduler_b_last_run = scheduler_b
            .tasks
            .read()
            .await
            .get("d11111111")
            .and_then(|task| task.last_run);
        assert_eq!(scheduler_b_last_run, None);
    }

    #[tokio::test]
    async fn corrupt_authoritative_state_never_executes_stale_in_memory_job() {
        let created_ms = (NOW - 120) * 1000;
        let body = format!(
            r#"{{"tasks":[{{"id":"d22222222","cron":"* * * * *","prompt":"must not run","createdAt":{created_ms},"recurring":true}}]}}"#
        );
        let fs = MemFs::with(TASKS_PATH, &body);
        let clock = FixedClock::at_secs(NOW);
        let (registry, handler) = registry_with_dream_handler(fs.clone());
        let scheduler = scheduler(registry, fs.clone(), clock.clone());
        scheduler.load_persisted().await;
        let stale_due_ids = vec!["d22222222".to_string()];
        fs.files
            .lock()
            .await
            .insert(TASKS_PATH.to_string(), "{".to_string());

        scheduler.process_due_ids(clock.now(), stale_due_ids).await;

        assert_eq!(
            handler.spawn_count(),
            0,
            "corrupt durable state must fail closed instead of running stale memory through the dream handler"
        );
    }

    #[tokio::test]
    async fn session_one_shot_is_listed_claimed_and_removed_without_disk_state() {
        let fs = MemFs::with(TASKS_PATH, r#"{"tasks":[]}"#);
        let clock = FixedClock::at_secs(NOW);
        let (registry, handler) = registry_with_dream_handler(fs.clone());
        let scheduler = scheduler(registry, fs.clone(), clock.clone());
        scheduler
            .register_tool_job(
                SessionCronTask {
                    id: "dsession1".into(),
                    cron: "* * * * *".into(),
                    prompt: "wake".into(),
                    created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(NOW - 120),
                    last_fired_at: None,
                    recurring: false,
                    owner: None,
                },
                false,
            )
            .await
            .unwrap();

        assert_eq!(scheduler.session_jobs().await.len(), 1);
        scheduler.tick().await;
        assert_eq!(handler.spawn_count(), 1);
        assert!(scheduler.session_jobs().await.is_empty());
        assert!(!scheduler.tasks.read().await.contains_key("dsession1"));
        let persisted = crate::tasks_file::parse_tasks(&fs.get(TASKS_PATH).await.unwrap());
        assert!(persisted.tasks.is_empty());
    }

    #[tokio::test]
    async fn session_recurring_claim_updates_memory_and_owner_guards_delete() {
        let fs = MemFs::with(TASKS_PATH, r#"{"tasks":[]}"#);
        let clock = FixedClock::at_secs(NOW);
        let (registry, handler) = registry_with_dream_handler(fs.clone());
        let scheduler = scheduler(registry, fs, clock.clone());
        scheduler
            .register_tool_job(
                SessionCronTask {
                    id: "dsession2".into(),
                    cron: "* * * * *".into(),
                    prompt: "repeat".into(),
                    created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(NOW - 120),
                    last_fired_at: None,
                    recurring: true,
                    owner: None,
                },
                false,
            )
            .await
            .unwrap();

        assert!(
            !scheduler
                .unregister_job("dsession2", Some("agent:two"))
                .await
        );
        scheduler.tick().await;
        assert_eq!(handler.spawn_count(), 1);
        let jobs = scheduler.session_jobs().await;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].last_fired_at, Some(clock.now()));
        assert!(scheduler.unregister_job("dsession2", None).await);
        assert!(scheduler.session_jobs().await.is_empty());
    }

    #[tokio::test]
    async fn session_one_shot_spawn_failure_restores_claimed_state() {
        let fs = MemFs::with(TASKS_PATH, r#"{"tasks":[]}"#);
        let clock = FixedClock::at_secs(NOW);
        let (registry, handler) = registry_with_failing_dream_handler(fs.clone());
        let scheduler = scheduler(registry, fs.clone(), clock.clone());
        scheduler
            .register_tool_job(
                SessionCronTask {
                    id: "dsession_fail_once".into(),
                    cron: "* * * * *".into(),
                    prompt: "wake".into(),
                    created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(NOW - 120),
                    last_fired_at: None,
                    recurring: false,
                    owner: None,
                },
                false,
            )
            .await
            .unwrap();

        scheduler.tick().await;

        assert_eq!(handler.attempt_count(), 1);
        let jobs = scheduler.session_jobs().await;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].last_fired_at, None);
        assert!(scheduler
            .tasks
            .read()
            .await
            .contains_key("dsession_fail_once"));
        assert_eq!(
            crate::tasks_file::parse_tasks(&fs.get(TASKS_PATH).await.unwrap())
                .tasks
                .len(),
            0
        );
    }

    #[tokio::test]
    async fn session_recurring_spawn_failure_restores_last_fired_at() {
        let fs = MemFs::with(TASKS_PATH, r#"{"tasks":[]}"#);
        let clock = FixedClock::at_secs(NOW);
        let (registry, handler) = registry_with_failing_dream_handler(fs.clone());
        let scheduler = scheduler(registry, fs, clock.clone());
        let previous_fire = SystemTime::UNIX_EPOCH + Duration::from_secs(NOW - 120);
        scheduler
            .register_tool_job(
                SessionCronTask {
                    id: "dsession_fail_repeat".into(),
                    cron: "* * * * *".into(),
                    prompt: "repeat".into(),
                    created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(NOW - 240),
                    last_fired_at: Some(previous_fire),
                    recurring: true,
                    owner: None,
                },
                false,
            )
            .await
            .unwrap();

        scheduler.tick().await;

        assert_eq!(handler.attempt_count(), 1);
        let jobs = scheduler.session_jobs().await;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].last_fired_at, Some(previous_fire));
        assert_eq!(
            scheduler
                .tasks
                .read()
                .await
                .get("dsession_fail_repeat")
                .and_then(|task| task.last_run),
            Some(previous_fire)
        );
    }

    #[tokio::test]
    async fn session_spawn_failure_does_not_overwrite_concurrent_job_edit() {
        let fs = MemFs::with(TASKS_PATH, r#"{"tasks":[]}"#);
        let clock = FixedClock::at_secs(NOW);
        let (registry, _handler) = registry_with_dream_handler(fs.clone());
        let scheduler = scheduler(registry, fs, clock.clone());
        scheduler
            .register_tool_job(
                SessionCronTask {
                    id: "dsession_edit".into(),
                    cron: "* * * * *".into(),
                    prompt: "before".into(),
                    created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(NOW - 120),
                    last_fired_at: None,
                    recurring: true,
                    owner: Some("agent:one".into()),
                },
                false,
            )
            .await
            .unwrap();

        let claimed = scheduler
            .claim_in_memory_due_job("dsession_edit", clock.now())
            .await
            .expect("job should be claimable");
        scheduler
            .register_tool_job(
                SessionCronTask {
                    id: "dsession_edit".into(),
                    cron: "*/5 * * * *".into(),
                    prompt: "edited while spawn was pending".into(),
                    created_at: clock.now(),
                    last_fired_at: None,
                    recurring: true,
                    owner: Some("agent:two".into()),
                },
                false,
            )
            .await
            .unwrap();

        let (_, rollback) = claimed.into_parts();
        scheduler.rollback_claim("dsession_edit", rollback).await;

        let jobs = scheduler.session_jobs().await;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].prompt, "edited while spawn was pending");
        assert_eq!(jobs[0].owner.as_deref(), Some("agent:two"));
        assert_eq!(
            scheduler
                .tasks
                .read()
                .await
                .get("dsession_edit")
                .map(|task| task.prompt.as_str()),
            Some("edited while spawn was pending")
        );
    }

    // PARITY `V(true)` + `zQn`/`be()`: a durable one-shot whose fire time passed
    // while nothing was running is never executed. It is removed from the file
    // and surfaced as the confirmation prompt, once.
    #[tokio::test]
    async fn missed_durable_one_shot_is_surfaced_not_fired() {
        let created_ms = (NOW - 120) * 1000;
        let body = format!(
            r#"{{"tasks":[{{"id":"dmissed01","cron":"* * * * *","prompt":"deploy it","createdAt":{created_ms}}},{{"id":"dkeep0001","cron":"0 0 1 1 *","prompt":"new year","createdAt":{created_ms},"recurring":true}}]}}"#
        );
        let fs = MemFs::with(TASKS_PATH, &body);
        let clock = FixedClock::at_secs(NOW);
        let (registry, handler) = registry_with_dream_handler(fs.clone());
        let scheduler = scheduler(registry, fs.clone(), clock.clone());
        scheduler.load_persisted().await;
        scheduler.tick().await;

        assert_eq!(
            handler.spawn_count(),
            1,
            "one confirmation prompt, not a fire"
        );
        let prompt = handler.prompts().remove(0);
        assert!(prompt.starts_with("The following one-shot scheduled task was missed while Claude was not running. It has already been removed from .claude/scheduled_tasks.json.\n\nDo NOT execute this prompt yet. First use the AskUserQuestion tool to ask whether to run it now. Only execute if the user confirms.\n\n[Every minute, created "), "{prompt}");
        assert!(prompt.ends_with("]\ndeploy it"), "{prompt}");
        assert!(!scheduler.tasks.read().await.contains_key("dmissed01"));
        assert!(scheduler.tasks.read().await.contains_key("dkeep0001"));
        let doc = crate::tasks_file::parse_tasks(&fs.get(TASKS_PATH).await.unwrap());
        assert_eq!(
            doc.tasks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec!["dkeep0001"]
        );

        // Subsequent ticks never fire it (the file no longer lists it).
        scheduler.tick().await;
        scheduler.tick().await;
        assert_eq!(handler.spawn_count(), 1);
    }

    #[tokio::test]
    async fn missed_durable_one_shots_plural_prompt_and_delivery_failure_still_removes() {
        let created_ms = (NOW - 120) * 1000;
        let body = format!(
            r#"{{"tasks":[{{"id":"dmissa","cron":"* * * * *","prompt":"a","createdAt":{created_ms}}},{{"id":"dmissb","cron":"* * * * *","prompt":"b","createdAt":{created_ms}}}]}}"#
        );
        let fs = MemFs::with(TASKS_PATH, &body);
        let clock = FixedClock::at_secs(NOW);
        let (registry, handler) = registry_with_failing_dream_handler(fs.clone());
        let scheduler = scheduler(registry, fs.clone(), clock.clone());
        scheduler.load_persisted().await;
        scheduler.tick().await;
        assert_eq!(
            handler.attempt_count(),
            1,
            "one delivery attempt for both tasks"
        );
        let doc = crate::tasks_file::parse_tasks(&fs.get(TASKS_PATH).await.unwrap());
        assert!(
            doc.tasks.is_empty(),
            "missed tasks are removed even when delivery fails"
        );
        assert!(scheduler.tasks.read().await.is_empty());
        let text = super::missed_one_shots_prompt(&[
            crate::tasks_file::CronTask {
                creator: Default::default(),
                id: "a".into(),
                cron: "* * * * *".into(),
                prompt: "a".into(),
                created_at: created_ms,
                last_fired_at: None,
                recurring: None,
                permanent: None,
                expires_at: None,
                session_id: None,
                automation: None,
            },
            crate::tasks_file::CronTask {
                creator: Default::default(),
                id: "b".into(),
                cron: "0 9 * * 1-5".into(),
                prompt: "b".into(),
                created_at: created_ms,
                last_fired_at: None,
                recurring: None,
                permanent: None,
                expires_at: None,
                session_id: None,
                automation: None,
            },
        ]);
        assert!(text.starts_with("The following one-shot scheduled tasks were missed while Claude was not running. They have already been removed from .claude/scheduled_tasks.json.\n\nDo NOT execute these prompts yet. First use the AskUserQuestion tool to ask whether to run each one now. Only execute if the user confirms.\n\n[Every minute, created "));
        assert!(text.contains("]\na\n\n[Weekdays at 9:00 AM, created "));
        assert!(text.ends_with("]\nb"));
    }

    #[tokio::test]
    async fn durable_recurring_spawn_failure_restores_original_body_and_last_run() {
        let created_ms = (NOW - 240) * 1000;
        let last_fired_ms = (NOW - 120) * 1000;
        let body = format!(
            r#"{{"tasks":[{{"id":"dfailrecur","cron":"* * * * *","prompt":"hello","createdAt":{created_ms},"lastFiredAt":{last_fired_ms},"recurring":true}}]}}"#
        );
        let fs = MemFs::with(TASKS_PATH, &body);
        let clock = FixedClock::at_secs(NOW);
        let (registry, handler) = registry_with_failing_dream_handler(fs.clone());
        let scheduler = scheduler(registry, fs.clone(), clock.clone());
        scheduler.load_persisted().await;
        let previous_fire = SystemTime::UNIX_EPOCH + Duration::from_secs(NOW - 120);

        scheduler.tick().await;

        assert_eq!(handler.attempt_count(), 1);
        assert_eq!(
            crate::parse_tasks(&fs.get(TASKS_PATH).await.unwrap()),
            crate::parse_tasks(&body)
        );
        assert_eq!(
            scheduler
                .tasks
                .read()
                .await
                .get("dfailrecur")
                .and_then(|task| task.last_run),
            Some(previous_fire)
        );
    }

    #[tokio::test]
    async fn aged_session_recurring_fires_once_then_expires() {
        let fs = MemFs::with(TASKS_PATH, r#"{"tasks":[]}"#);
        let clock = FixedClock::at_secs(NOW);
        let (registry, handler) = registry_with_dream_handler(fs.clone());
        let scheduler = scheduler(registry, fs.clone(), clock.clone());
        scheduler
            .register_tool_job(
                SessionCronTask {
                    id: "00000000".into(),
                    cron: "* * * * *".into(),
                    prompt: "final".into(),
                    created_at: SystemTime::UNIX_EPOCH
                        + Duration::from_secs(NOW - 8 * 24 * 60 * 60),
                    last_fired_at: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(NOW - 120)),
                    recurring: true,
                    owner: None,
                },
                false,
            )
            .await
            .unwrap();

        scheduler.tick().await;

        assert_eq!(handler.spawn_count(), 1);
        assert!(scheduler.session_jobs().await.is_empty());
        assert!(!scheduler.tasks.read().await.contains_key("00000000"));
    }

    #[tokio::test]
    async fn aged_session_recurring_not_due_is_not_predeleted() {
        let fs = MemFs::with(TASKS_PATH, r#"{"tasks":[]}"#);
        let clock = FixedClock::at_secs(NOW);
        let scheduler = scheduler(registry(fs.clone()), fs, clock.clone());
        scheduler
            .register_tool_job(
                SessionCronTask {
                    id: "00000000".into(),
                    cron: "* * * * *".into(),
                    prompt: "later".into(),
                    created_at: SystemTime::UNIX_EPOCH
                        + Duration::from_secs(NOW - 8 * 24 * 60 * 60),
                    last_fired_at: Some(clock.now()),
                    recurring: true,
                    owner: None,
                },
                false,
            )
            .await
            .unwrap();

        scheduler.tick().await;

        assert_eq!(scheduler.session_jobs().await.len(), 1);
        assert!(scheduler.tasks.read().await.contains_key("00000000"));
    }

    #[tokio::test]
    async fn public_session_api_resolves_same_registry_after_trait_coercion() {
        let fs = MemFs::with(TASKS_PATH, r#"{"tasks":[]}"#);
        let concrete_registry = registry(fs.clone());
        let trait_registry =
            concrete_registry.clone() as Arc<dyn platform_api::task_registry::TaskRegistryHandle>;
        assert_eq!(
            super::task_registry_identity(&concrete_registry),
            super::task_registry_identity(&trait_registry)
        );
        let scheduler = Arc::new(scheduler(
            concrete_registry.clone(),
            fs,
            FixedClock::at_secs(NOW),
        ));
        let key = super::task_registry_identity(&concrete_registry);
        super::LIVE_SCHEDULERS
            .lock()
            .unwrap()
            .insert(key, Arc::downgrade(&scheduler));

        super::register_live_job(
            &trait_registry,
            SessionCronTask {
                id: "dpublic01".into(),
                cron: "0 9 * * *".into(),
                prompt: "visible".into(),
                created_at: SystemTime::UNIX_EPOCH,
                last_fired_at: None,
                recurring: true,
                owner: None,
            },
            false,
        )
        .await
        .unwrap();
        assert_eq!(super::session_jobs(&trait_registry).await.unwrap().len(), 1);
        assert!(
            super::unregister_live_job(&trait_registry, "dpublic01", None)
                .await
                .unwrap()
        );
        super::LIVE_SCHEDULERS.lock().unwrap().remove(&key);
    }
    #[tokio::test]
    async fn durable_live_registration_fires_without_restarting_scheduler() {
        let created_ms = (NOW - 120) * 1000;
        let body = format!(
            r#"{{"tasks":[{{"id":"duilive01","cron":"* * * * *","prompt":"saved UI task","createdAt":{created_ms},"recurring":true}}]}}"#
        );
        let fs = MemFs::with(TASKS_PATH, &body);
        let clock = FixedClock::at_secs(NOW);
        let (registry, handler) = registry_with_dream_handler(fs.clone());
        let scheduler = Arc::new(scheduler(registry.clone(), fs, clock.clone()));
        let key = super::task_registry_identity(&registry);
        super::LIVE_SCHEDULERS
            .lock()
            .unwrap()
            .insert(key, Arc::downgrade(&scheduler));
        let registry = registry as Arc<dyn platform_api::task_registry::TaskRegistryHandle>;
        super::register_live_job(
            &registry,
            SessionCronTask {
                id: "duilive01".into(),
                cron: "* * * * *".into(),
                prompt: "saved UI task".into(),
                created_at: SystemTime::UNIX_EPOCH + Duration::from_millis(created_ms),
                last_fired_at: None,
                recurring: true,
                owner: None,
            },
            true,
        )
        .await
        .unwrap();
        scheduler.tick().await;
        assert_eq!(
            handler.spawn_count(),
            1,
            "a UI-created live job must dispatch through the Dream handler"
        );
        scheduler.tick().await;
        assert_eq!(handler.spawn_count(), 1, "same minute must not double-fire");
        super::LIVE_SCHEDULERS.lock().unwrap().remove(&key);
    }

    #[tokio::test]
    async fn deleted_durable_job_never_fires_from_another_scheduler() {
        for missing_file in [false, true] {
            let created_ms = (NOW - 120) * 1000;
            let body = format!(
                r#"{{"tasks":[{{"id":"ddeleted1","cron":"* * * * *","prompt":"deleted","createdAt":{created_ms},"recurring":true}}]}}"#
            );
            let fs = MemFs::with(TASKS_PATH, &body);
            let clock = FixedClock::at_secs(NOW);
            let (registry_a, handler_a) = registry_with_dream_handler(fs.clone());
            let (registry_b, handler_b) = registry_with_dream_handler(fs.clone());
            let a = scheduler(registry_a, fs.clone(), clock.clone());
            let b = scheduler(registry_b, fs.clone(), clock.clone());
            a.load_persisted().await;
            b.load_persisted().await;
            if missing_file {
                fs.files.lock().await.remove(TASKS_PATH);
            } else {
                fs.files
                    .lock()
                    .await
                    .insert(TASKS_PATH.into(), r#"{"tasks":[]}"#.into());
            }
            a.unregister_job("ddeleted1", None).await;
            b.process_due_ids(clock.now(), vec!["ddeleted1".into()])
                .await;
            assert_eq!(handler_a.spawn_count() + handler_b.spawn_count(), 0);
            assert!(!b.tasks.read().await.contains_key("ddeleted1"));
        }
    }

    #[tokio::test]
    async fn durable_creator_session_owns_live_process_and_resume_refreshes_pid() {
        for is_owner in [false, true] {
            let creator_pid = if is_owner { 1 } else { std::process::id() };
            let body = serde_json::json!({"tasks":[{
                "id":"00000000", "cron":"* * * * *", "prompt":"owned",
                "createdAt":(NOW-120)*1000, "recurring":true,
                "createdBySessionId":"creator", "createdByPid":creator_pid
            }]})
            .to_string();
            let fs = MemFs::with(TASKS_PATH, &body);
            let clock = FixedClock::at_secs(NOW);
            let (registry, handler) = registry_with_dream_handler(fs.clone());
            let scheduler = CronScheduler::new(
                registry,
                fs.clone(),
                clock,
                Arc::new(UnusedRuntime),
                PathBuf::from(TASKS_PATH),
            )
            .with_test_registry_delivery()
            .with_session_id(if is_owner { "creator" } else { "foreign" }.into());
            scheduler.load_persisted().await;
            scheduler.tick().await;
            assert_eq!(handler.spawn_count(), usize::from(is_owner));
            let doc = crate::tasks_file::parse_tasks(&fs.get(TASKS_PATH).await.unwrap());
            assert_eq!(doc.tasks.len(), 1);
            assert_eq!(
                doc.tasks[0].creator.created_by_pid,
                Some(std::process::id())
            );
        }
    }

    #[tokio::test]
    async fn legacy_ignores_task_center_expiration_and_preserves_permanent() {
        for (expired, permanent) in [(false, false), (true, false), (false, true)] {
            let created_ms = (NOW - 30 * 24 * 60 * 60) * 1000;
            let expiry = if expired {
                format!(",\"expiresAt\":{}", (NOW - 1) * 1000)
            } else {
                String::new()
            };
            let body = format!(
                r#"{{"tasks":[{{"id":"dexpiry01","cron":"* * * * *","prompt":"expiry test","createdAt":{created_ms},"recurring":true,"permanent":{permanent}{expiry}}}]}}"#
            );
            let fs = MemFs::with(TASKS_PATH, &body);
            let clock = FixedClock::at_secs(NOW);
            let (registry, handler) = registry_with_dream_handler(fs.clone());
            let scheduler = CronScheduler::new(
                registry,
                fs.clone(),
                clock,
                Arc::new(UnusedRuntime),
                PathBuf::from(TASKS_PATH),
            )
            .with_test_registry_delivery();
            assert_eq!(
                scheduler.recurring_max_age,
                Some(super::DEFAULT_RECURRING_MAX_AGE)
            );
            scheduler.load_persisted().await;
            scheduler.tick().await;
            assert_eq!(handler.spawn_count(), 1);
            assert_eq!(
                scheduler.tasks.read().await.contains_key("dexpiry01"),
                permanent
            );
            let doc = crate::tasks_file::parse_tasks(&fs.get(TASKS_PATH).await.unwrap());
            assert_eq!(doc.tasks.len(), usize::from(permanent));
        }
    }

    #[tokio::test]
    async fn tick_refreshes_peer_edits_and_new_durable_jobs() {
        let created_ms = (NOW - 120) * 1000;
        let body = format!(
            r#"{{"tasks":[{{"id":"dpeeredit","cron":"0 0 1 1 *","prompt":"old","createdAt":{created_ms},"recurring":true}}]}}"#
        );
        let fs = MemFs::with(TASKS_PATH, &body);
        let clock = FixedClock::at_secs(NOW);
        let (registry, handler) = registry_with_dream_handler(fs.clone());
        let scheduler = scheduler(registry, fs.clone(), clock.clone());
        scheduler.load_persisted().await;
        assert!(!super::is_job_due(
            &scheduler.tasks.read().await["dpeeredit"],
            clock.now()
        ));
        let edited = format!(
            r#"{{"tasks":[{{"id":"dpeeredit","cron":"* * * * *","prompt":"updated","createdAt":{created_ms},"recurring":true}},{{"id":"dpeernew1","cron":"* * * * *","prompt":"new","createdAt":{created_ms},"recurring":true}}]}}"#
        );
        fs.files.lock().await.insert(TASKS_PATH.into(), edited);
        scheduler.tick().await;
        assert_eq!(handler.spawn_count(), 2);
        scheduler.tick().await;
        assert_eq!(
            handler.spawn_count(),
            2,
            "persisted firing history must survive refresh"
        );
    }

    #[tokio::test]
    async fn authoritative_recurring_flag_controls_claim_finalization() {
        for recurring in [true, false] {
            let created_ms = (NOW - 120) * 1000;
            let body = format!(
                r#"{{"tasks":[{{"id":"dpeerflag","cron":"* * * * *","prompt":"test","createdAt":{created_ms},"recurring":{recurring}}}]}}"#
            );
            let fs = MemFs::with(TASKS_PATH, &body);
            let clock = FixedClock::at_secs(NOW);
            let (registry, handler) = registry_with_dream_handler(fs.clone());
            let scheduler = scheduler(registry, fs, clock.clone());
            // `refresh_durable_tasks` (the watcher-reload analogue) registers file
            // tasks without the startup missed-one-shot pass, so the past-due
            // one-shot is a live due job here rather than a surfaced miss.
            scheduler.refresh_durable_tasks().await;
            scheduler
                .tasks
                .write()
                .await
                .get_mut("dpeerflag")
                .unwrap()
                .recurring = !recurring;
            scheduler
                .process_due_ids(clock.now(), vec!["dpeerflag".into()])
                .await;
            assert_eq!(handler.spawn_count(), 1);
            assert_eq!(
                scheduler.tasks.read().await.contains_key("dpeerflag"),
                recurring
            );
            assert_eq!(
                scheduler.durable_ids.read().await.contains("dpeerflag"),
                recurring
            );
        }
    }

    #[tokio::test]
    async fn durable_refresh_preserves_other_registrations_and_corrupt_storage() {
        let created_ms = (NOW - 120) * 1000;
        let body = format!(
            r#"{{"tasks":[{{"id":"drefresh1","cron":"* * * * *","prompt":"saved","createdAt":{created_ms},"recurring":true}}]}}"#
        );
        let fs = MemFs::with(TASKS_PATH, &body);
        let clock = FixedClock::at_secs(NOW);
        let (registry, handler) = registry_with_dream_handler(fs.clone());
        let scheduler = scheduler(registry, fs.clone(), clock.clone());
        scheduler.load_persisted().await;
        scheduler
            .register("direct", "* * * * *", "direct", None)
            .await
            .unwrap();
        scheduler
            .register_tool_job(
                SessionCronTask {
                    id: "session".into(),
                    cron: "* * * * *".into(),
                    prompt: "session".into(),
                    created_at: clock.now(),
                    last_fired_at: None,
                    recurring: true,
                    owner: None,
                },
                false,
            )
            .await
            .unwrap();
        fs.files
            .lock()
            .await
            .insert(TASKS_PATH.into(), "invalid".into());
        scheduler.refresh_durable_tasks().await;
        assert_eq!(scheduler.tasks.read().await.len(), 3);
        scheduler
            .process_due_ids(clock.now(), vec!["drefresh1".into()])
            .await;
        assert_eq!(
            handler.spawn_count(),
            0,
            "corrupt durable state must never fire"
        );
        fs.files
            .lock()
            .await
            .insert(TASKS_PATH.into(), r#"{"tasks":[]}"#.into());
        scheduler.refresh_durable_tasks().await;
        let tasks = scheduler.tasks.read().await;
        assert_eq!(tasks.len(), 2);
        assert!(tasks.contains_key("direct"));
        assert!(tasks.contains_key("session"));
    }
    /// PARITY the chokidar watcher: a tick over an UNCHANGED tasks file must not
    /// re-read, re-parse and re-register durable state.
    ///
    /// The probe is a local edit the reload would clobber: `refresh_durable_tasks`
    /// rebuilds every file-backed definition, so if it ran, the doctored prompt
    /// would be back to the on-disk one. Asserting the prompt (not a read count)
    /// keeps the test honest about what the gate is FOR.
    #[tokio::test]
    async fn an_unchanged_tasks_file_is_not_reloaded_on_every_tick() {
        let created_ms = (NOW - 120) * 1000;
        let body = format!(
            r#"{{"tasks":[{{"id":"dgate","cron":"0 0 1 1 *","prompt":"on-disk","createdAt":{created_ms},"recurring":true}}]}}"#
        );
        let fs = MemFs::with(TASKS_PATH, &body);
        let clock = FixedClock::at_secs(NOW);
        let (registry, _) = registry_with_dream_handler(fs.clone());
        let scheduler = scheduler(registry, fs.clone(), clock.clone());
        scheduler.tick().await;
        assert!(scheduler.durable_ids.read().await.contains("dgate"));
        scheduler
            .tasks
            .write()
            .await
            .get_mut("dgate")
            .unwrap()
            .prompt = "doctored".into();
        scheduler.tick().await;
        assert_eq!(
            scheduler.tasks.read().await["dgate"].prompt,
            "doctored",
            "an unchanged document must not be re-applied"
        );

        // …and a real edit still lands on the very next tick.
        let edited = format!(
            r#"{{"tasks":[{{"id":"dgate","cron":"0 0 1 1 *","prompt":"edited","createdAt":{created_ms},"recurring":true}}]}}"#
        );
        fs.files.lock().await.insert(TASKS_PATH.into(), edited);
        scheduler.tick().await;
        assert_eq!(scheduler.tasks.read().await["dgate"].prompt, "edited");
    }

    /// PARITY `I`: the next fire is computed once per schedule and reused, and a
    /// record the file changed under us recomputes rather than firing on the
    /// schedule it had when first seen.
    #[tokio::test]
    async fn the_next_fire_is_cached_until_the_record_changes() {
        let created_ms = (NOW - 120) * 1000;
        let body = format!(
            r#"{{"tasks":[{{"id":"dcache","cron":"0 0 1 1 *","prompt":"p","createdAt":{created_ms},"recurring":true}}]}}"#
        );
        let fs = MemFs::with(TASKS_PATH, &body);
        let clock = FixedClock::at_secs(NOW);
        let (registry, handler) = registry_with_dream_handler(fs.clone());
        let scheduler = scheduler(registry, fs.clone(), clock.clone());
        scheduler.tick().await;
        let first = scheduler.next_fire.read().await["dcache"].at;
        assert!(first.is_some(), "a resolvable schedule caches an instant");
        scheduler.tick().await;
        assert_eq!(
            scheduler.next_fire.read().await["dcache"].at,
            first,
            "an unchanged record reuses the cached fire"
        );
        assert_eq!(handler.spawn_count(), 0);

        // A peer rewrites the cron to one that is due now.
        let edited = format!(
            r#"{{"tasks":[{{"id":"dcache","cron":"* * * * *","prompt":"p","createdAt":{created_ms},"recurring":true}}]}}"#
        );
        fs.files.lock().await.insert(TASKS_PATH.into(), edited);
        scheduler.tick().await;
        assert_eq!(
            handler.spawn_count(),
            1,
            "an edited schedule must not keep the fire time it was first seen with"
        );
    }

    /// The oracle's `for(let e of I.keys()) if(!h.has(e)) I.delete(e)` sweep: a
    /// job that is gone — or disabled, which LingXi has and the oracle does not
    /// — leaves no cache entry behind.
    #[tokio::test]
    async fn the_fire_cache_is_swept_of_jobs_that_are_gone_or_disabled() {
        let fs = MemFs::with(TASKS_PATH, r#"{"tasks":[]}"#);
        let clock = FixedClock::at_secs(NOW);
        let (registry, _) = registry_with_dream_handler(fs.clone());
        let scheduler = scheduler(registry, fs, clock.clone());
        scheduler
            .register("swept", "0 0 1 1 *", "p", None)
            .await
            .unwrap();
        scheduler.tick().await;
        assert!(scheduler.next_fire.read().await.contains_key("swept"));
        scheduler
            .tasks
            .write()
            .await
            .get_mut("swept")
            .unwrap()
            .enabled = false;
        scheduler.tick().await;
        assert!(
            !scheduler.next_fire.read().await.contains_key("swept"),
            "a disabled job keeps no cached fire"
        );
        scheduler.unregister_job("swept", None).await;
        scheduler.tick().await;
        assert!(scheduler.next_fire.read().await.is_empty());
    }

    #[tokio::test]
    async fn durable_refresh_serializes_identity_with_live_registration() {
        let fs = MemFs::with(TASKS_PATH, r#"{"tasks":[]}"#);
        let clock = FixedClock::at_secs(NOW);
        let (registry, _) = registry_with_dream_handler(fs.clone());
        let scheduler = Arc::new(scheduler(registry, fs, clock.clone()));
        let tasks_guard = scheduler.tasks.write().await;
        let refreshing = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move { scheduler.refresh_durable_tasks().await })
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if scheduler.durable_ids.try_write().is_err() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("refresh must hold identity lock while waiting to reconcile tasks");
        let registering = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move {
                scheduler
                    .register_tool_job(
                        SessionCronTask {
                            id: "dregister".into(),
                            cron: "* * * * *".into(),
                            prompt: "new".into(),
                            created_at: clock.now(),
                            last_fired_at: None,
                            recurring: true,
                            owner: None,
                        },
                        true,
                    )
                    .await
                    .unwrap();
            })
        };
        drop(tasks_guard);
        refreshing.await.unwrap();
        registering.await.unwrap();
        assert!(scheduler.durable_ids.read().await.contains("dregister"));
        assert!(scheduler.tasks.read().await.contains_key("dregister"));
    }
    #[derive(Default)]
    struct AutomationTestRuntime {
        sequence: AtomicUsize,
        handles: std::sync::Mutex<HashMap<u64, tokio::task::JoinHandle<()>>>,
    }
    #[async_trait]
    impl RuntimeSpawner for AutomationTestRuntime {
        async fn spawn(
            &self,
            name: &str,
            task: Pin<Box<dyn Future<Output = ()> + Send>>,
        ) -> Result<BackgroundTaskHandle, RuntimeError> {
            let id = self.sequence.fetch_add(1, Ordering::SeqCst) as u64;
            self.handles.lock().unwrap().insert(id, tokio::spawn(task));
            Ok(BackgroundTaskHandle {
                task_name: name.into(),
                task_id: id,
            })
        }
        async fn sleep(&self, duration: Duration) {
            tokio::time::sleep(duration).await;
        }
        async fn cancel(&self, handle: &BackgroundTaskHandle) -> Result<(), RuntimeError> {
            let task = self.handles.lock().unwrap().remove(&handle.task_id);
            if let Some(task) = task {
                task.abort();
                let _ = task.await;
            }
            Ok(())
        }
    }
    struct AutomationClock(AtomicUsize);
    impl Clock for AutomationClock {
        fn now(&self) -> SystemTime {
            SystemTime::UNIX_EPOCH + Duration::from_secs(self.0.load(Ordering::SeqCst) as u64)
        }
    }
    struct BlockedAutomationFirer {
        started: tokio::sync::Notify,
        fast_finished: tokio::sync::Notify,
        dropped: Arc<AtomicUsize>,
    }
    struct DroppedAutomation(Arc<AtomicUsize>);
    impl Drop for DroppedAutomation {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    #[async_trait]
    impl crate::CronJobFirer for BlockedAutomationFirer {
        async fn fire(&self, _: &str, _: &str) -> Result<String, String> {
            panic!("v2 must not use legacy execution");
        }
        async fn fire_automation(
            &self,
            request: &crate::AutomationRunRequest,
        ) -> Result<crate::AutomationRunResult, String> {
            if request.task.id == "slow" {
                let _drop = DroppedAutomation(self.dropped.clone());
                self.started.notify_one();
                std::future::pending::<()>().await;
            }
            self.fast_finished.notify_one();
            Ok(crate::AutomationRunResult {
                session_id: "fast-session".into(),
                summary: "done".into(),
            })
        }
    }
    #[tokio::test]
    async fn blocked_automation_does_not_block_other_tasks_ticks_or_pending_coalescing() {
        let tasks: Vec<_> = ["slow", "fast"]
            .iter()
            .map(|id| {
                serde_json::json!({
                    "id": id,
                    "cron": "* * * * *",
                    "prompt": "run",
                    "createdAt": (NOW - 120) * 1000,
                    "recurring": true,
                    "automation": { "version": 2, "model": "provider/model" }
                })
            })
            .collect();
        let body = serde_json::json!({ "tasks": tasks }).to_string();
        let fs = MemFs::with(AUTOMATION_PATH, &body);
        let clock = Arc::new(AutomationClock(AtomicUsize::new(NOW as usize)));
        let runtime = Arc::new(AutomationTestRuntime::default());
        let executor = Arc::new(BlockedAutomationFirer {
            started: tokio::sync::Notify::new(),
            fast_finished: tokio::sync::Notify::new(),
            dropped: Arc::new(AtomicUsize::new(0)),
        });
        let scheduler = CronScheduler::new(
            registry(fs.clone()),
            fs.clone(),
            clock.clone(),
            runtime,
            PathBuf::from(AUTOMATION_PATH),
        )
        .with_test_registry_delivery();
        scheduler.set_automation_firer(executor.clone()).await;
        tokio::time::timeout(Duration::from_secs(1), scheduler.tick())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), executor.started.notified())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), executor.fast_finished.notified())
            .await
            .unwrap();
        clock.0.store((NOW + 180) as usize, Ordering::SeqCst);
        for _ in 0..3 {
            tokio::time::timeout(Duration::from_secs(1), scheduler.tick())
                .await
                .unwrap();
        }
        let doc = crate::parse_tasks(&fs.get(AUTOMATION_PATH).await.unwrap());
        let slow = doc
            .tasks
            .iter()
            .find(|task| task.id == "slow")
            .unwrap()
            .automation
            .as_ref()
            .unwrap();
        assert_eq!(
            slow.runs
                .iter()
                .filter(|run| run.status == crate::AutomationRunStatus::Running)
                .count(),
            1
        );
        assert_eq!(
            slow.runs
                .iter()
                .filter(|run| run.status == crate::AutomationRunStatus::Queued)
                .count(),
            1
        );
        tokio::time::timeout(Duration::from_secs(1), scheduler.stop())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(executor.dropped.load(Ordering::SeqCst), 1);
        assert!(scheduler.automation_runs.lock().await.is_empty());
        let doc = crate::parse_tasks(&fs.get(AUTOMATION_PATH).await.unwrap());
        let slow = doc
            .tasks
            .iter()
            .find(|task| task.id == "slow")
            .unwrap()
            .automation
            .as_ref()
            .unwrap();
        assert_eq!(
            slow.runs
                .iter()
                .filter(|run| run.status == crate::AutomationRunStatus::Running)
                .count(),
            0
        );
        assert_eq!(
            slow.runs
                .iter()
                .filter(|run| run.status == crate::AutomationRunStatus::Interrupted)
                .count(),
            1
        );
    }

    fn review_automation_file() -> Arc<MemFs> {
        MemFs::with(
            AUTOMATION_PATH,
            &serde_json::json!({"tasks":[{
                "id":"review-task", "cron":"* * * * *", "prompt":"run", "createdAt":(NOW-120)*1000,
                "recurring":true, "automation":{"version":2,"model":"provider/model"}
            }]})
            .to_string(),
        )
    }

    #[tokio::test]
    async fn stale_automation_claim_cannot_bind_or_finish_queued_or_reclaimed_run() {
        let fs = review_automation_file();
        let root = Path::new("/proj");
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(NOW);
        let first = crate::claim_automation_run(fs.as_ref(), root, "review-task", now, None)
            .await
            .unwrap();
        assert!(
            !crate::finish_automation_run(
                fs.as_ref(),
                root,
                &first,
                &Err("busy:target".into()),
                NOW * 1000
            )
            .await
        );
        assert!(
            !crate::finish_automation_run(
                fs.as_ref(),
                root,
                &first,
                &Err("interrupted:old host".into()),
                NOW * 1000
            )
            .await
        );
        assert!(
            crate::bind_automation_run_session(fs.as_ref(), root, &first, "stale-chat")
                .await
                .is_err()
        );
        let retry = crate::claim_automation_run(fs.as_ref(), root, "review-task", now, None)
            .await
            .unwrap();
        assert_eq!(first.run_id, retry.run_id);
        assert!(retry.claim_generation > first.claim_generation);
        assert!(
            !crate::finish_automation_run(
                fs.as_ref(),
                root,
                &first,
                &Err("interrupted:old host".into()),
                NOW * 1000
            )
            .await
        );
        assert!(
            crate::bind_automation_run_session(fs.as_ref(), root, &first, "stale-chat")
                .await
                .is_err()
        );
        let mut doc = crate::parse_tasks(&fs.get(AUTOMATION_PATH).await.unwrap());
        doc.tasks[0].automation.as_mut().unwrap().runs[0].owner_pid =
            Some(std::process::id().wrapping_add(1));
        crate::tasks_file::write_automation_tasks_body(
            fs.as_ref(),
            root,
            &crate::serialize_tasks(&doc),
        )
        .await
        .unwrap();
        assert!(
            !crate::finish_automation_run(
                fs.as_ref(),
                root,
                &retry,
                &Err("interrupted:wrong process".into()),
                NOW * 1000
            )
            .await
        );
        assert!(crate::bind_automation_run_session(
            fs.as_ref(),
            root,
            &retry,
            "wrong-process-chat"
        )
        .await
        .is_err());
        assert_eq!(
            crate::parse_tasks(&fs.get(AUTOMATION_PATH).await.unwrap()),
            doc
        );
    }

    struct BusyReviewFirer;
    #[async_trait]
    impl crate::CronJobFirer for BusyReviewFirer {
        async fn fire(&self, _: &str, _: &str) -> Result<String, String> {
            unreachable!()
        }
        async fn fire_automation(
            &self,
            _: &crate::AutomationRunRequest,
        ) -> Result<crate::AutomationRunResult, String> {
            Err("busy:target".into())
        }
    }

    #[tokio::test]
    async fn stop_preserves_busy_pending_work_and_peer_reclaimed_generation() {
        for reclaim in [false, true] {
            let fs = review_automation_file();
            let scheduler = CronScheduler::new(
                registry(fs.clone()),
                fs.clone(),
                Arc::new(AutomationClock(AtomicUsize::new(NOW as usize))),
                Arc::new(AutomationTestRuntime::default()),
                PathBuf::from(AUTOMATION_PATH),
            )
            .with_test_registry_delivery();
            scheduler
                .set_automation_firer(Arc::new(BusyReviewFirer))
                .await;
            scheduler.tick().await;
            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    let mut flights = scheduler.automation_runs.lock().await;
                    if flights.values_mut().all(|flight| !flight.is_running()) {
                        break;
                    }
                    drop(flights);
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            if reclaim {
                crate::claim_automation_run(
                    fs.as_ref(),
                    Path::new("/proj"),
                    "review-task",
                    SystemTime::UNIX_EPOCH + Duration::from_secs(NOW),
                    None,
                )
                .await
                .unwrap();
            }
            let before = fs.get(AUTOMATION_PATH).await.unwrap();
            scheduler.stop().await.unwrap();
            assert_eq!(fs.get(AUTOMATION_PATH).await.unwrap(), before);
        }
    }

    #[tokio::test]
    async fn bind_rechecks_expiration_after_claim_and_completes_without_session() {
        let fs = review_automation_file();
        let root = Path::new("/proj");
        let request = crate::claim_automation_run(
            fs.as_ref(),
            root,
            "review-task",
            SystemTime::UNIX_EPOCH + Duration::from_secs(NOW),
            None,
        )
        .await
        .unwrap();
        let mut doc = crate::parse_tasks(&fs.get(AUTOMATION_PATH).await.unwrap());
        // Simulate preparation crossing the expiry boundary without another tick.
        doc.tasks[0].expires_at = Some(1);
        crate::tasks_file::write_automation_tasks_body(
            fs.as_ref(),
            root,
            &crate::serialize_tasks(&doc),
        )
        .await
        .unwrap();
        let error =
            crate::bind_automation_run_session(fs.as_ref(), root, &request, "never-started")
                .await
                .unwrap_err();
        assert!(error.starts_with(crate::AUTOMATION_CANCELLED_PREFIX));
        let doc = crate::parse_tasks(&fs.get(AUTOMATION_PATH).await.unwrap());
        let config = doc.tasks[0].automation.as_ref().unwrap();
        assert_eq!(config.status, crate::AutomationStatus::Completed);
        assert_eq!(config.runs[0].status, crate::AutomationRunStatus::Cancelled);
        assert!(config.runs[0].session_id.is_none());
    }

    #[tokio::test]
    async fn repaired_configuration_is_not_paused_by_old_run_failure() {
        for repaired in [false, true] {
            let fs = review_automation_file();
            let root = Path::new("/proj");
            let request = crate::claim_automation_run(
                fs.as_ref(),
                root,
                "review-task",
                SystemTime::UNIX_EPOCH + Duration::from_secs(NOW),
                None,
            )
            .await
            .unwrap();
            // Coalescing changes last_fired_at but is not a user repair.
            assert!(crate::claim_automation_run(
                fs.as_ref(),
                root,
                "review-task",
                SystemTime::UNIX_EPOCH + Duration::from_secs(NOW + 180),
                None
            )
            .await
            .is_none());
            if repaired {
                let mut doc = crate::parse_tasks(&fs.get(AUTOMATION_PATH).await.unwrap());
                doc.tasks[0].automation.as_mut().unwrap().model = "provider/repaired".into();
                crate::tasks_file::write_automation_tasks_body(
                    fs.as_ref(),
                    root,
                    &crate::serialize_tasks(&doc),
                )
                .await
                .unwrap();
            }
            assert!(
                crate::finish_automation_run(
                    fs.as_ref(),
                    root,
                    &request,
                    &Err("paused:Old model unavailable".into()),
                    (NOW + 180) * 1000
                )
                .await
            );
            let doc = crate::parse_tasks(&fs.get(AUTOMATION_PATH).await.unwrap());
            let config = doc.tasks[0].automation.as_ref().unwrap();
            assert_eq!(
                config.status,
                if repaired {
                    crate::AutomationStatus::Active
                } else {
                    crate::AutomationStatus::Paused
                }
            );
            assert_eq!(config.runs[0].status, crate::AutomationRunStatus::Failed);
            assert_eq!(
                config
                    .runs
                    .iter()
                    .filter(|run| run.status == crate::AutomationRunStatus::Queued)
                    .count(),
                usize::from(repaired)
            );
        }
    }

    #[tokio::test]
    async fn stable_manual_occurrence_retries_once_and_rejects_terminal_redelivery() {
        let fs = review_automation_file();
        let root = Path::new("/proj");
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(NOW);
        let first = crate::claim_automation_run_now_at(fs.as_ref(), root, "review-task", now, 123)
            .await
            .unwrap();
        assert!(
            crate::claim_automation_run_now_at(fs.as_ref(), root, "review-task", now, 123)
                .await
                .is_none()
        );
        assert!(
            !crate::finish_automation_run(
                fs.as_ref(),
                root,
                &first,
                &Err("busy:target".into()),
                NOW * 1000
            )
            .await
        );
        let retry = crate::claim_automation_run_now_at(fs.as_ref(), root, "review-task", now, 123)
            .await
            .unwrap();
        assert_eq!(retry.run_id, first.run_id);
        assert_eq!(retry.claim_generation, first.claim_generation + 1);
        assert!(
            crate::finish_automation_run(
                fs.as_ref(),
                root,
                &retry,
                &Ok(crate::AutomationRunResult {
                    session_id: "chat".into(),
                    summary: "done".into()
                }),
                NOW * 1000
            )
            .await
        );
        assert!(
            crate::claim_automation_run_now_at(fs.as_ref(), root, "review-task", now, 123)
                .await
                .is_none()
        );
        assert!(
            crate::claim_automation_run_now_at(fs.as_ref(), root, "review-task", now, 124)
                .await
                .is_some()
        );
    }

    #[tokio::test]
    async fn legacy_manual_pending_is_adopted_once_without_changing_its_run_id() {
        // Equal clocks need a durable adoption marker too: a later different
        // token must not look like another first adoption of the legacy ID.
        for host_at in [123, NOW * 1000] {
            let fs = review_automation_file();
            let root = Path::new("/proj");
            let now = SystemTime::UNIX_EPOCH + Duration::from_secs(NOW);
            let legacy = crate::claim_automation_run_now(fs.as_ref(), root, "review-task", now)
                .await
                .unwrap();
            assert!(
                !crate::finish_automation_run(
                    fs.as_ref(),
                    root,
                    &legacy,
                    &Err("busy:target".into()),
                    NOW * 1000
                )
                .await
            );
            let adopted =
                crate::claim_automation_run_now_at(fs.as_ref(), root, "review-task", now, host_at)
                    .await
                    .unwrap();
            assert_eq!(adopted.run_id, legacy.run_id);
            let doc = crate::parse_tasks(&fs.get(AUTOMATION_PATH).await.unwrap());
            let run = &doc.tasks[0].automation.as_ref().unwrap().runs[0];
            assert_eq!(run.scheduled_at, NOW * 1000);
            assert_eq!(run.manual_occurrence_at, Some(host_at));
            assert!(
                !crate::finish_automation_run(
                    fs.as_ref(),
                    root,
                    &adopted,
                    &Err("busy:target".into()),
                    NOW * 1000
                )
                .await
            );
            assert!(crate::claim_automation_run_now_at(
                fs.as_ref(),
                root,
                "review-task",
                now,
                host_at + 1
            )
            .await
            .is_none());
            let retry =
                crate::claim_automation_run_now_at(fs.as_ref(), root, "review-task", now, host_at)
                    .await
                    .unwrap();
            assert_eq!(retry.run_id, legacy.run_id);
            assert!(
                crate::finish_automation_run(
                    fs.as_ref(),
                    root,
                    &retry,
                    &Ok(crate::AutomationRunResult {
                        session_id: "chat".into(),
                        summary: "done".into()
                    }),
                    NOW * 1000
                )
                .await
            );
            assert!(crate::claim_automation_run_now_at(
                fs.as_ref(),
                root,
                "review-task",
                now,
                host_at
            )
            .await
            .is_none());
        }
    }

    struct CommitReviewFirer {
        fs: Arc<MemFs>,
        failures: usize,
        calls: AtomicUsize,
        panic: bool,
    }
    #[async_trait]
    impl crate::CronJobFirer for CommitReviewFirer {
        async fn fire(&self, _: &str, _: &str) -> Result<String, String> {
            unreachable!()
        }
        async fn fire_automation(
            &self,
            _: &crate::AutomationRunRequest,
        ) -> Result<crate::AutomationRunResult, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert!(!self.panic, "injected executor panic");
            self.fs.fail_writes.store(self.failures, Ordering::SeqCst);
            Ok(crate::AutomationRunResult {
                session_id: "completed-chat".into(),
                summary: "actual result".into(),
            })
        }
    }
    async fn await_review_flights(scheduler: &CronScheduler) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let mut flights = scheduler.automation_runs.lock().await;
                if !flights.is_empty() && flights.values_mut().all(|flight| !flight.is_running()) {
                    return;
                }
                drop(flights);
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    fn commit_review_scheduler(fs: Arc<MemFs>) -> CronScheduler {
        CronScheduler::new(
            registry(fs.clone()),
            fs,
            Arc::new(AutomationClock(AtomicUsize::new(NOW as usize))),
            Arc::new(AutomationTestRuntime::default()),
            PathBuf::from(TASKS_PATH),
        )
    }

    #[tokio::test]
    async fn review_commit_retry_retains_actual_result_without_replaying_model() {
        let fs = review_automation_file();
        let executor = Arc::new(CommitReviewFirer {
            fs: fs.clone(),
            failures: 2,
            calls: AtomicUsize::new(0),
            panic: false,
        });
        let scheduler = commit_review_scheduler(fs.clone());
        scheduler.set_automation_firer(executor.clone()).await;
        scheduler.tick().await;
        await_review_flights(&scheduler).await;
        scheduler.tick().await;
        assert_eq!(
            scheduler.automation_runs.lock().await.len(),
            1,
            "retain uncommitted outcome"
        );
        scheduler.tick().await;
        let doc =
            crate::tasks_file::parse_automation_tasks(&fs.get(AUTOMATION_PATH).await.unwrap());
        let run = &doc.tasks[0].automation.as_ref().unwrap().runs[0];
        assert_eq!(run.status, crate::AutomationRunStatus::Succeeded);
        assert_eq!(run.summary.as_deref(), Some("actual result"));
        assert_eq!(run.session_id.as_deref(), Some("completed-chat"));
        assert_eq!(run.finished_at, Some(NOW * 1000));
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        scheduler.stop().await.unwrap();
    }

    #[tokio::test]
    async fn review_commit_stop_retries_original_result_and_reports_persistence_failure() {
        let fs = review_automation_file();
        let executor = Arc::new(CommitReviewFirer {
            fs: fs.clone(),
            failures: 2,
            calls: AtomicUsize::new(0),
            panic: false,
        });
        let scheduler = commit_review_scheduler(fs.clone());
        scheduler.set_automation_firer(executor.clone()).await;
        scheduler.tick().await;
        await_review_flights(&scheduler).await;
        assert!(scheduler.stop().await.is_err());
        assert_eq!(scheduler.automation_runs.lock().await.len(), 1);
        scheduler.stop().await.unwrap();
        let doc =
            crate::tasks_file::parse_automation_tasks(&fs.get(AUTOMATION_PATH).await.unwrap());
        let run = &doc.tasks[0].automation.as_ref().unwrap().runs[0];
        assert_eq!(run.status, crate::AutomationRunStatus::Succeeded);
        assert_eq!(run.summary.as_deref(), Some("actual result"));
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn review_commit_panicked_flight_is_reconciled_before_its_owner_is_removed() {
        let fs = review_automation_file();
        let scheduler = commit_review_scheduler(fs.clone());
        scheduler
            .set_automation_firer(Arc::new(CommitReviewFirer {
                fs: fs.clone(),
                failures: 0,
                calls: AtomicUsize::new(0),
                panic: true,
            }))
            .await;
        scheduler.tick().await;
        await_review_flights(&scheduler).await;
        scheduler.tick().await;
        let doc =
            crate::tasks_file::parse_automation_tasks(&fs.get(AUTOMATION_PATH).await.unwrap());
        assert_eq!(
            doc.tasks[0].automation.as_ref().unwrap().runs[0].status,
            crate::AutomationRunStatus::Interrupted
        );
        scheduler.stop().await.unwrap();
    }

    #[tokio::test]
    async fn review_commit_old_completion_does_not_complete_new_future_one_shot() {
        let fs = review_automation_file();
        let root = Path::new("/proj");
        let request = crate::claim_automation_run(
            fs.as_ref(),
            root,
            "review-task",
            SystemTime::UNIX_EPOCH + Duration::from_secs(NOW),
            None,
        )
        .await
        .unwrap();
        let mut doc =
            crate::tasks_file::parse_automation_tasks(&fs.get(AUTOMATION_PATH).await.unwrap());
        doc.tasks[0].recurring = None;
        doc.tasks[0].cron = "0 9 * * *".into();
        doc.tasks[0].prompt = "new one-shot instructions".into();
        crate::tasks_file::write_automation_tasks_body(
            fs.as_ref(),
            root,
            &crate::serialize_tasks(&doc),
        )
        .await
        .unwrap();
        assert!(
            crate::finish_automation_run(
                fs.as_ref(),
                root,
                &request,
                &Ok(crate::AutomationRunResult {
                    session_id: "chat".into(),
                    summary: "old recurring result".into()
                }),
                NOW * 1000
            )
            .await
        );
        let doc =
            crate::tasks_file::parse_automation_tasks(&fs.get(AUTOMATION_PATH).await.unwrap());
        let config = doc.tasks[0].automation.as_ref().unwrap();
        assert_eq!(config.status, crate::AutomationStatus::Active);
        assert_eq!(config.runs[0].status, crate::AutomationRunStatus::Succeeded);
    }

    #[tokio::test]
    async fn review_commit_waits_for_host_drain_before_marking_interrupted() {
        struct DrainFirer {
            started: tokio::sync::Notify,
            cancels: AtomicUsize,
        }
        #[async_trait]
        impl crate::CronJobFirer for DrainFirer {
            async fn fire(&self, _: &str, _: &str) -> Result<String, String> {
                unreachable!()
            }
            async fn fire_automation(
                &self,
                _: &crate::AutomationRunRequest,
            ) -> Result<crate::AutomationRunResult, String> {
                self.started.notify_one();
                std::future::pending().await
            }
            async fn cancel_run(&self, _: &str) -> Result<(), String> {
                if self.cancels.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err("native writer is still draining".into())
                } else {
                    Ok(())
                }
            }
        }
        let fs = review_automation_file();
        let scheduler = commit_review_scheduler(fs.clone());
        let firer = Arc::new(DrainFirer {
            started: tokio::sync::Notify::new(),
            cancels: AtomicUsize::new(0),
        });
        scheduler.set_automation_firer(firer.clone()).await;
        scheduler.tick().await;
        firer.started.notified().await;
        assert!(scheduler.stop().await.is_err());
        let doc =
            crate::tasks_file::parse_automation_tasks(&fs.get(AUTOMATION_PATH).await.unwrap());
        assert_eq!(
            doc.tasks[0].automation.as_ref().unwrap().runs[0].status,
            crate::AutomationRunStatus::Running
        );
        // Rebinding the scheduler cannot substitute a new executor for an old
        // flight's destruction barrier.
        scheduler
            .set_automation_firer(Arc::new(BusyReviewFirer))
            .await;
        scheduler.stop().await.unwrap();
        assert_eq!(firer.cancels.load(Ordering::SeqCst), 2);
        let doc =
            crate::tasks_file::parse_automation_tasks(&fs.get(AUTOMATION_PATH).await.unwrap());
        assert_eq!(
            doc.tasks[0].automation.as_ref().unwrap().runs[0].status,
            crate::AutomationRunStatus::Interrupted
        );
    }
}
