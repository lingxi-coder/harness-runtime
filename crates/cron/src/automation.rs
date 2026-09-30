//! Versioned, session-aware scheduling shared by desktop and mobile hosts.
use crate::{CronJobFirer, CronTask, FireStatus, FiredJob};
use lingxi_core::host::{Clock, FileSystem};
use serde::{Deserialize, Serialize};
use std::{path::Path, sync::Arc, time::SystemTime};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Persisted task lifecycle, separate from individual run outcomes.
pub enum AutomationStatus {
    #[default]
    Active,
    Paused,
    Completed,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Determines which conversation receives an automatic turn.
pub enum RunMode {
    #[default]
    NewSession,
    SelectedSession,
    TaskSession,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Terminal notification delivery policy enforced by the host.
pub enum NotificationPolicy {
    #[default]
    All,
    Failed,
    None,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Durable execution state for one claimed occurrence.
pub enum AutomationRunStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
}
impl AutomationRunStatus {
    /// Whether this record can be pruned by the history limit.
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// One execution record, stored inside its task scope.
pub struct AutomationRun {
    pub id: String,
    pub task_id: String,
    /// Owning process while running; absent for older records and pending work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_pid: Option<u32>,
    /// Monotonic claim identity; retries retain the run ID but replace its owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_generation: Option<u64>,
    /// Stable host token, including adoption of pre-token manual run IDs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_occurrence_at: Option<u64>,
    pub scheduled_at: u64,
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
    pub status: AutomationRunStatus,
    pub model: String,
    pub reasoning: serde_json::Value,
    pub session_id: Option<String>,
    pub summary: Option<String>,
    pub error: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Version-two configuration persisted without modifying the creator session.
pub struct CronAutomation {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub status: AutomationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
    pub model: String,
    #[serde(default)]
    pub reasoning: serde_json::Value,
    #[serde(default)]
    pub run_mode: RunMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owned_session_id: Option<String>,
    #[serde(default)]
    pub notification_policy: NotificationPolicy,
    #[serde(default)]
    pub runs: Vec<AutomationRun>,
}
/// Configuration errors pause a task instead of retrying against another target.
pub const AUTOMATION_PAUSED_PREFIX: &str = "paused:";
/// A busy target keeps one queued occurrence for a later attempt.
pub const AUTOMATION_BUSY_PREFIX: &str = "busy:";
/// The task stopped before its claimed turn began.
pub const AUTOMATION_CANCELLED_PREFIX: &str = "cancelled:";
/// Host execution ended without a confirmed terminal result.
pub const AUTOMATION_INTERRUPTED_PREFIX: &str = "interrupted:";
/// Descriptive alias for host adapters.
pub type AutomationConfig = CronAutomation;
#[derive(Debug, Clone)]
/// Claimed immutable execution input. Hosts must not substitute another target.
pub struct AutomationRunRequest {
    pub run_id: String,
    /// Generation acquired under the tasks-file lock for this execution attempt.
    pub claim_generation: u64,
    pub task: CronTask,
}
#[derive(Debug, Clone)]
/// Persistable result from a host session executor.
pub struct AutomationRunResult {
    pub session_id: String,
    pub summary: String,
}
fn epoch(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Execute due v2 tasks. The shared tasks-file lock atomically claims an
/// occurrence before host execution and merges completion into the latest file.
/// A persisted running record suppresses duplicates across runtimes/processes.
pub async fn run_due_automations(
    path: &Path,
    fs: Arc<dyn FileSystem>,
    clock: Arc<dyn Clock>,
    firer: &dyn CronJobFirer,
) -> Vec<FiredJob> {
    let Some(root) = crate::project_root_from_tasks_path(path) else {
        return vec![];
    };
    let Ok(body) = crate::tasks_file::read_automation_tasks_body(fs.as_ref(), root).await else {
        return vec![];
    };
    let ids: Vec<_> = crate::tasks_file::parse_automation_tasks(&body)
        .tasks
        .into_iter()
        .filter(|t| t.automation.is_some())
        .map(|t| t.id)
        .collect();
    let mut pending: Vec<
        std::pin::Pin<Box<dyn std::future::Future<Output = Option<FiredJob>> + Send + '_>>,
    > = Vec::new();
    for id in ids {
        let now = clock.now();
        let Some(request) = claim_automation_run(fs.as_ref(), root, &id, now, None).await else {
            continue;
        };
        let fs = fs.clone();
        let clock = clock.clone();
        pending.push(Box::pin(async move {
            let result = firer.fire_automation(&request).await;
            if finish_automation_run(fs.as_ref(), root, &request, &result, epoch(clock.now())).await
            {
                Some(FiredJob {
                    id,
                    prompt: request.task.prompt,
                    result_text: result.as_ref().ok().map(|r| r.summary.clone()),
                    status: match result {
                        Ok(_) => FireStatus::Ok,
                        Err(error) => FireStatus::Failed(error),
                    },
                })
            } else {
                None
            }
        }));
    }
    let mut fired = Vec::new();
    // Poll every execution independently. Single-shot mobile callers await the
    // batch, but a blocked target cannot prevent another target from running.
    std::future::poll_fn(|cx| {
        let mut index = 0;
        while index < pending.len() {
            match pending[index].as_mut().poll(cx) {
                std::task::Poll::Ready(result) => {
                    drop(pending.swap_remove(index));
                    if let Some(result) = result {
                        fired.push(result);
                    }
                }
                std::task::Poll::Pending => index += 1,
            }
        }
        if pending.is_empty() {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    })
    .await;
    fired
}
/// Atomically claim a due occurrence, optionally matching a delayed worker token.
pub async fn claim_automation_run(
    fs: &dyn FileSystem,
    root: &Path,
    id: &str,
    now: SystemTime,
    expected_scheduled_at: Option<u64>,
) -> Option<AutomationRunRequest> {
    let _guard = crate::lock_cron_file().await;
    let _file = crate::tasks_file::lock_automation_tasks(fs, root)
        .await
        .ok()?;
    let mut doc = crate::tasks_file::parse_automation_tasks_strict(
        &crate::tasks_file::read_automation_tasks_body(fs, root)
            .await
            .ok()?,
    )
    .ok()?;
    let task = doc.tasks.iter_mut().find(|t| t.id == id)?;
    let next = crate::next_fire_epoch_ms_for_persisted_task(task, now);
    let config = task.automation.as_mut()?;
    if config.status == AutomationStatus::Completed {
        return None;
    }
    let now_ms = epoch(now);
    if task.expires_at.is_some_and(|e| e <= now_ms) {
        config.status = AutomationStatus::Completed;
        config.status_reason = Some("Schedule expired".into());
        config
            .runs
            .retain(|r| r.status != AutomationRunStatus::Queued);
        crate::tasks_file::write_automation_tasks_body(fs, root, &crate::serialize_tasks(&doc))
            .await
            .ok()?;
        return None;
    }
    if config.status != AutomationStatus::Active {
        return None;
    }
    let next = next?;
    let running = config
        .runs
        .iter()
        .any(|r| r.status == AutomationRunStatus::Running);
    let queued = config
        .runs
        .iter()
        .position(|r| r.status == AutomationRunStatus::Queued);
    let run_id;
    if !running && queued.is_some() {
        let run = &mut config.runs[queued?];
        if expected_scheduled_at.is_some_and(|expected| expected != run.scheduled_at) {
            return None;
        }
        run.claim_generation = Some(run.claim_generation.unwrap_or(0).checked_add(1)?);
        run.status = AutomationRunStatus::Running;
        run.owner_pid = Some(std::process::id());
        run.started_at = Some(now_ms);
        run.model.clone_from(&config.model);
        run.reasoning.clone_from(&config.reasoning);
        run_id = run.id.clone();
    } else {
        if next > now_ms || expected_scheduled_at.is_some_and(|expected| expected != next) {
            return None;
        }
        if running && (!task.recurring.unwrap_or(false) || queued.is_some()) {
            return None;
        }
        run_id = format!("{}-{next}", task.id);
        if config.runs.iter().any(|r| r.id == run_id) {
            return None;
        }
        config.runs.push(AutomationRun {
            id: run_id.clone(),
            task_id: id.into(),
            owner_pid: (!running).then_some(std::process::id()),
            claim_generation: (!running).then_some(1),
            manual_occurrence_at: None,
            scheduled_at: next,
            started_at: (!running).then_some(now_ms),
            finished_at: None,
            status: if running {
                AutomationRunStatus::Queued
            } else {
                AutomationRunStatus::Running
            },
            model: config.model.clone(),
            reasoning: config.reasoning.clone(),
            session_id: None,
            summary: None,
            error: None,
        });
        task.last_fired_at = Some(now_ms);
        if running {
            crate::tasks_file::write_automation_tasks_body(fs, root, &crate::serialize_tasks(&doc))
                .await
                .ok()?;
            return None;
        }
    }
    let claim_generation = task
        .automation
        .as_ref()?
        .runs
        .iter()
        .find(|run| run.id == run_id)?
        .claim_generation?;
    let mut snapshot = task.clone();
    if let Some(a) = snapshot.automation.as_mut() {
        a.runs.clear();
    }
    crate::tasks_file::write_automation_tasks_body(fs, root, &crate::serialize_tasks(&doc))
        .await
        .ok()?;
    Some(AutomationRunRequest {
        run_id,
        claim_generation,
        task: snapshot,
    })
}
fn owns_run(run: &AutomationRun, request: &AutomationRunRequest) -> bool {
    run.status == AutomationRunStatus::Running
        && run.owner_pid == Some(std::process::id())
        && run.claim_generation == Some(request.claim_generation)
}

// Execution results describe the saved snapshot, not settings the user may
// have repaired while the host was preparing or running that snapshot.
fn same_execution_configuration(current: &CronTask, snapshot: &CronTask) -> bool {
    let (Some(current_config), Some(saved_config)) =
        (current.automation.as_ref(), snapshot.automation.as_ref())
    else {
        return false;
    };
    current.cron == snapshot.cron
        && current.prompt == snapshot.prompt
        && current.recurring == snapshot.recurring
        && current.expires_at == snapshot.expires_at
        && current_config.model == saved_config.model
        && current_config.reasoning == saved_config.reasoning
        && current_config.run_mode == saved_config.run_mode
        && current_config.target_session_id == saved_config.target_session_id
        // The first task-session binding is written by this run itself.
        && (saved_config.owned_session_id.is_none()
            || current_config.owned_session_id == saved_config.owned_session_id)
}

/// Durable disposition of a result attempt; I/O failures are returned separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomationFinishDisposition {
    /// The terminal result has been persisted.
    Terminal,
    /// A busy occurrence was durably returned to the queue.
    Queued,
    /// The task/run was removed, already completed, or belongs to another claim.
    Superseded,
}

/// Compatibility helper for callers interested only in a newly persisted terminal.
pub async fn finish_automation_run(
    fs: &dyn FileSystem,
    root: &Path,
    request: &AutomationRunRequest,
    result: &Result<AutomationRunResult, String>,
    now: u64,
) -> bool {
    matches!(
        finish_automation_run_checked(fs, root, request, result, now).await,
        Ok(AutomationFinishDisposition::Terminal)
    )
}

/// Merge a host result into the latest task without overwriting concurrent edits.
pub async fn finish_automation_run_checked(
    fs: &dyn FileSystem,
    root: &Path,
    request: &AutomationRunRequest,
    result: &Result<AutomationRunResult, String>,
    now: u64,
) -> Result<AutomationFinishDisposition, String> {
    let _guard = crate::lock_cron_file().await;
    let _file = crate::tasks_file::lock_automation_tasks(fs, root)
        .await
        .map_err(|error| error.to_string())?;
    let body = match crate::tasks_file::read_automation_tasks_body(fs, root).await {
        Ok(body) => body,
        Err(error) => return Err(error.to_string()),
    };
    let mut doc = crate::tasks_file::parse_automation_tasks_strict(&body)?;
    let Some(task) = doc.tasks.iter_mut().find(|t| t.id == request.task.id) else {
        return Ok(AutomationFinishDisposition::Superseded);
    };
    let configuration_unchanged = same_execution_configuration(task, &request.task);
    let Some(a) = task.automation.as_mut() else {
        return Ok(AutomationFinishDisposition::Superseded);
    };
    let Some(run) = a.runs.iter_mut().find(|r| r.id == request.run_id) else {
        return Ok(AutomationFinishDisposition::Superseded);
    };
    if !owns_run(run, request) {
        return Ok(AutomationFinishDisposition::Superseded);
    }
    if let Err(error) = result {
        if error.starts_with(AUTOMATION_BUSY_PREFIX) && a.status == AutomationStatus::Active {
            run.status = AutomationRunStatus::Queued;
            run.started_at = None;
            run.session_id = None;
            run.owner_pid = None;
            // A later occurrence may have queued while this host discovered
            // the target was busy. Retry the original durable run and merge
            // all later pending occurrences into it under the same file lock.
            a.runs.retain(|pending| {
                pending.status != AutomationRunStatus::Queued || pending.id == request.run_id
            });
            crate::tasks_file::write_automation_tasks_body(fs, root, &crate::serialize_tasks(&doc))
                .await
                .map_err(|error| error.to_string())?;
            return Ok(AutomationFinishDisposition::Queued);
        }
    }
    run.finished_at = Some(now);
    match result {
        Ok(value) => {
            run.status = AutomationRunStatus::Succeeded;
            run.session_id = Some(value.session_id.clone());
            run.summary = Some(value.summary.clone());
            if a.run_mode == RunMode::TaskSession
                && request
                    .task
                    .automation
                    .as_ref()
                    .is_some_and(|config| config.run_mode == RunMode::TaskSession)
                && a.owned_session_id.is_none()
            {
                a.owned_session_id = Some(value.session_id.clone());
            }
        }
        Err(error) => {
            run.status = if error.starts_with(AUTOMATION_CANCELLED_PREFIX) {
                AutomationRunStatus::Cancelled
            } else if error.starts_with(AUTOMATION_INTERRUPTED_PREFIX) {
                AutomationRunStatus::Interrupted
            } else {
                AutomationRunStatus::Failed
            };
            run.error = Some(error.clone());
        }
    }
    if let Err(error) = result {
        if let Some(reason) = error
            .strip_prefix(AUTOMATION_PAUSED_PREFIX)
            .filter(|_| configuration_unchanged && a.status != AutomationStatus::Completed)
        {
            a.status = AutomationStatus::Paused;
            a.status_reason = Some(reason.trim().into());
            a.runs.retain(|r| r.status != AutomationRunStatus::Queued);
        }
    }
    // A one-shot is Completed only once its occurrence actually ran. A
    // `paused:`, `busy:`, `cancelled:` or `interrupted:` outcome says the run
    // never reached a verdict — cancelling a manual "Run now", or an app
    // shutdown mid-flight, must not silently retire the task's own schedule
    // (`claim_automation_run` returns `None` for a `Completed` automation).
    let unresolved = result.as_ref().err().is_some_and(|error| {
        error.starts_with(AUTOMATION_PAUSED_PREFIX)
            || error.starts_with(AUTOMATION_BUSY_PREFIX)
            || error.starts_with(AUTOMATION_CANCELLED_PREFIX)
            || error.starts_with(AUTOMATION_INTERRUPTED_PREFIX)
    });
    if configuration_unchanged && !request.task.recurring.unwrap_or(false) && !unresolved {
        a.status = AutomationStatus::Completed;
    }
    prune_history(&mut doc);
    crate::tasks_file::write_automation_tasks_body(fs, root, &crate::serialize_tasks(&doc))
        .await
        .map_err(|error| error.to_string())?;
    Ok(AutomationFinishDisposition::Terminal)
}
/// Keep terminal history bounded without removing in-flight records.
pub fn prune_history(doc: &mut crate::ScheduledTasks) {
    for a in doc.tasks.iter_mut().filter_map(|t| t.automation.as_mut()) {
        let mut excess = a
            .runs
            .iter()
            .filter(|r| r.status.is_terminal())
            .count()
            .saturating_sub(20);
        a.runs.retain(|r| {
            if excess > 0 && r.status.is_terminal() {
                excess -= 1;
                false
            } else {
                true
            }
        });
    }
    let mut terminal: Vec<_> = doc
        .tasks
        .iter()
        .filter_map(|t| t.automation.as_ref())
        .flat_map(|a| a.runs.iter())
        .filter(|r| r.status.is_terminal())
        .map(|r| (r.finished_at.unwrap_or(0), r.id.clone()))
        .collect();
    terminal.sort();
    let remove: std::collections::HashSet<_> = terminal
        .iter()
        .take(terminal.len().saturating_sub(500))
        .map(|(_, id)| id.clone())
        .collect();
    for a in doc.tasks.iter_mut().filter_map(|t| t.automation.as_mut()) {
        a.runs
            .retain(|r| !r.status.is_terminal() || !remove.contains(&r.id));
    }
}

/// Call only after the host proves no former runtime owns these running records.
/// Recovery never replays a possibly executed prompt.
pub async fn interrupt_orphaned_automation_runs(
    fs: &dyn FileSystem,
    root: &Path,
    now: u64,
) -> Result<(), String> {
    let _guard = crate::lock_cron_file().await;
    let _file = crate::tasks_file::lock_automation_tasks(fs, root)
        .await
        .map_err(|e| e.to_string())?;
    let body = crate::tasks_file::read_automation_tasks_body(fs, root)
        .await
        .map_err(|e| e.to_string())?;
    let mut doc = crate::tasks_file::parse_automation_tasks_strict(&body)?;
    for task in &mut doc.tasks {
        if let Some(config) = task.automation.as_mut() {
            let mut interrupted = false;
            for run in &mut config.runs {
                if run.status == AutomationRunStatus::Running {
                    run.status = AutomationRunStatus::Interrupted;
                    run.finished_at = Some(now);
                    run.error =
                        Some("Execution interrupted before its result was confirmed".into());
                    interrupted = true;
                }
            }
            if interrupted && !task.recurring.unwrap_or(false) {
                config.status = AutomationStatus::Completed;
            }
        }
    }
    prune_history(&mut doc);
    crate::tasks_file::write_automation_tasks_body(fs, root, &crate::serialize_tasks(&doc))
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn history_limits_keep_all_nonterminal_records() {
        let mut doc = crate::tasks_file::parse_automation_tasks(&serde_json::json!({"tasks": (0..30).map(|task| {
            serde_json::json!({"id":format!("task-{task}"),"cron":"* * * * *","prompt":"test","createdAt":1,"automation":{"version":2,"model":"provider/model","runs":(0..32).map(|run| AutomationRun {
                id: format!("{task}-{run}"), task_id: format!("task-{task}"), owner_pid: None, claim_generation: None, manual_occurrence_at: None, scheduled_at: run,
                started_at: Some(run), finished_at: (run < 30).then_some(run),
                status: if run < 30 { AutomationRunStatus::Succeeded } else { AutomationRunStatus::Running },
                model: "provider/model".into(), reasoning: serde_json::Value::Null,
                session_id: None, summary: None, error: None,
            }).collect::<Vec<_>>()}})
        }).collect::<Vec<_>>()} ).to_string());
        prune_history(&mut doc);
        let runs: Vec<_> = doc
            .tasks
            .iter()
            .flat_map(|task| &task.automation.as_ref().unwrap().runs)
            .collect();
        assert_eq!(runs.iter().filter(|r| r.status.is_terminal()).count(), 500);
        assert_eq!(runs.iter().filter(|r| !r.status.is_terminal()).count(), 60);
        assert!(doc.tasks.iter().all(|task| task
            .automation
            .as_ref()
            .unwrap()
            .runs
            .iter()
            .filter(|r| r.status.is_terminal())
            .count()
            <= 20));
    }
}

/// Persist the selected/created session before starting its turn, so failure or
/// interruption still has a result destination and task-session retries reuse it.
pub async fn bind_automation_run_session(
    fs: &dyn FileSystem,
    root: &Path,
    request: &AutomationRunRequest,
    session_id: &str,
) -> Result<(), String> {
    let _guard = crate::lock_cron_file().await;
    let _file = crate::tasks_file::lock_automation_tasks(fs, root)
        .await
        .map_err(|e| e.to_string())?;
    let body = crate::tasks_file::read_automation_tasks_body(fs, root)
        .await
        .map_err(|e| e.to_string())?;
    let mut doc = crate::tasks_file::parse_automation_tasks_strict(&body)?;
    let task = doc
        .tasks
        .iter_mut()
        .find(|t| t.id == request.task.id)
        .ok_or("Scheduled task was removed")?;
    let now = epoch(SystemTime::now());
    let expired = task.expires_at.is_some_and(|expiry| expiry <= now);
    let config = task
        .automation
        .as_mut()
        .ok_or("Task configuration was removed")?;
    let run = config
        .runs
        .iter_mut()
        .find(|r| r.id == request.run_id && owns_run(r, request))
        .ok_or("Run is no longer active")?;
    if expired || config.status != AutomationStatus::Active {
        run.status = AutomationRunStatus::Cancelled;
        run.started_at = None;
        run.finished_at = Some(now);
        let reason = if expired {
            "Schedule expired before execution began"
        } else {
            "Task stopped before execution began"
        };
        run.error = Some(reason.into());
        if expired {
            config.status = AutomationStatus::Completed;
            config.status_reason = Some("Schedule expired".into());
            for pending in &mut config.runs {
                if pending.status == AutomationRunStatus::Queued {
                    pending.status = AutomationRunStatus::Cancelled;
                    pending.finished_at = Some(now);
                    pending.error = Some(reason.into());
                }
            }
        }
        prune_history(&mut doc);
        crate::tasks_file::write_automation_tasks_body(fs, root, &crate::serialize_tasks(&doc))
            .await
            .map_err(|e| e.to_string())?;
        return Err(format!("{AUTOMATION_CANCELLED_PREFIX}{reason}"));
    }
    run.session_id = Some(session_id.into());
    if config.run_mode == RunMode::TaskSession
        && request
            .task
            .automation
            .as_ref()
            .is_some_and(|a| a.run_mode == RunMode::TaskSession)
        && config.owned_session_id.is_none()
    {
        config.owned_session_id = Some(session_id.into());
    }
    crate::tasks_file::write_automation_tasks_body(fs, root, &crate::serialize_tasks(&doc))
        .await
        .map_err(|e| e.to_string())
}

/// Claim an explicit Run now request without advancing the automatic schedule.
/// Active state and the single-running-or-pending rule still apply.
pub async fn claim_automation_run_now(
    fs: &dyn FileSystem,
    root: &Path,
    id: &str,
    now: SystemTime,
) -> Option<AutomationRunRequest> {
    claim_manual_run(fs, root, id, now, None).await
}

/// Claim a stable host-provided manual occurrence. Repeated delivery of the
/// same timestamp retries its queued attempt but never repeats a terminal run.
pub async fn claim_automation_run_now_at(
    fs: &dyn FileSystem,
    root: &Path,
    id: &str,
    now: SystemTime,
    scheduled_at: u64,
) -> Option<AutomationRunRequest> {
    claim_manual_run(fs, root, id, now, Some(scheduled_at)).await
}

async fn claim_manual_run(
    fs: &dyn FileSystem,
    root: &Path,
    id: &str,
    now: SystemTime,
    scheduled_at: Option<u64>,
) -> Option<AutomationRunRequest> {
    let _guard = crate::lock_cron_file().await;
    let _file = crate::tasks_file::lock_automation_tasks(fs, root)
        .await
        .ok()?;
    let mut doc = crate::tasks_file::parse_automation_tasks_strict(
        &crate::tasks_file::read_automation_tasks_body(fs, root)
            .await
            .ok()?,
    )
    .ok()?;
    let task = doc.tasks.iter_mut().find(|task| task.id == id)?;
    let config = task.automation.as_mut()?;
    let now_ms = epoch(now);
    if config.status != AutomationStatus::Active
        || task.expires_at.is_some_and(|expiry| expiry <= now_ms)
        || config
            .runs
            .iter()
            .any(|run| run.status == AutomationRunStatus::Running)
    {
        return None;
    }
    let stable_id = scheduled_at.map(|at| format!("{id}-manual-at-{at}"));
    if stable_id.as_ref().is_some_and(|id| {
        config.runs.iter().any(|run| {
            (run.id == *id || run.manual_occurrence_at == scheduled_at) && run.status.is_terminal()
        })
    }) {
        return None;
    }
    let run_id = if let Some(run) = config
        .runs
        .iter_mut()
        .find(|run| run.status == AutomationRunStatus::Queued)
    {
        // Native hosts retry explicit Run now requests through this endpoint.
        // Preserve that occurrence instead of treating its queued retry as busy.
        if !run.id.starts_with(&format!("{id}-manual-")) {
            return None;
        }
        if let Some(at) = scheduled_at {
            match run.manual_occurrence_at {
                Some(bound) if bound != at => return None,
                Some(_) => {}
                None => {
                    // Adopt only legacy non-token manual IDs. A previously
                    // explicit token must never be rebound to another action.
                    if run.id.starts_with(&format!("{id}-manual-at-"))
                        && stable_id.as_ref() != Some(&run.id)
                    {
                        return None;
                    }
                    run.manual_occurrence_at = Some(at);
                }
            }
        }
        run.claim_generation = Some(run.claim_generation.unwrap_or(0).checked_add(1)?);
        run.status = AutomationRunStatus::Running;
        run.owner_pid = Some(std::process::id());
        run.started_at = Some(now_ms);
        run.model.clone_from(&config.model);
        run.reasoning.clone_from(&config.reasoning);
        run.id.clone()
    } else {
        let mut suffix = 0_u64;
        let run_id = if let Some(stable_id) = stable_id {
            stable_id
        } else {
            loop {
                let candidate = format!("{id}-manual-{now_ms}-{suffix}");
                if !config.runs.iter().any(|run| run.id == candidate) {
                    break candidate;
                }
                suffix = suffix.checked_add(1)?;
            }
        };
        config.runs.push(AutomationRun {
            id: run_id.clone(),
            task_id: id.into(),
            owner_pid: Some(std::process::id()),
            claim_generation: Some(1),
            manual_occurrence_at: scheduled_at,
            scheduled_at: scheduled_at.unwrap_or(now_ms),
            started_at: Some(now_ms),
            finished_at: None,
            status: AutomationRunStatus::Running,
            model: config.model.clone(),
            reasoning: config.reasoning.clone(),
            session_id: None,
            summary: None,
            error: None,
        });
        run_id
    };
    let claim_generation = task
        .automation
        .as_ref()?
        .runs
        .iter()
        .find(|run| run.id == run_id)?
        .claim_generation?;
    let mut snapshot = task.clone();
    snapshot.automation.as_mut()?.runs.clear();
    crate::tasks_file::write_automation_tasks_body(fs, root, &crate::serialize_tasks(&doc))
        .await
        .ok()?;
    Some(AutomationRunRequest {
        run_id,
        claim_generation,
        task: snapshot,
    })
}

/// Reconcile abandoned claims without interrupting another live host process.
/// Unknown owners pause the task for review; potentially executed work is never replayed.
pub async fn recover_orphaned_automation_runs(
    fs: &dyn FileSystem,
    root: &Path,
    now: u64,
) -> Result<(), String> {
    let _guard = crate::lock_cron_file().await;
    let _file = crate::tasks_file::lock_automation_tasks(fs, root)
        .await
        .map_err(|e| e.to_string())?;
    let body = crate::tasks_file::read_automation_tasks_body(fs, root)
        .await
        .map_err(|e| e.to_string())?;
    let mut doc = crate::tasks_file::parse_automation_tasks_strict(&body)?;
    if reconcile_run_owners(&mut doc, now, crate::scheduler::pid_alive_check) {
        prune_history(&mut doc);
        crate::tasks_file::write_automation_tasks_body(fs, root, &crate::serialize_tasks(&doc))
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn reconcile_run_owners(
    doc: &mut crate::ScheduledTasks,
    now: u64,
    is_alive: impl Fn(u32) -> bool,
) -> bool {
    let mut changed = false;
    for task in &mut doc.tasks {
        let Some(config) = task.automation.as_mut() else {
            continue;
        };
        let mut interrupted = false;
        let mut unknown_owner = false;
        for run in &mut config.runs {
            if run.status != AutomationRunStatus::Running {
                continue;
            }
            match run.owner_pid {
                Some(pid) if !is_alive(pid) => {
                    run.status = AutomationRunStatus::Interrupted;
                    run.finished_at = Some(now);
                    run.error =
                        Some("Execution owner exited before its result was confirmed".into());
                    interrupted = true;
                    changed = true;
                }
                None => unknown_owner = true,
                Some(_) => {}
            }
        }
        if unknown_owner && config.status == AutomationStatus::Active {
            config.status = AutomationStatus::Paused;
            config.status_reason = Some(
                "Cannot verify the previous execution owner; review the run before resuming".into(),
            );
            config
                .runs
                .retain(|run| run.status != AutomationRunStatus::Queued);
            changed = true;
        } else if interrupted && !task.recurring.unwrap_or(false) {
            config.status = AutomationStatus::Completed;
        }
    }
    changed
}

#[cfg(test)]
mod recovery_tests {
    use super::*;

    #[test]
    fn recovery_interrupts_only_dead_owners_and_preserves_live_and_unknown_runs() {
        let mut doc = crate::tasks_file::parse_automation_tasks(&serde_json::json!({"tasks": [
            {"id":"dead","cron":"* * * * *","prompt":"test","createdAt":1,"automation":{"version":2,"model":"p/m","runs":[{"id":"dead-run","taskId":"dead","ownerPid":12,"scheduledAt":1,"status":"running","model":"p/m","reasoning":null}]}},
            {"id":"live","cron":"* * * * *","prompt":"test","createdAt":1,"automation":{"version":2,"model":"p/m","runs":[{"id":"live-run","taskId":"live","ownerPid":34,"scheduledAt":1,"status":"running","model":"p/m","reasoning":null}]}},
            {"id":"unknown","cron":"* * * * *","prompt":"test","createdAt":1,"automation":{"version":2,"model":"p/m","runs":[{"id":"unknown-run","taskId":"unknown","scheduledAt":1,"status":"running","model":"p/m","reasoning":null}]}}
        ]}).to_string());
        assert_eq!(doc.tasks.len(), 3);
        assert!(reconcile_run_owners(&mut doc, 100, |pid| pid == 34));
        let states: Vec<_> = doc
            .tasks
            .iter()
            .filter_map(|task| task.automation.as_ref())
            .collect();
        assert_eq!(states[0].status, AutomationStatus::Completed);
        assert_eq!(states[0].runs[0].status, AutomationRunStatus::Interrupted);
        assert_eq!(states[1].status, AutomationStatus::Active);
        assert_eq!(states[1].runs[0].status, AutomationRunStatus::Running);
        assert_eq!(states[2].status, AutomationStatus::Paused);
        assert_eq!(states[2].runs[0].status, AutomationRunStatus::Running);
        assert!(states[2].status_reason.is_some());
        assert!(!reconcile_run_owners(&mut doc, 101, |pid| pid == 34));
    }
}
