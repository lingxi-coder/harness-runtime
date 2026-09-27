//! Tick-loop-free, single-shot cron firing core.
//!
//! The desktop [`crate::scheduler::CronScheduler`] owns a long-lived one-second
//! tick loop and fires due jobs as `TaskType::Dream` subagents through a
//! [`tasks::TaskRegistry`]. Mobile hosts cannot run that loop — a backgrounded
//! Android process is killed, and the mobile engine binds no `TaskRegistry` /
//! subagent spawner. Instead, an OS scheduler (Android `AlarmManager`) wakes the
//! app and asks the engine to evaluate the persisted tasks file ONCE and fire
//! whatever is due.
//!
//! This module is that single-shot operation. It REUSES the exact pure
//! due-detection + file-bookkeeping helpers the desktop tick loop uses
//! (`is_job_due`, `finalize_fired_job`, `tasks_file_with_last_fired`,
//! `tasks_file_without`, `is_recurring_task_aged` from [`crate::scheduler`], and
//! [`crate::schedule::CronExpression::next_match_after`]), so the semantics —
//! epoch-millisecond timestamps, missed-run catch-up-once, one-shot auto-delete,
//! recurring `lastFiredAt` persistence, deterministic jitter/cache lead, and
//! seven-day recurring auto-expiry — are 1:1 with desktop. The single-shot path
//! takes no per-job A9 firing lock (a phone is single process and the foreground
//! service is the only firer); every scheduled-task file mutation still takes
//! the shared root-confined cross-process lock.
//!
//! Firing itself is abstracted behind [`CronJobFirer`] (`fire(prompt) -> result`)
//! so the same core serves desktop (a Dream subagent) and mobile (a fresh
//! orchestrator turn).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use platform_api::{Clock, FileSystem};

use crate::schedule::parse_cron;
use crate::scheduler::{
    finalize_fired_job, is_job_due, is_recurring_task_aged, next_fire_time,
    tasks_file_with_last_fired, tasks_file_without, CronTaskDef,
};
use crate::tasks_file::parse_tasks;

/// Process-global serialization for ALL `scheduled_tasks.json` read-modify-write
/// mutations. The mobile single-shot firing path took no A9 cross-process lock
/// (one app process), but the firing pass and the management-UI `cron_create` /
/// `cron_delete` FFI run on DIFFERENT engine handles / tokio runtimes in the SAME
/// process, so without this an in-process lost-update could clobber a new/deleted
/// job or a `lastFiredAt` write-back. Held only across the brief RMW — NEVER
/// during a (≤180s) fire — so a UI edit never blocks for the length of a turn.
static CRON_FILE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Acquire the process-global cron-file lock ([`CRON_FILE_LOCK`]). FFI writers
/// (`cron_create` / `cron_delete`) hold this across their own read-modify-write so
/// they cannot interleave with a concurrent firing pass's write-back.
pub async fn lock_cron_file() -> tokio::sync::MutexGuard<'static, ()> {
    CRON_FILE_LOCK.lock().await
}

/// Caller-supplied seam: run one due job's `prompt` to completion and return a
/// textual result to surface (e.g. an Android notification body).
///
/// Generic over "fire(prompt) -> result" so the firing core is host-agnostic:
/// desktop adapts it to a Dream subagent, mobile to a fresh orchestrator turn.
#[async_trait]
pub trait CronJobFirer: Send + Sync {
    /// Fire `prompt` for job `id`. `Ok(result_text)` on success; `Err(message)`
    /// on failure (surfaced as [`FireStatus::Failed`]).
    async fn fire(&self, id: &str, prompt: &str) -> Result<String, String>;

    /// Cancel and join host-owned execution resources after the outer firing
    /// future is dropped. Errors retain scheduler ownership for a later retry.
    async fn cancel_run(&self, _run_id: &str) -> Result<(), String> {
        Ok(())
    }

    /// Hosts must explicitly implement session-aware execution for v2 tasks.
    async fn fire_automation(
        &self,
        _request: &crate::AutomationRunRequest,
    ) -> Result<crate::AutomationRunResult, String> {
        Err(format!(
            "{}This host does not support scheduled session execution",
            crate::AUTOMATION_PAUSED_PREFIX
        ))
    }
}

/// Terminal status of one fired job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FireStatus {
    /// The job's turn completed.
    Ok,
    /// The job's turn failed; carries a log-safe message.
    Failed(String),
}

/// One fired-job record returned by [`run_due_jobs`]. Mobile lifts these to FFI
/// DTOs for the notification surface.
#[derive(Debug, Clone)]
pub struct FiredJob {
    /// The job id that fired.
    pub id: String,
    /// The prompt that was run.
    pub prompt: String,
    /// The final assistant text, if the firer produced any.
    pub result_text: Option<String>,
    /// Terminal status.
    pub status: FireStatus,
}

/// Evaluate the persisted tasks file ONCE and fire every due job via `firer`,
/// applying the EXACT desktop post-fire bookkeeping (recurring `lastFiredAt`
/// write-back, one-shot auto-delete, aged-recurring auto-expiry). Returns one
/// [`FiredJob`] per job that fired this call.
///
/// `recurring_max_age` mirrors [`crate::scheduler::CronScheduler`]'s field
/// (`None` on both hosts; an explicit value enables
/// expiry). A missing / unparseable tasks file fires nothing. Unlike the desktop
/// tick loop, firing takes no per-job lock; deterministic fire-time jitter is
/// already included by the shared due predicate. Persistence mutations remain
/// serialized in-process and cross-process.
pub async fn run_due_jobs(
    tasks_file: &Path,
    fs: Arc<dyn FileSystem>,
    clock: Arc<dyn Clock>,
    firer: &dyn CronJobFirer,
    recurring_max_age: Option<Duration>,
) -> Vec<FiredJob> {
    let mut automation_fired =
        crate::automation::run_due_automations(tasks_file, fs.clone(), clock.clone(), firer).await;
    let now = clock.now();
    let Some(project_root) = crate::tasks_file::project_root_from_tasks_path(tasks_file) else {
        return automation_fired;
    };

    // Load the persisted (durable) jobs into a transient in-memory map, mirroring
    // `CronScheduler::load_persisted` (epoch-ms timestamps; invalid cron skipped).
    let Ok(body) = crate::tasks_file::read_tasks_body(fs.as_ref(), project_root).await else {
        return automation_fired; // file absent → no session cron to fire
    };
    let mut tasks: HashMap<String, CronTaskDef> = HashMap::new();
    let mut permanent_ids = HashSet::new();
    for t in parse_tasks(&body).tasks {
        if t.automation.is_some() {
            continue;
        }
        if t.expires_at
            .is_some_and(|expiry| expiry <= system_time_to_epoch_ms(now))
        {
            remove_task_from_file(
                fs.as_ref(),
                project_root,
                &t.id,
                Some(system_time_to_epoch_ms(now)),
            )
            .await;
            continue;
        }
        let schedule = match parse_cron(&t.cron) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("cron: skipping job {} with invalid schedule: {e}", t.id);
                continue;
            }
        };
        let created_at = SystemTime::UNIX_EPOCH + Duration::from_millis(t.created_at);
        let last_run = t
            .last_fired_at
            .map(|ms| SystemTime::UNIX_EPOCH + Duration::from_millis(ms));
        if t.permanent == Some(true) {
            permanent_ids.insert(t.id.clone());
        }
        tasks.insert(
            t.id.clone(),
            CronTaskDef {
                id: t.id,
                schedule,
                prompt: t.prompt,
                agent_type: None,
                last_run,
                enabled: true,
                created_at,
                recurring: t.recurring.unwrap_or(false),
            },
        );
    }

    // (1) Due-detection with missed-run catch-up (`is_job_due`). Age is not
    // checked here: Claude Code lets an aged recurring job keep waiting until
    // its next due time, then gives it one final fire before deletion.
    let due_ids: Vec<String> = tasks
        .values()
        .filter(|t| is_job_due(t, now))
        .map(|t| t.id.clone())
        .collect();

    // (2) Fire each due job, then apply post-fire bookkeeping identically to
    //     `CronScheduler::tick` (one-shot auto-delete vs recurring lastFiredAt).
    let now_ms = system_time_to_epoch_ms(now);
    let mut fired = Vec::with_capacity(due_ids.len());
    for id in due_ids {
        let prompt = tasks.get(&id).map(|t| t.prompt.clone()).unwrap_or_default();
        let (status, result_text) = match firer.fire(&id, &prompt).await {
            Ok(text) => (FireStatus::Ok, Some(text)),
            Err(message) => (FireStatus::Failed(message), None),
        };

        let expires_after_fire = tasks.get(&id).is_some_and(|task| {
            !permanent_ids.contains(&id)
                && is_recurring_task_aged(now, task.created_at, task.recurring, recurring_max_age)
        });
        let remove_after_fire = finalize_fired_job(&mut tasks, &id, now) || expires_after_fire;
        if expires_after_fire {
            tasks.remove(&id);
        }
        if remove_after_fire {
            remove_task_from_file(fs.as_ref(), project_root, &id, None).await;
            if expires_after_fire {
                tracing::info!(
                    event = "tengu_scheduled_task_expired",
                    cron_id = %id,
                    "cron job fired its final run after exceeding the recurring max age"
                );
            } else {
                tracing::info!(cron_id = %id, "one-shot cron job fired and auto-deleted");
            }
        } else {
            set_last_fired_in_file(fs.as_ref(), project_root, &id, now_ms).await;
        }

        fired.push(FiredJob {
            id,
            prompt,
            result_text,
            status,
        });
    }
    automation_fired.extend(fired);
    automation_fired
}

/// The earliest next fire across all persisted ENABLED jobs, as epoch
/// **milliseconds**, or `None` if there are no jobs / none ever fire again. The
/// host arms its next OS alarm at this instant. The anchor for each job is
/// `lastFiredAt ?? createdAt`; the returned instant includes the same
/// deterministic recurring/one-shot jitter and cache-lead exception as the
/// desktop scheduler. An overdue job yields a past instant, so the host fires
/// immediately and re-arms.
pub async fn next_fire_epoch_ms(
    tasks_file: &Path,
    fs: Arc<dyn FileSystem>,
    clock: Arc<dyn Clock>,
) -> Option<u64> {
    let now = clock.now();
    let project_root = crate::tasks_file::project_root_from_tasks_path(tasks_file)?;
    // Which of the two stores this path names — session cron under `.claude/`,
    // or the v2 task centre under `branding::DOT_DIR`. Decided once; the body
    // and the parser must come from the SAME store or they disagree about what
    // is scheduled.
    let session_store = tasks_file.parent()?.file_name()? == std::ffi::OsStr::new(".claude");
    let body = if session_store {
        crate::tasks_file::read_tasks_body(fs.as_ref(), project_root).await
    } else {
        crate::tasks_file::read_automation_tasks_body(fs.as_ref(), project_root).await
    }
    .ok()?;

    // The earliest jittered next-fire across all tasks. Each task is scored by
    // the SAME per-task helper the management-UI display uses, so the armed alarm
    // (this value) and the displayed per-task next fire cannot drift apart.
    let mut earliest: Option<u64> = None;
    let document = if session_store {
        parse_tasks(&body)
    } else {
        crate::tasks_file::parse_automation_tasks(&body)
    };
    for t in document.tasks {
        if t.automation
            .as_ref()
            .is_some_and(|a| a.status != crate::AutomationStatus::Active)
        {
            continue;
        }
        if let Some(queued) = t.automation.as_ref().and_then(|a| {
            a.runs
                .iter()
                .filter(|r| r.status == crate::AutomationRunStatus::Queued)
                .map(|r| r.scheduled_at)
                .min()
        }) {
            if t.expires_at
                .is_none_or(|expiry| expiry > system_time_to_epoch_ms(now))
            {
                earliest = Some(earliest.map_or(queued, |e| e.min(queued)));
                continue;
            }
        }

        if let Some(next) = next_fire_epoch_ms_for_persisted_task(&t, now) {
            if t.expires_at
                .is_none_or(|expiry| expiry > system_time_to_epoch_ms(now) && next < expiry)
            {
                earliest = Some(earliest.map_or(next, |e| e.min(next)));
            }
        }
    }
    earliest
}

/// Compute the next occurrence using versioned task semantics. V2 reactivation
/// records its future-only schedule anchor in `last_fired_at`; creation time and
/// historical run IDs remain unchanged. Legacy one-shots retain their original
/// creation-based catch-up behavior.
#[must_use]
pub fn next_fire_epoch_ms_for_persisted_task(
    task: &crate::CronTask,
    now: SystemTime,
) -> Option<u64> {
    let recurring = task.recurring.unwrap_or(false);
    let resumed_one_shot = task.automation.is_some() && !recurring;
    let anchor = if resumed_one_shot {
        task.last_fired_at
            .unwrap_or(task.created_at)
            .max(task.created_at)
    } else {
        task.created_at
    };
    let next = next_fire_epoch_ms_for_task(
        &task.id,
        &task.cron,
        anchor,
        task.last_fired_at,
        recurring,
        now,
    )?;
    // One-shot cache lead may clamp to the anchor. Reactivation must still
    // schedule a future occurrence, so use the nominal match in that case.
    if resumed_one_shot && task.last_fired_at.is_some() && next <= anchor {
        return parse_cron(&task.cron)
            .ok()?
            .next_match_after(SystemTime::UNIX_EPOCH + Duration::from_millis(anchor))
            .map(system_time_to_epoch_ms);
    }
    Some(next)
}

/// The jittered next-fire time of ONE task, epoch **milliseconds** — the same
/// computation [`next_fire_epoch_ms`] applies per task ([`next_fire_time`],
/// which includes Claude Code's recurring/one-shot jitter + cache-lead), exposed
/// so a management-UI list can show the instant the job will ACTUALLY fire (the
/// armed-alarm time), not the un-jittered nominal schedule. A caller that shows
/// the raw schedule would display a time that disagrees with the scheduler's
/// alarm by up to [`DEFAULT_RECURRING_JITTER_CAP`].
///
/// `created_at_ms` / `last_fired_at_ms` are the persisted task fields; `now` is
/// the fallback anchor when the task carries no creation time. Returns `None`
/// for an unparseable expression or a schedule that never fires again.
#[must_use]
pub fn next_fire_epoch_ms_for_task(
    id: &str,
    cron: &str,
    created_at_ms: u64,
    last_fired_at_ms: Option<u64>,
    recurring: bool,
    now: SystemTime,
) -> Option<u64> {
    // `id` and `recurring` are load-bearing: the jitter fraction is derived from
    // the id and the recurring/one-shot arms differ, so passing placeholders
    // would diverge from the scheduler's own per-task computation.
    let schedule = parse_cron(cron).ok()?;
    let created_at = if created_at_ms > 0 {
        SystemTime::UNIX_EPOCH + Duration::from_millis(created_at_ms)
    } else {
        now
    };
    let last_run = last_fired_at_ms
        .filter(|ms| *ms > 0)
        .map(|ms| SystemTime::UNIX_EPOCH + Duration::from_millis(ms));
    let task = CronTaskDef {
        id: id.to_string(),
        schedule,
        prompt: String::new(),
        agent_type: None,
        last_run,
        enabled: true,
        created_at,
        recurring,
    };
    next_fire_time(&task).map(system_time_to_epoch_ms)
}

/// `SystemTime` → epoch **milliseconds** (the on-disk / alarm unit). A
/// pre-epoch instant clamps to 0.
#[must_use]
fn system_time_to_epoch_ms(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Remove `id` from the single tasks file (read-modify-write via `fs`). A missing
/// file / id is a no-op. Mirrors `CronScheduler::remove_task_from_file`.
async fn remove_task_from_file(
    fs: &dyn FileSystem,
    project_root: &Path,
    id: &str,
    expired_at: Option<u64>,
) {
    let _guard = CRON_FILE_LOCK.lock().await;
    let Ok(_file_guard) = crate::tasks_file::lock_scheduled_tasks(fs, project_root).await else {
        return;
    };
    if let Ok(body) = crate::tasks_file::read_tasks_body(fs, project_root).await {
        if let Some(now) = expired_at {
            if !parse_tasks(&body)
                .tasks
                .iter()
                .any(|task| task.id == id && task.expires_at.is_some_and(|expiry| expiry <= now))
            {
                return;
            }
        }
        if let Some(updated) = tasks_file_without(&body, id) {
            let _ = crate::tasks_file::write_tasks_body(fs, project_root, &updated).await;
        }
    }
}

/// Set `lastFiredAt` (epoch ms) on `id` in the single tasks file (read-modify-
/// write via `fs`). A missing file / id is a no-op. Mirrors
/// `CronScheduler::set_last_fired_in_file`.
async fn set_last_fired_in_file(
    fs: &dyn FileSystem,
    project_root: &Path,
    id: &str,
    last_fired_at_ms: u64,
) {
    let _guard = CRON_FILE_LOCK.lock().await;
    let Ok(_file_guard) = crate::tasks_file::lock_scheduled_tasks(fs, project_root).await else {
        return;
    };
    if let Ok(body) = crate::tasks_file::read_tasks_body(fs, project_root).await {
        if let Some(updated) = tasks_file_with_last_fired(&body, id, last_fired_at_ms) {
            let _ = crate::tasks_file::write_tasks_body(fs, project_root, &updated).await;
        }
    }
}

/// Default for Claude-compatible cron jobs; task-center v2 automations bypass it.
#[must_use]
pub const fn default_recurring_max_age() -> Option<Duration> {
    Some(crate::scheduler::DEFAULT_RECURRING_MAX_AGE)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use futures::Stream;
    use platform_api::filesystem::{FileContent, FileEvent, FlockGuard, FsError};
    use std::pin::Pin;
    use std::sync::Mutex as StdMutex;
    use tokio::sync::Mutex as TokioMutex;

    // ---- Minimal in-memory FileSystem (only read/write/delete are exercised) ----
    struct MemFs {
        files: TokioMutex<HashMap<String, String>>,
    }
    struct MemFlockGuard(String);
    impl FlockGuard for MemFlockGuard {
        fn path(&self) -> &str {
            &self.0
        }
    }
    impl MemFs {
        fn with(path: &str, body: &str) -> Arc<Self> {
            let mut m = HashMap::new();
            m.insert(path.to_string(), body.to_string());
            Arc::new(Self {
                files: TokioMutex::new(m),
            })
        }
        fn empty() -> Arc<Self> {
            Arc::new(Self {
                files: TokioMutex::new(HashMap::new()),
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
                Some(content) => {
                    let total_lines = content.lines().count() as u64;
                    Ok(FileContent {
                        content: content.clone(),
                        truncated: false,
                        total_lines,
                    })
                }
                None => Err(FsError::NotFound(path.to_string())),
            }
        }
        async fn write_file(&self, path: &str, content: &str) -> Result<(), FsError> {
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
                .map_or(0, |s| s.len() as u64))
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

    // ---- Fixed clock (local, to avoid a test-harness dep) ----
    struct FixedClock(SystemTime);
    impl FixedClock {
        fn at_secs(s: u64) -> Arc<Self> {
            Arc::new(Self(SystemTime::UNIX_EPOCH + Duration::from_secs(s)))
        }
    }
    impl Clock for FixedClock {
        fn now(&self) -> SystemTime {
            self.0
        }
    }

    // ---- Recording firer ----
    struct RecordingFirer {
        canned: String,
        calls: StdMutex<Vec<(String, String)>>,
    }
    impl RecordingFirer {
        fn new(canned: &str) -> Self {
            Self {
                canned: canned.to_string(),
                calls: StdMutex::new(Vec::new()),
            }
        }
        fn calls(&self) -> Vec<(String, String)> {
            self.calls.lock().unwrap().clone()
        }
    }
    #[async_trait]
    impl CronJobFirer for RecordingFirer {
        async fn fire(&self, id: &str, prompt: &str) -> Result<String, String> {
            self.calls
                .lock()
                .unwrap()
                .push((id.to_string(), prompt.to_string()));
            Ok(self.canned.clone())
        }
    }

    const NOW: u64 = 1_700_000_000; // not on a minute boundary (…020 seconds)
    const PATH: &str = "/proj/.claude/scheduled_tasks.json";
    const AUTOMATION_PATH: &str = "/proj/.lingxi/scheduled_tasks.json";

    fn file_with(tasks_json: &str) -> Arc<MemFs> {
        MemFs::with(PATH, tasks_json)
    }

    #[tokio::test]
    async fn fires_due_recurring_job_and_writes_last_fired() {
        // Recurring `* * * * *`, created 2 minutes ago, never fired → due now.
        let created_ms = (NOW - 120) * 1000;
        let body = format!(
            r#"{{"tasks":[{{"id":"d11111111","cron":"* * * * *","prompt":"hello","createdAt":{created_ms},"recurring":true}}]}}"#
        );
        let fs = file_with(&body);
        let firer = RecordingFirer::new("done");

        let fired = run_due_jobs(
            Path::new(PATH),
            fs.clone(),
            FixedClock::at_secs(NOW),
            &firer,
            Some(crate::scheduler::DEFAULT_RECURRING_MAX_AGE),
        )
        .await;

        assert_eq!(fired.len(), 1, "the due recurring job fires");
        assert_eq!(fired[0].id, "d11111111");
        assert_eq!(fired[0].prompt, "hello");
        assert_eq!(fired[0].result_text.as_deref(), Some("done"));
        assert_eq!(fired[0].status, FireStatus::Ok);
        assert_eq!(firer.calls(), vec![("d11111111".into(), "hello".into())]);

        // lastFiredAt (epoch ms) was written back; the job is still present.
        let after = parse_tasks(&fs.get(PATH).await.unwrap());
        assert_eq!(after.tasks.len(), 1);
        assert_eq!(after.tasks[0].last_fired_at, Some(NOW * 1000));
    }

    #[tokio::test]
    async fn one_shot_job_is_deleted_after_firing() {
        // A one-shot (recurring:false) job that is due → fires once, then removed.
        let created_ms = (NOW - 120) * 1000;
        let body = format!(
            r#"{{"tasks":[{{"id":"d22222222","cron":"* * * * *","prompt":"once","createdAt":{created_ms}}}]}}"#
        );
        let fs = file_with(&body);
        let firer = RecordingFirer::new("ok");

        let fired = run_due_jobs(
            Path::new(PATH),
            fs.clone(),
            FixedClock::at_secs(NOW),
            &firer,
            Some(crate::scheduler::DEFAULT_RECURRING_MAX_AGE),
        )
        .await;

        assert_eq!(fired.len(), 1);
        assert_eq!(firer.calls().len(), 1);
        // The one-shot was auto-deleted from the file.
        let after = parse_tasks(&fs.get(PATH).await.unwrap());
        assert!(after.tasks.is_empty(), "one-shot job removed after firing");
    }

    #[tokio::test]
    async fn job_not_yet_due_does_not_fire() {
        // Recurring `* * * * *` fired THIS minute → next run is next minute → not due.
        let body = format!(
            r#"{{"tasks":[{{"id":"d33333333","cron":"* * * * *","prompt":"p","createdAt":{c},"recurring":true,"lastFiredAt":{f}}}]}}"#,
            c = (NOW - 600) * 1000,
            f = NOW * 1000,
        );
        let fs = file_with(&body);
        let firer = RecordingFirer::new("x");

        let fired = run_due_jobs(
            Path::new(PATH),
            fs.clone(),
            FixedClock::at_secs(NOW),
            &firer,
            Some(crate::scheduler::DEFAULT_RECURRING_MAX_AGE),
        )
        .await;

        assert!(
            fired.is_empty(),
            "a job already fired this minute is not due"
        );
        assert!(firer.calls().is_empty());
    }

    #[tokio::test]
    async fn aged_due_recurring_job_fires_final_run_then_is_deleted() {
        let created_ms = (NOW - 8 * 24 * 60 * 60) * 1000;
        let last_fired_ms = (NOW - 120) * 1000;
        let body = format!(
            r#"{{"tasks":[{{"id":"00000000","cron":"* * * * *","prompt":"final","createdAt":{created_ms},"recurring":true,"lastFiredAt":{last_fired_ms}}}]}}"#
        );
        let fs = file_with(&body);
        let firer = RecordingFirer::new("done");

        let fired = run_due_jobs(
            Path::new(PATH),
            fs.clone(),
            FixedClock::at_secs(NOW),
            &firer,
            Some(crate::scheduler::DEFAULT_RECURRING_MAX_AGE),
        )
        .await;

        assert_eq!(fired.len(), 1, "an aged due job gets one final fire");
        assert_eq!(firer.calls(), vec![("00000000".into(), "final".into())]);
        assert!(
            parse_tasks(&fs.get(PATH).await.unwrap()).tasks.is_empty(),
            "the final fire removes the aged recurring descriptor"
        );
    }

    #[tokio::test]
    async fn aged_recurring_job_not_yet_due_is_retained() {
        let created_ms = (NOW - 8 * 24 * 60 * 60) * 1000;
        let body = format!(
            r#"{{"tasks":[{{"id":"00000000","cron":"* * * * *","prompt":"later","createdAt":{created_ms},"recurring":true,"lastFiredAt":{last}}}]}}"#,
            last = NOW * 1000,
        );
        let fs = file_with(&body);
        let firer = RecordingFirer::new("unused");

        let fired = run_due_jobs(
            Path::new(PATH),
            fs.clone(),
            FixedClock::at_secs(NOW),
            &firer,
            Some(crate::scheduler::DEFAULT_RECURRING_MAX_AGE),
        )
        .await;

        assert!(fired.is_empty());
        assert!(firer.calls().is_empty());
        assert_eq!(
            parse_tasks(&fs.get(PATH).await.unwrap()).tasks.len(),
            1,
            "age alone must not delete a recurring job before its due time"
        );
    }

    #[tokio::test]
    async fn missing_file_fires_nothing() {
        let fs = MemFs::empty();
        let firer = RecordingFirer::new("x");
        let fired = run_due_jobs(
            Path::new(PATH),
            fs,
            FixedClock::at_secs(NOW),
            &firer,
            Some(crate::scheduler::DEFAULT_RECURRING_MAX_AGE),
        )
        .await;
        assert!(fired.is_empty());
        assert!(firer.calls().is_empty());
    }

    #[tokio::test]
    async fn next_fire_epoch_ms_returns_earliest_in_ms() {
        // Two `* * * * *` jobs created now → both next-fire at the next minute
        // boundary; the earliest is that boundary, strictly after NOW, in ms.
        let created_ms = NOW * 1000;
        let body = format!(
            r#"{{"tasks":[
                {{"id":"da","cron":"* * * * *","prompt":"a","createdAt":{created_ms},"recurring":true}},
                {{"id":"db","cron":"* * * * *","prompt":"b","createdAt":{created_ms},"recurring":true}}
            ]}}"#
        );
        let fs = file_with(&body);
        let next = next_fire_epoch_ms(Path::new(PATH), fs, FixedClock::at_secs(NOW))
            .await
            .expect("a next fire exists");
        // Next whole minute strictly after NOW (…020s) is …040s → ms.
        let expected = ((NOW / 60 + 1) * 60) * 1000;
        assert_eq!(next, expected);
        assert!(next > NOW * 1000);
    }

    #[tokio::test]
    async fn next_fire_epoch_ms_none_when_empty() {
        let fs = file_with(r#"{"tasks":[]}"#);
        assert_eq!(
            next_fire_epoch_ms(Path::new(PATH), fs, FixedClock::at_secs(NOW)).await,
            None
        );
    }
    #[tokio::test]
    async fn legacy_mobile_cron_ignores_task_center_expiration() {
        for expired in [false, true] {
            let created_ms = (NOW - 30 * 24 * 60 * 60) * 1000;
            let expiry = if expired {
                format!(",\"expiresAt\":{}", (NOW - 1) * 1000)
            } else {
                String::new()
            };
            let body = format!(
                r#"{{"tasks":[{{"id":"dmobile01","cron":"* * * * *","prompt":"test","createdAt":{created_ms},"recurring":true{expiry}}}]}}"#
            );
            let fs = file_with(&body);
            let firer = RecordingFirer::new("done");
            let fired = run_due_jobs(
                Path::new(PATH),
                fs,
                FixedClock::at_secs(NOW),
                &firer,
                default_recurring_max_age(),
            )
            .await;
            assert_eq!(fired.len(), 1);
        }
    }
    fn v2_file(status: &str, recurring: bool) -> Arc<MemFs> {
        MemFs::with(AUTOMATION_PATH, &serde_json::json!({"tasks":[{"id":"dv2", "cron":"* * * * *", "prompt":"v2 prompt", "createdAt":(NOW-120)*1000, "recurring":recurring, "automation":{"version":2,"name":"Test task","status":status,"model":"provider/model","reasoning":{"kind":"level","id":"high"},"runMode":"task_session","notificationPolicy":"all"}}]}).to_string())
    }
    #[tokio::test]
    async fn v2_lifecycle_and_model_snapshot_survive_claim_and_completion() {
        let fs = v2_file("active", false);
        let root = Path::new("/proj");
        let now = FixedClock::at_secs(NOW).now();
        let request = crate::claim_automation_run(fs.as_ref(), root, "dv2", now, None)
            .await
            .unwrap();
        let config = request.task.automation.as_ref().unwrap();
        assert_eq!(config.model, "provider/model");
        assert_eq!(config.reasoning["id"], "high");
        assert_eq!(config.name.as_deref(), Some("Test task"));
        assert!(
            crate::claim_automation_run(fs.as_ref(), root, "dv2", now, None)
                .await
                .is_none()
        );
        crate::bind_automation_run_session(fs.as_ref(), root, &request, "owned-chat")
            .await
            .unwrap();
        let bound = parse_tasks(&fs.get(AUTOMATION_PATH).await.unwrap())
            .tasks
            .remove(0)
            .automation
            .unwrap();
        assert_eq!(bound.owned_session_id.as_deref(), Some("owned-chat"));
        assert_eq!(bound.runs[0].session_id.as_deref(), Some("owned-chat"));
        let result = Ok(crate::AutomationRunResult {
            session_id: "owned-chat".into(),
            summary: "result".into(),
        });
        assert!(
            crate::finish_automation_run(fs.as_ref(), root, &request, &result, NOW * 1000).await
        );
        assert!(
            !crate::finish_automation_run(fs.as_ref(), root, &request, &result, NOW * 1000).await
        );
        let task = parse_tasks(&fs.get(AUTOMATION_PATH).await.unwrap())
            .tasks
            .remove(0);
        let config = task.automation.unwrap();
        assert_eq!(config.status, crate::AutomationStatus::Completed);
        assert_eq!(config.owned_session_id.as_deref(), Some("owned-chat"));
        assert_eq!(config.runs[0].summary.as_deref(), Some("result"));
    }
    #[tokio::test]
    async fn v2_inactive_and_future_version_never_fall_back_to_legacy() {
        for status in ["paused", "completed"] {
            let fs = v2_file(status, true);
            let firer = RecordingFirer::new("legacy");
            assert!(run_due_jobs(
                Path::new(AUTOMATION_PATH),
                fs.clone(),
                FixedClock::at_secs(NOW),
                &firer,
                None
            )
            .await
            .is_empty());
            assert!(
                next_fire_epoch_ms(Path::new(AUTOMATION_PATH), fs, FixedClock::at_secs(NOW))
                    .await
                    .is_none()
            );
            assert!(firer.calls().is_empty());
        }
        let fs = v2_file("active", true);
        let body = fs
            .get(AUTOMATION_PATH)
            .await
            .unwrap()
            .replace("\"version\":2", "\"version\":3");
        let doc = parse_tasks(&body);
        assert!(doc.tasks.is_empty());
        assert_eq!(doc.unmodeled.len(), 1);
        assert!(crate::serialize_tasks(&doc).contains("\"version\": 3"));
    }
    #[tokio::test]
    async fn v2_busy_coalesces_and_invalid_configuration_pauses() {
        let fs = v2_file("active", true);
        let root = Path::new("/proj");
        let now = FixedClock::at_secs(NOW).now();
        let request = crate::claim_automation_run(fs.as_ref(), root, "dv2", now, None)
            .await
            .unwrap();
        crate::bind_automation_run_session(fs.as_ref(), root, &request, "busy-session")
            .await
            .unwrap();
        assert!(
            !crate::finish_automation_run(
                fs.as_ref(),
                root,
                &request,
                &Err("busy:target".into()),
                NOW * 1000
            )
            .await
        );
        let pending = parse_tasks(&fs.get(AUTOMATION_PATH).await.unwrap())
            .tasks
            .remove(0)
            .automation
            .unwrap();
        assert_eq!(pending.runs[0].session_id, None);
        assert_eq!(pending.runs[0].owner_pid, None);
        assert_eq!(pending.owned_session_id.as_deref(), Some("busy-session"));
        let next = crate::claim_automation_run(fs.as_ref(), root, "dv2", now, None)
            .await
            .unwrap();
        assert_eq!(request.run_id, next.run_id);
        assert!(
            crate::finish_automation_run(
                fs.as_ref(),
                root,
                &next,
                &Err("paused:Missing target".into()),
                NOW * 1000
            )
            .await
        );
        let task = parse_tasks(&fs.get(AUTOMATION_PATH).await.unwrap())
            .tasks
            .remove(0);
        let config = task.automation.unwrap();
        assert_eq!(config.status, crate::AutomationStatus::Paused);
        assert_eq!(config.status_reason.as_deref(), Some("Missing target"));
        assert_eq!(config.runs.len(), 1);
    }

    #[tokio::test]
    async fn v2_concurrent_claims_coalesce_one_pending_occurrence() {
        let fs = v2_file("active", true);
        let root = Path::new("/proj");
        let now = FixedClock::at_secs(NOW).now();
        let (a, b) = tokio::join!(
            crate::claim_automation_run(fs.as_ref(), root, "dv2", now, None),
            crate::claim_automation_run(fs.as_ref(), root, "dv2", now, None)
        );
        assert_eq!(usize::from(a.is_some()) + usize::from(b.is_some()), 1);
        let later = now + Duration::from_secs(180);
        for _ in 0..3 {
            assert!(
                crate::claim_automation_run(fs.as_ref(), root, "dv2", later, None)
                    .await
                    .is_none()
            );
        }
        let task = parse_tasks(&fs.get(AUTOMATION_PATH).await.unwrap())
            .tasks
            .remove(0);
        let config = task.automation.unwrap();
        assert_eq!(
            config
                .runs
                .iter()
                .filter(|r| r.status == crate::AutomationRunStatus::Running)
                .count(),
            1
        );
        assert_eq!(
            config
                .runs
                .iter()
                .filter(|r| r.status == crate::AutomationRunStatus::Queued)
                .count(),
            1
        );
    }
    #[tokio::test]
    async fn v2_busy_merges_a_later_pending_occurrence_into_the_retry() {
        let fs = v2_file("active", true);
        let root = Path::new("/proj");
        let now = FixedClock::at_secs(NOW).now();
        let first = crate::claim_automation_run(fs.as_ref(), root, "dv2", now, None)
            .await
            .unwrap();
        let later = now + Duration::from_secs(180);
        assert!(
            crate::claim_automation_run(fs.as_ref(), root, "dv2", later, None)
                .await
                .is_none()
        );
        assert_eq!(
            parse_tasks(&fs.get(AUTOMATION_PATH).await.unwrap()).tasks[0]
                .automation
                .as_ref()
                .unwrap()
                .runs
                .len(),
            2
        );
        assert!(
            !crate::finish_automation_run(
                fs.as_ref(),
                root,
                &first,
                &Err("busy:target".into()),
                (NOW + 180) * 1000
            )
            .await
        );
        let config = parse_tasks(&fs.get(AUTOMATION_PATH).await.unwrap())
            .tasks
            .remove(0)
            .automation
            .unwrap();
        assert_eq!(config.runs.len(), 1);
        assert_eq!(config.runs[0].id, first.run_id);
        assert_eq!(config.runs[0].status, crate::AutomationRunStatus::Queued);
        let retry = crate::claim_automation_run(fs.as_ref(), root, "dv2", later, None)
            .await
            .unwrap();
        assert_eq!(retry.run_id, first.run_id);
        assert!(
            crate::finish_automation_run(
                fs.as_ref(),
                root,
                &retry,
                &Ok(crate::AutomationRunResult {
                    session_id: "chat".into(),
                    summary: "done".into()
                }),
                (NOW + 180) * 1000
            )
            .await
        );
        assert!(
            crate::claim_automation_run(fs.as_ref(), root, "dv2", later, None)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn v2_one_shot_reactivation_uses_future_anchor_and_preserves_history() {
        for previously_executed in [false, true] {
            let fs = v2_file("active", false);
            let root = Path::new("/proj");
            let now = FixedClock::at_secs(NOW).now();
            if previously_executed {
                let old = crate::claim_automation_run(fs.as_ref(), root, "dv2", now, None)
                    .await
                    .unwrap();
                assert!(
                    crate::finish_automation_run(
                        fs.as_ref(),
                        root,
                        &old,
                        &Err("failed".into()),
                        NOW * 1000
                    )
                    .await
                );
            }
            let mut doc = parse_tasks(&fs.get(AUTOMATION_PATH).await.unwrap());
            let created = doc.tasks[0].created_at;
            let history = doc.tasks[0].automation.as_ref().unwrap().runs.clone();
            // The management API writes this anchor when resuming or saving
            // valid Active settings for a completed task.
            doc.tasks[0].last_fired_at = Some(NOW * 1000);
            doc.tasks[0].automation.as_mut().unwrap().status = crate::AutomationStatus::Active;
            crate::tasks_file::write_automation_tasks_body(
                fs.as_ref(),
                root,
                &crate::serialize_tasks(&doc),
            )
            .await
            .unwrap();
            let next = crate::next_fire_epoch_ms_for_persisted_task(&doc.tasks[0], now).unwrap();
            assert!(next > NOW * 1000);
            assert_eq!(
                next_fire_epoch_ms(
                    Path::new(AUTOMATION_PATH),
                    fs.clone(),
                    FixedClock::at_secs(NOW)
                )
                .await,
                Some(next)
            );
            assert!(
                crate::claim_automation_run(fs.as_ref(), root, "dv2", now, None)
                    .await
                    .is_none()
            );
            let claim = crate::claim_automation_run(
                fs.as_ref(),
                root,
                "dv2",
                SystemTime::UNIX_EPOCH + Duration::from_millis(next),
                Some(next),
            )
            .await
            .unwrap();
            assert!(!history.iter().any(|old| old.id == claim.run_id));
            let saved = parse_tasks(&fs.get(AUTOMATION_PATH).await.unwrap())
                .tasks
                .remove(0);
            assert_eq!(saved.created_at, created);
            assert_eq!(
                &saved.automation.unwrap().runs[..history.len()],
                history.as_slice()
            );

            let mut legacy = doc.tasks.remove(0);
            legacy.automation = None;
            assert_eq!(
                crate::next_fire_epoch_ms_for_persisted_task(&legacy, now),
                next_fire_epoch_ms_for_task(&legacy.id, &legacy.cron, created, None, false, now)
            );
        }
    }

    #[tokio::test]
    async fn v2_expiry_retains_task_and_recovery_does_not_replay() {
        let fs = v2_file("active", false);
        let root = Path::new("/proj");
        let now = FixedClock::at_secs(NOW).now();
        crate::claim_automation_run(fs.as_ref(), root, "dv2", now, None)
            .await
            .unwrap();
        crate::interrupt_orphaned_automation_runs(fs.as_ref(), root, NOW * 1000)
            .await
            .unwrap();
        let task = parse_tasks(&fs.get(AUTOMATION_PATH).await.unwrap())
            .tasks
            .remove(0);
        let config = task.automation.unwrap();
        assert_eq!(config.status, crate::AutomationStatus::Completed);
        assert_eq!(
            config.runs[0].status,
            crate::AutomationRunStatus::Interrupted
        );
        assert!(
            crate::claim_automation_run(fs.as_ref(), root, "dv2", now, None)
                .await
                .is_none()
        );
        let fs = v2_file("active", true);
        let mut doc = parse_tasks(&fs.get(AUTOMATION_PATH).await.unwrap());
        doc.tasks[0].expires_at = Some(NOW * 1000 - 1);
        crate::tasks_file::write_automation_tasks_body(
            fs.as_ref(),
            root,
            &crate::serialize_tasks(&doc),
        )
        .await
        .unwrap();
        assert!(
            crate::claim_automation_run(fs.as_ref(), root, "dv2", now, None)
                .await
                .is_none()
        );
        let task = parse_tasks(&fs.get(AUTOMATION_PATH).await.unwrap())
            .tasks
            .remove(0);
        assert_eq!(
            task.automation.unwrap().status,
            crate::AutomationStatus::Completed
        );
    }
    #[tokio::test]
    async fn v2_paused_before_binding_cancels_unstarted_run_without_overriding_task() {
        for status in [
            crate::AutomationStatus::Paused,
            crate::AutomationStatus::Completed,
        ] {
            let fs = v2_file("active", true);
            let root = Path::new("/proj");
            let request = crate::claim_automation_run(
                fs.as_ref(),
                root,
                "dv2",
                FixedClock::at_secs(NOW).now(),
                None,
            )
            .await
            .unwrap();
            let mut doc = parse_tasks(&fs.get(AUTOMATION_PATH).await.unwrap());
            doc.tasks[0].automation.as_mut().unwrap().status = status;
            crate::tasks_file::write_automation_tasks_body(
                fs.as_ref(),
                root,
                &crate::serialize_tasks(&doc),
            )
            .await
            .unwrap();
            let error = crate::bind_automation_run_session(fs.as_ref(), root, &request, "chat")
                .await
                .unwrap_err();
            assert!(error.starts_with(crate::AUTOMATION_CANCELLED_PREFIX));
            assert!(
                !crate::finish_automation_run(fs.as_ref(), root, &request, &Err(error), NOW * 1000)
                    .await
            );
            let a = parse_tasks(&fs.get(AUTOMATION_PATH).await.unwrap())
                .tasks
                .remove(0)
                .automation
                .unwrap();
            assert_eq!(a.status, status);
            assert_eq!(a.runs[0].status, crate::AutomationRunStatus::Cancelled);
            assert_eq!(a.runs[0].session_id, None);
            assert_eq!(a.owned_session_id, None);
        }
    }
    #[tokio::test]
    async fn v2_bound_run_can_finish_after_pause_and_resume_rejects_old_occurrence() {
        let fs = v2_file("active", true);
        let root = Path::new("/proj");
        let request = crate::claim_automation_run(
            fs.as_ref(),
            root,
            "dv2",
            FixedClock::at_secs(NOW).now(),
            None,
        )
        .await
        .unwrap();
        crate::bind_automation_run_session(fs.as_ref(), root, &request, "chat")
            .await
            .unwrap();
        let mut doc = parse_tasks(&fs.get(AUTOMATION_PATH).await.unwrap());
        doc.tasks[0].automation.as_mut().unwrap().status = crate::AutomationStatus::Paused;
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
                    summary: "finished".into()
                }),
                NOW * 1000
            )
            .await
        );
        let mut doc = parse_tasks(&fs.get(AUTOMATION_PATH).await.unwrap());
        let a = doc.tasks[0].automation.as_mut().unwrap();
        assert_eq!(a.status, crate::AutomationStatus::Paused);
        assert_eq!(a.runs[0].status, crate::AutomationRunStatus::Succeeded);
        let old_scheduled_at = a.runs[0].scheduled_at;
        a.status = crate::AutomationStatus::Active;
        a.runs
            .retain(|run| run.status != crate::AutomationRunStatus::Queued);
        doc.tasks[0].last_fired_at = Some((NOW + 600) * 1000);
        crate::tasks_file::write_automation_tasks_body(
            fs.as_ref(),
            root,
            &crate::serialize_tasks(&doc),
        )
        .await
        .unwrap();
        assert!(crate::claim_automation_run(
            fs.as_ref(),
            root,
            "dv2",
            FixedClock::at_secs(NOW + 600).now(),
            Some(old_scheduled_at)
        )
        .await
        .is_none());
    }
    #[tokio::test]
    async fn v2_manual_run_preserves_schedule_anchor_and_records_cancellation() {
        let fs = v2_file("active", true);
        let root = Path::new("/proj");
        let now = FixedClock::at_secs(NOW).now();
        let mut doc = parse_tasks(&fs.get(AUTOMATION_PATH).await.unwrap());
        doc.tasks[0].created_at = NOW * 1000;
        crate::tasks_file::write_automation_tasks_body(
            fs.as_ref(),
            root,
            &crate::serialize_tasks(&doc),
        )
        .await
        .unwrap();
        assert!(
            crate::claim_automation_run(fs.as_ref(), root, "dv2", now, None)
                .await
                .is_none()
        );
        let request = crate::claim_automation_run_now(fs.as_ref(), root, "dv2", now)
            .await
            .unwrap();
        assert!(request.run_id.contains("manual"));
        assert!(
            crate::claim_automation_run_now(fs.as_ref(), root, "dv2", now)
                .await
                .is_none()
        );
        let task = parse_tasks(&fs.get(AUTOMATION_PATH).await.unwrap())
            .tasks
            .remove(0);
        assert_eq!(task.last_fired_at, None);
        assert_eq!(
            task.automation.unwrap().runs[0].owner_pid,
            Some(std::process::id())
        );
        assert!(
            crate::finish_automation_run(
                fs.as_ref(),
                root,
                &request,
                &Err("cancelled:User cancelled".into()),
                NOW * 1000
            )
            .await
        );
        let a = parse_tasks(&fs.get(AUTOMATION_PATH).await.unwrap())
            .tasks
            .remove(0)
            .automation
            .unwrap();
        assert_eq!(a.status, crate::AutomationStatus::Active);
        assert_eq!(a.runs[0].status, crate::AutomationRunStatus::Cancelled);
    }
}
