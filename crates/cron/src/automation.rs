//! Versioned, session-aware scheduling shared by desktop and mobile hosts.
use crate::{CronJobFirer, CronTask, FireStatus, FiredJob};
use lingxi_core::host::{Clock, FileSystem};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

/// A durable allocation of a conversation owner, independent of task history.
/// A reservation records allocation; it does not imply that a turn ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OwnerReservation {
    pub session_id: String,
    pub session_transcript_path: String,
    pub session_mode: String,
    pub session_scope_key: String,
}

fn validate_owner_reservation(reservation: &OwnerReservation) -> Result<(), String> {
    let canonical_id = lingxi_core::types::SessionId::parse_prefixed(&reservation.session_id)
        .ok_or("Invalid owner reservation session identity")?
        .as_uuid()
        .to_string();
    if reservation.session_id != canonical_id
        || !Path::new(&reservation.session_transcript_path).is_absolute()
        || !matches!(reservation.session_mode.as_str(), "chat" | "code")
        || reservation.session_scope_key.trim().is_empty()
        || reservation.session_scope_key.len() > 8192
    {
        return Err("Invalid owner reservation source".into());
    }
    Ok(())
}

struct LockedAutomationScope {
    scope_root: PathBuf,
    scope_identity: Option<lingxi_core::host::rooted_fs::RootIdentity>,
    state_root: PathBuf,
    state_identity: Option<lingxi_core::host::rooted_fs::RootIdentity>,
    lock: Box<dyn lingxi_core::host::FlockGuard>,
}

impl LockedAutomationScope {
    async fn acquire(fs: &dyn FileSystem, root: &Path) -> Result<Self, String> {
        let scope_identity = fs
            .root_identity_no_follow(root)
            .await
            .map_err(|error| error.to_string())?;
        let state_identity = fs
            .ensure_directory_rooted_identity_pinned(
                root,
                Path::new(crate::tasks_file::CLAUDE_DIR),
                scope_identity.as_ref(),
            )
            .await
            .map_err(|error| error.to_string())?;
        let state_root = root.join(crate::tasks_file::CLAUDE_DIR);
        let lock = crate::tasks_file::lock_automation_tasks_pinned(
            fs,
            &state_root,
            state_identity.as_ref(),
        )
        .await
        .map_err(|error| error.to_string())?;
        let scope = Self {
            scope_root: root.to_path_buf(),
            scope_identity,
            state_root,
            state_identity,
            lock,
        };
        scope.validate(fs).await?;
        Ok(scope)
    }

    async fn validate(&self, fs: &dyn FileSystem) -> Result<(), String> {
        self.lock
            .validate_current()
            .map_err(|error| error.to_string())?;
        if fs
            .root_identity_no_follow(&self.scope_root)
            .await
            .map_err(|error| error.to_string())?
            != self.scope_identity
            || fs
                .root_identity_no_follow(&self.state_root)
                .await
                .map_err(|error| error.to_string())?
                != self.state_identity
        {
            return Err("Scheduler scope or state directory changed during transaction".into());
        }
        self.lock
            .validate_current()
            .map_err(|error| error.to_string())
    }
}

// Caller holds both scheduler locks. An empty, malformed, partial, or
// conflicting existing journal is corruption, never an empty owner list.
async fn read_owner_reservations_locked(
    fs: &dyn FileSystem,
    root: &Path,
    expected_root: Option<&lingxi_core::host::rooted_fs::RootIdentity>,
) -> Result<
    (
        Vec<OwnerReservation>,
        lingxi_core::host::rooted_fs::CheckedFileSnapshot,
    ),
    String,
> {
    let relative = Path::new(crate::tasks_file::OWNER_RESERVATIONS_FILE);
    let snapshot = fs
        .read_file_rooted_single_link_checked(root, relative)
        .await
        .map_err(|error| error.to_string())?;
    if !snapshot.matches_root(expected_root) {
        return Err("Scheduler root changed during owner reservation read".into());
    }
    let Some(content) = snapshot.content.as_ref() else {
        return Ok((Vec::new(), snapshot));
    };
    if content.is_empty() || !content.ends_with('\n') {
        return Err("Incomplete owner reservation ledger".into());
    }
    let mut owners: Vec<OwnerReservation> = Vec::new();
    let mut by_id = std::collections::HashMap::<String, usize>::new();
    for line in content.split_terminator('\n') {
        let owner: OwnerReservation = serde_json::from_str(line)
            .map_err(|error| format!("Invalid owner reservation ledger: {error}"))?;
        validate_owner_reservation(&owner)?;
        if let Some(index) = by_id.get(&owner.session_id) {
            if owners[*index] != owner {
                return Err("Conflicting owner reservation ledger entries".into());
            }
        } else {
            by_id.insert(owner.session_id.clone(), owners.len());
            owners.push(owner);
        }
    }
    Ok((owners, snapshot))
}

/// List allocated owners while holding the scheduler's process and file locks.
/// A genuinely missing ledger is empty; any malformed or partial entry fails.
/// Acquiring the lock may create the scheduler directory and lock file, but
/// this method never creates or changes task or ledger data.
pub async fn list_owner_reservations_strict(
    fs: &dyn FileSystem,
    root: &Path,
) -> Result<Vec<OwnerReservation>, String> {
    let _guard = crate::lock_cron_file().await;
    let scope = LockedAutomationScope::acquire(fs, root).await?;
    let (owners, _) =
        read_owner_reservations_locked(fs, &scope.state_root, scope.state_identity.as_ref())
            .await?;
    scope.validate(fs).await?;
    Ok(owners)
}

async fn reserve_owner_locked(
    fs: &dyn FileSystem,
    scope: &LockedAutomationScope,
    owner: &OwnerReservation,
) -> Result<(), String> {
    validate_owner_reservation(owner)?;
    let relative = Path::new(crate::tasks_file::OWNER_RESERVATIONS_FILE);
    let (owners, expected) =
        read_owner_reservations_locked(fs, &scope.state_root, scope.state_identity.as_ref())
            .await?;
    scope.validate(fs).await?;
    if let Some(previous) = owners
        .iter()
        .find(|entry| entry.session_id == owner.session_id)
    {
        if previous != owner {
            return Err("Session already has a different owner reservation".into());
        }
        // A previous append could have reached the file but failed to sync.
        fs.append_or_sync_file_rooted_durable_checked(&scope.state_root, relative, &expected, None)
            .await
            .map_err(|error| error.to_string())?;
        return scope.validate(fs).await;
    }
    let mut line = serde_json::to_string(owner).map_err(|error| error.to_string())?;
    line.push('\n');
    fs.append_or_sync_file_rooted_durable_checked(
        &scope.state_root,
        relative,
        &expected,
        Some(&line),
    )
    .await
    .map_err(|error| error.to_string())?;
    scope.validate(fs).await
}

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
    /// Exact source selected for this run, when known to the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_transcript_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_mode: Option<String>,
    /// Opaque native routing metadata; never an authorization capability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_scope_key: Option<String>,
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
    /// Creation mode and native owner scope for new/task-session runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_scope_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_session_transcript_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_session_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_session_scope_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owned_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owned_session_transcript_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owned_session_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owned_session_scope_key: Option<String>,
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

/// Validate the explicit execution source before a host admits a configured run.
/// The opaque scope is routing metadata and does not grant filesystem access.
pub fn validate_automation_source(automation: &CronAutomation) -> Result<(), String> {
    let valid_scope = |scope: Option<&str>| {
        scope.is_some_and(|scope| !scope.trim().is_empty() && scope.len() <= 8192)
    };
    let valid_mode = |mode: Option<&str>| mode.is_some_and(|mode| matches!(mode, "chat" | "code"));
    if automation.run_mode != RunMode::SelectedSession {
        return if valid_mode(automation.session_mode.as_deref())
            && valid_scope(automation.session_scope_key.as_deref())
        {
            Ok(())
        } else {
            Err("Choose a session mode and owner for this task".into())
        };
    }
    let valid_id = automation
        .target_session_id
        .as_deref()
        .and_then(lingxi_core::types::SessionId::parse_prefixed)
        .is_some();
    let valid_path = automation
        .target_session_transcript_path
        .as_deref()
        .is_some_and(|path| !path.is_empty() && Path::new(path).is_absolute());
    if !valid_id
        || !valid_path
        || !valid_mode(automation.target_session_mode.as_deref())
        || !valid_scope(automation.target_session_scope_key.as_deref())
    {
        return Err("Choose a conversation with an exact source, mode and owner".into());
    }
    Ok(())
}

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
    pub session_transcript_path: Option<String>,
    pub session_mode: Option<String>,
    pub session_scope_key: Option<String>,
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
            match finish_automation_run_checked(
                fs.as_ref(),
                root,
                &request,
                &result,
                epoch(clock.now()),
            )
            .await
            {
                Ok(AutomationFinishDisposition::Terminal) => Some(FiredJob {
                    id,
                    prompt: request.task.prompt,
                    result_text: result.as_ref().ok().map(|r| r.summary.clone()),
                    status: match result {
                        Ok(_) => FireStatus::Ok,
                        Err(error) => FireStatus::Failed(error),
                    },
                }),
                Ok(
                    AutomationFinishDisposition::Queued | AutomationFinishDisposition::Superseded,
                ) => None,
                Err(error) => {
                    tracing::warn!(
                        "cron: failed to persist automation completion for {id}: {error}"
                    );
                    None
                }
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
            session_transcript_path: None,
            session_mode: None,
            session_scope_key: None,
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
        && current_config.session_mode == saved_config.session_mode
        && current_config.session_scope_key == saved_config.session_scope_key
        && current_config.target_session_id == saved_config.target_session_id
        && current_config.target_session_transcript_path == saved_config.target_session_transcript_path
        && current_config.target_session_mode == saved_config.target_session_mode
        && current_config.target_session_scope_key == saved_config.target_session_scope_key
        // The first task-session binding is written by this run itself.
        && ((saved_config.owned_session_id.is_none()
                && saved_config.owned_session_transcript_path.is_none()
                && saved_config.owned_session_mode.is_none()
                && saved_config.owned_session_scope_key.is_none())
            || (current_config.owned_session_id == saved_config.owned_session_id
                && current_config.owned_session_transcript_path == saved_config.owned_session_transcript_path
                && current_config.owned_session_mode == saved_config.owned_session_mode
                && current_config.owned_session_scope_key == saved_config.owned_session_scope_key))
}

// A previously unbound task session may gain an owner during this run. Only a
// source already persisted on this exact run may justify that transition;
// an independent configuration edit must not be mistaken for our own bind.
fn first_owned_binding_matches_run(current: &CronTask, request: &AutomationRunRequest) -> bool {
    let (Some(current), Some(saved)) = (
        current.automation.as_ref(),
        request.task.automation.as_ref(),
    ) else {
        return false;
    };
    let unbound = |config: &CronAutomation| {
        config.owned_session_id.is_none()
            && config.owned_session_transcript_path.is_none()
            && config.owned_session_mode.is_none()
            && config.owned_session_scope_key.is_none()
    };
    if !unbound(saved) || unbound(current) {
        return true;
    }
    if saved.run_mode != RunMode::TaskSession {
        return false;
    }
    current.runs.iter().any(|run| {
        run.id == request.run_id
            && owns_run(run, request)
            && run.session_id == current.owned_session_id
            && run.session_transcript_path == current.owned_session_transcript_path
            && run.session_mode == current.owned_session_mode
            && run.session_scope_key == current.owned_session_scope_key
    })
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
    let configuration_unchanged = same_execution_configuration(task, &request.task)
        && first_owned_binding_matches_run(task, request);
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
        if error.starts_with(AUTOMATION_BUSY_PREFIX)
            && a.status == AutomationStatus::Active
            && configuration_unchanged
        {
            run.status = AutomationRunStatus::Queued;
            run.started_at = None;
            run.session_id = None;
            run.session_transcript_path = None;
            run.session_mode = None;
            run.session_scope_key = None;
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
            let conflict = |existing: &Option<String>, incoming: &Option<String>| {
                existing
                    .as_ref()
                    .zip(incoming.as_ref())
                    .is_some_and(|(left, right)| left != right)
            };
            if run
                .session_id
                .as_deref()
                .is_some_and(|id| !same_session_id(Some(id), &value.session_id))
                || conflict(&run.session_transcript_path, &value.session_transcript_path)
                || conflict(&run.session_mode, &value.session_mode)
                || conflict(&run.session_scope_key, &value.session_scope_key)
            {
                return Err("Execution result does not match the bound session source".into());
            }
            run.status = AutomationRunStatus::Succeeded;
            if run.session_id.is_none() {
                run.session_id = Some(value.session_id.clone());
            }
            if value.session_transcript_path.is_some() {
                run.session_transcript_path
                    .clone_from(&value.session_transcript_path);
            }
            if value.session_mode.is_some() {
                run.session_mode.clone_from(&value.session_mode);
            }
            if value.session_scope_key.is_some() {
                run.session_scope_key.clone_from(&value.session_scope_key);
            }
            run.summary = Some(value.summary.clone());
            if configuration_unchanged
                && a.run_mode == RunMode::TaskSession
                && request
                    .task
                    .automation
                    .as_ref()
                    .is_some_and(|config| config.run_mode == RunMode::TaskSession)
                && a.owned_session_id.is_none()
            {
                a.owned_session_id = run.session_id.clone();
                a.owned_session_transcript_path = run.session_transcript_path.clone();
                a.owned_session_mode = run.session_mode.clone();
                a.owned_session_scope_key = run.session_scope_key.clone();
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
                session_id: None, session_transcript_path: None, session_mode: None,
                session_scope_key: None, summary: None, error: None,
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

    #[test]
    fn execution_snapshot_compares_every_session_source_field() {
        let doc = crate::tasks_file::parse_automation_tasks(
            &serde_json::json!({"tasks": [{
                "id": "task", "cron": "* * * * *", "prompt": "test", "createdAt": 1,
                "automation": {"version": 2, "model": "p/m", "runMode": "task_session",
                    "sessionMode": "chat", "sessionScopeKey": "scope",
                    "targetSessionId": "target-a", "targetSessionTranscriptPath": "/target/a",
                    "targetSessionMode": "chat", "targetSessionScopeKey": "target-scope",
                    "ownedSessionId": "owned-a", "ownedSessionTranscriptPath": "/owned/a",
                    "ownedSessionMode": "chat", "ownedSessionScopeKey": "owned-scope"}
            }]})
            .to_string(),
        );
        let original = &doc.tasks[0];
        assert!(same_execution_configuration(original, original));
        let changes: &[fn(&mut CronAutomation)] = &[
            |a| a.session_mode = Some("code".into()),
            |a| a.session_scope_key = Some("other".into()),
            |a| a.target_session_id = Some("target-b".into()),
            |a| a.target_session_transcript_path = Some("/target/b".into()),
            |a| a.target_session_mode = Some("code".into()),
            |a| a.target_session_scope_key = Some("other".into()),
            |a| a.owned_session_id = Some("owned-b".into()),
            |a| a.owned_session_transcript_path = Some("/owned/b".into()),
            |a| a.owned_session_mode = Some("code".into()),
            |a| a.owned_session_scope_key = Some("other".into()),
        ];
        for edit in changes {
            let mut current = original.clone();
            edit(current.automation.as_mut().unwrap());
            assert!(!same_execution_configuration(&current, original));
        }
    }

    #[test]
    fn automation_source_requires_explicit_mode_and_bounded_scope() {
        let mut config: CronAutomation = serde_json::from_value(serde_json::json!({
            "version":2,"model":"provider/model","runMode":"new_session",
            "sessionMode":"chat","sessionScopeKey":"global"
        }))
        .unwrap();
        for mode in [RunMode::NewSession, RunMode::TaskSession] {
            config.run_mode = mode;
            assert!(validate_automation_source(&config).is_ok());
            config.session_scope_key = Some("x".repeat(8193));
            assert!(validate_automation_source(&config).is_err());
            config.session_scope_key = Some("global".into());
            config.session_mode = None;
            assert!(validate_automation_source(&config).is_err());
            config.session_mode = Some("chat".into());
        }
        config.run_mode = RunMode::SelectedSession;
        config.target_session_id = Some("sess:11111111-2222-4333-8444-555555555555".into());
        config.target_session_transcript_path = Some("/catalog/selected.jsonl".into());
        config.target_session_mode = Some("code".into());
        config.target_session_scope_key = Some("selected-owner".into());
        assert!(validate_automation_source(&config).is_ok());
        for edit in [
            (|a: &mut CronAutomation| a.target_session_id = None) as fn(&mut CronAutomation),
            |a| a.target_session_transcript_path = Some("relative.jsonl".into()),
            |a| a.target_session_mode = Some("unknown".into()),
            |a| a.target_session_scope_key = Some(" ".into()),
        ] {
            let mut invalid = config.clone();
            edit(&mut invalid);
            assert!(validate_automation_source(&invalid).is_err());
        }
    }

    #[test]
    fn session_source_identity_requires_valid_matching_uuids() {
        let id = "11111111-2222-4333-8444-555555555555";
        assert!(same_session_id(Some(&format!("sess:{id}")), id));
        assert!(!same_session_id(Some("damaged-id"), "damaged-id"));
        assert!(!same_session_id(None, id));
        assert!(!same_session_id(
            Some(id),
            "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"
        ));
    }
}

/// Persist a run's exact session identity and task-owned source in the
/// same tasks-file transaction, before its first turn can start.
pub async fn bind_automation_run_session_with_source(
    fs: &dyn FileSystem,
    root: &Path,
    request: &AutomationRunRequest,
    session_id: &str,
    session_transcript_path: &str,
    session_mode: &str,
    session_scope_key: Option<&str>,
) -> Result<(), String> {
    let (id, source) = normalize_bound_source(
        session_id,
        session_transcript_path,
        session_mode,
        session_scope_key,
    )?;
    bind_automation_run_session_inner(fs, root, request, &id, source, OwnerBindingAction::Bind)
        .await
}

/// Durably reserve a complete owner before its transcript is created. This
/// never changes the task or run: a failed creation leaves only an allocated
/// ledger entry, and a later attempt may allocate a fresh session identity.
pub async fn reserve_automation_owner(
    fs: &dyn FileSystem,
    root: &Path,
    request: &AutomationRunRequest,
    session_id: &str,
    session_transcript_path: &str,
    session_mode: &str,
    session_scope_key: Option<&str>,
) -> Result<(), String> {
    let (id, source) = normalize_bound_source(
        session_id,
        session_transcript_path,
        session_mode,
        session_scope_key,
    )?;
    bind_automation_run_session_inner(
        fs,
        root,
        request,
        &id,
        source,
        OwnerBindingAction::ReserveOnly,
    )
    .await
}

fn normalize_bound_source<'a>(
    session_id: &str,
    session_transcript_path: &'a str,
    session_mode: &'a str,
    session_scope_key: Option<&'a str>,
) -> Result<(String, BoundSource<'a>), String> {
    let id = lingxi_core::types::SessionId::parse_prefixed(session_id)
        .ok_or("Invalid scheduled session identity")?
        .as_uuid()
        .to_string();
    if !Path::new(session_transcript_path).is_absolute()
        || !matches!(session_mode, "chat" | "code")
        || session_scope_key.is_none_or(|scope| scope.trim().is_empty() || scope.len() > 8192)
    {
        return Err("Invalid scheduled session source".into());
    }
    let source = BoundSource {
        path: session_transcript_path,
        mode: session_mode,
        scope: session_scope_key,
    };
    Ok((id, source))
}

#[derive(Clone, Copy)]
struct BoundSource<'a> {
    path: &'a str,
    mode: &'a str,
    scope: Option<&'a str>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OwnerBindingAction {
    ReserveOnly,
    Bind,
}

fn same_session_id(left: Option<&str>, right: &str) -> bool {
    let Some(left) = left else {
        return false;
    };
    matches!(
        (
            lingxi_core::types::SessionId::parse_prefixed(left),
            lingxi_core::types::SessionId::parse_prefixed(right)
        ),
        (Some(a), Some(b)) if a == b
    )
}

fn source_matches_snapshot(
    request: &AutomationRunRequest,
    id: &str,
    source: BoundSource<'_>,
) -> bool {
    let Some(config) = request.task.automation.as_ref() else {
        return false;
    };
    match config.run_mode {
        RunMode::SelectedSession => {
            same_session_id(config.target_session_id.as_deref(), id)
                && config.target_session_transcript_path.as_deref() == Some(source.path)
                && config.target_session_mode.as_deref() == Some(source.mode)
                && config.target_session_scope_key.as_deref() == source.scope
                && source.scope.is_some()
        }
        RunMode::NewSession | RunMode::TaskSession => {
            config.session_mode.as_deref() == Some(source.mode)
                && config.session_scope_key.as_deref() == source.scope
                && (config.run_mode != RunMode::TaskSession
                    || (config.owned_session_id.is_none()
                        && config.owned_session_transcript_path.is_none()
                        && config.owned_session_mode.is_none()
                        && config.owned_session_scope_key.is_none())
                    || (same_session_id(config.owned_session_id.as_deref(), id)
                        && config.owned_session_transcript_path.as_deref() == Some(source.path)
                        && config.owned_session_mode.as_deref() == Some(source.mode)
                        && config.owned_session_scope_key.as_deref() == source.scope))
        }
    }
}

async fn bind_automation_run_session_inner(
    fs: &dyn FileSystem,
    root: &Path,
    request: &AutomationRunRequest,
    session_id: &str,
    source: BoundSource<'_>,
    action: OwnerBindingAction,
) -> Result<(), String> {
    let _guard = crate::lock_cron_file().await;
    let scope = LockedAutomationScope::acquire(fs, root).await?;
    let body = crate::tasks_file::read_automation_tasks_body_pinned(
        fs,
        &scope.state_root,
        scope.state_identity.as_ref(),
    )
    .await
    .map_err(|e| e.to_string())?;
    scope.validate(fs).await?;
    let mut doc = crate::tasks_file::parse_automation_tasks_strict(&body)?;
    let task = doc
        .tasks
        .iter_mut()
        .find(|t| t.id == request.task.id)
        .ok_or("Scheduled task was removed")?;
    let stale_source = !same_execution_configuration(task, &request.task)
        || !first_owned_binding_matches_run(task, request)
        || !source_matches_snapshot(request, session_id, source);
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
    if expired || config.status != AutomationStatus::Active || stale_source {
        let reason = if expired {
            "Schedule expired before execution began"
        } else if stale_source {
            "Task source changed before execution began"
        } else {
            "Task stopped before execution began"
        };
        if action == OwnerBindingAction::ReserveOnly {
            return Err(format!("{AUTOMATION_CANCELLED_PREFIX}{reason}"));
        }
        run.status = AutomationRunStatus::Cancelled;
        run.started_at = None;
        run.finished_at = Some(now);
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
        scope.validate(fs).await?;
        crate::tasks_file::write_automation_tasks_body_pinned(
            fs,
            &scope.state_root,
            &crate::serialize_tasks(&doc),
            scope.state_identity.as_ref(),
        )
        .await
        .map_err(|e| e.to_string())?;
        scope.validate(fs).await?;
        return Err(format!("{AUTOMATION_CANCELLED_PREFIX}{reason}"));
    }
    let already_bound = run.session_transcript_path.is_some()
        || run.session_mode.is_some()
        || run.session_scope_key.is_some();
    if run
        .session_id
        .as_deref()
        .is_some_and(|id| !same_session_id(Some(id), session_id))
        || (already_bound
            && (run.session_transcript_path.as_deref() != Some(source.path)
                || run.session_mode.as_deref() != Some(source.mode)
                || run.session_scope_key.as_deref() != source.scope))
    {
        return Err("Run is already bound to a different session source".into());
    }
    // Record allocation first: if the subsequent task write or the process
    // fails, the independently durable owner remains available for recovery.
    let owner = OwnerReservation {
        session_id: session_id.into(),
        session_transcript_path: source.path.into(),
        session_mode: source.mode.into(),
        session_scope_key: source
            .scope
            .ok_or("Missing scheduled session owner")?
            .into(),
    };
    reserve_owner_locked(fs, &scope, &owner).await?;
    if action == OwnerBindingAction::ReserveOnly {
        return Ok(());
    }
    run.session_transcript_path = Some(source.path.to_owned());
    run.session_mode = Some(source.mode.to_owned());
    run.session_scope_key = source.scope.map(str::to_owned);
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
        config.owned_session_transcript_path = Some(source.path.to_owned());
        config.owned_session_mode = Some(source.mode.to_owned());
        config.owned_session_scope_key = source.scope.map(str::to_owned);
    }
    scope.validate(fs).await?;
    crate::tasks_file::write_automation_tasks_body_pinned(
        fs,
        &scope.state_root,
        &crate::serialize_tasks(&doc),
        scope.state_identity.as_ref(),
    )
    .await
    .map_err(|e| e.to_string())?;
    scope.validate(fs).await
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
                None => return None,
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
            session_transcript_path: None,
            session_mode: None,
            session_scope_key: None,
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
