//! Host-owned execution for journaled local-app background flows.
//!
//! Native schedulers only wake this broker. They never receive a capability
//! handle, app path, prompt, or flow source. Claiming, step execution, journal
//! advancement, retry classification, and terminal state all stay behind the
//! same Host boundary used by the foreground bridge.

use super::LocalAppsHostBroker;
use crate::mobile::host::LocalAppBackgroundRunDto;
use client::protocol::local_apps::{AppCapabilityKindDto, AppEventDto};
use local_apps::{AppCapability, BackgroundTaskStatus, CapabilityId};
use serde_json::{json, Map, Value};
use std::time::Duration;

const BACKGROUND_STEP_TIMEOUT: Duration = Duration::from_secs(120);
const BACKGROUND_RETRY_DELAY_MS: u64 = 15 * 60 * 1_000;
const BACKGROUND_RECOVERY_DELAY_MS: u64 = 15 * 60 * 1_000;
const MAX_BACKGROUND_RESULT_BYTES: usize = 64 * 1024;
pub(crate) const MAX_BACKGROUND_TASKS: usize = 32;

impl LocalAppsHostBroker {
    pub(crate) async fn acquire_background_process_lock(
        &self,
        app_id: &str,
    ) -> Result<rooted_fs::RootedFileLock, String> {
        let root = self.root.clone();
        let app_id = app_id.to_string();
        tokio::task::spawn_blocking(move || {
            local_apps::storage::lock_app_background(&root, &app_id)
                .map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| format!("background lock worker failed: {error}"))?
    }

    /// Claim and execute every due task in the profile. Duplicate native
    /// wake-ups are harmless: the in-memory claim set covers one process, the
    /// per-app file lock covers concurrent engine instances, and the persisted
    /// `Running`/journal state covers process death.
    pub(crate) async fn run_due_background_tasks(
        &self,
        now_ms: u64,
    ) -> Vec<LocalAppBackgroundRunDto> {
        let Some(service) = self.service().ok() else {
            return Vec::new();
        };
        let records = service.records().await;
        let mut outcomes = Vec::new();
        for record in records {
            let Ok(layout) = self.layout(&record.id) else {
                continue;
            };
            match Self::background_schedule_authorized(&layout) {
                Ok(true) => {}
                Ok(false) => {
                    if let Ok(revoked_outcomes) = self
                        .cancel_background_tasks_for_revoked_schedule(
                            &record.id,
                            "background scheduling permission was revoked",
                        )
                        .await
                    {
                        for outcome in revoked_outcomes {
                            self.emit_background_task_changed(&outcome).await;
                            outcomes.push(outcome);
                        }
                    }
                    continue;
                }
                Err(_) => continue,
            }
            let Ok(tasks) = local_apps::background::load_tasks(&layout) else {
                continue;
            };
            for task in tasks {
                if !matches!(
                    task.status,
                    BackgroundTaskStatus::Scheduled
                        | BackgroundTaskStatus::WaitingForSystem
                        | BackgroundTaskStatus::Running
                ) {
                    continue;
                }
                let Ok(journal) = local_apps::background::load_journal(&layout) else {
                    continue;
                };
                let Some(entry) = journal.iter().find(|entry| entry.task_id == task.task_id) else {
                    continue;
                };
                if entry
                    .next_run_at_ms
                    .is_some_and(|next_run_at_ms| next_run_at_ms > now_ms)
                {
                    continue;
                }
                let outcome = self
                    .run_background_task(&record.id, &task.task_id, now_ms, false)
                    .await;
                self.emit_background_task_changed(&outcome).await;
                outcomes.push(outcome);
            }
        }
        outcomes
    }

    /// Return the earliest persisted wake-up so iOS can submit a precise
    /// `earliestBeginDate`. Android uses its bounded watchdog because periodic
    /// WorkManager is the reliable path across reboot and exact-alarm policy.
    pub(crate) async fn next_background_wake_ms(&self, now_ms: u64) -> Option<u64> {
        let service = self.service().ok()?;
        let mut next = None;
        for record in service.records().await {
            let Ok(layout) = self.layout(&record.id) else {
                continue;
            };
            if !Self::background_schedule_authorized(&layout).unwrap_or(false) {
                continue;
            }
            let Ok(tasks) = local_apps::background::load_tasks(&layout) else {
                continue;
            };
            let Ok(journal) = local_apps::background::load_journal(&layout) else {
                continue;
            };
            for task in tasks {
                let Some(entry) = journal.iter().find(|entry| entry.task_id == task.task_id) else {
                    continue;
                };
                let wake = match task.status {
                    BackgroundTaskStatus::Scheduled | BackgroundTaskStatus::WaitingForSystem => {
                        entry.next_run_at_ms.unwrap_or(now_ms)
                    }
                    // A crashed engine leaves `Running` with no next run. Wake
                    // once after a bounded delay so the Host can fail closed
                    // instead of replaying an unknown side effect.
                    BackgroundTaskStatus::Running => entry
                        .next_run_at_ms
                        .unwrap_or_else(|| now_ms.saturating_add(BACKGROUND_RECOVERY_DELAY_MS)),
                    _ => continue,
                };
                next = Some(next.map_or(wake, |current: u64| current.min(wake)));
            }
        }
        next
    }

    /// Cancel one task from a trusted native management surface. A page may
    /// only request cancellation through the normal capability route.
    pub(crate) async fn cancel_background_task(&self, app_id: &str, task_id: &str) -> bool {
        let Ok(layout) = self.layout(app_id) else {
            return false;
        };
        let Ok(tasks) = local_apps::background::load_tasks(&layout) else {
            return false;
        };
        let Some(task) = tasks.iter().find(|task| task.task_id == task_id) else {
            return false;
        };
        if matches!(
            task.status,
            BackgroundTaskStatus::Succeeded | BackgroundTaskStatus::Cancelled
        ) {
            let _ = local_apps::background::clear_cancellation(&layout, task_id);
            return false;
        }
        // A running task may be inside a long LLM/network call. Write a
        // durable marker and return immediately; the executor consumes it at
        // the next journal boundary and persists terminal cancellation.
        if task.status == BackgroundTaskStatus::Running {
            // The executor only holds this lock at journal boundaries, so the
            // cancellation request remains non-blocking relative to a long
            // capability call while still racing safely with terminal writes.
            let _guard = self.background_task_writes.lock().await;
            let mut latest = match local_apps::background::load_tasks(&layout) {
                Ok(latest) => latest,
                Err(_) => return false,
            };
            let Some(latest_task) = latest.iter().find(|candidate| candidate.task_id == task_id)
            else {
                return false;
            };
            if latest_task.status == BackgroundTaskStatus::Running {
                return local_apps::background::request_cancellation(&layout, task_id).is_ok();
            }
            if matches!(
                latest_task.status,
                BackgroundTaskStatus::Succeeded | BackgroundTaskStatus::Cancelled
            ) {
                let _ = local_apps::background::clear_cancellation(&layout, task_id);
                return false;
            }
            return self
                .cancel_loaded_background_task(&layout, &mut latest, task_id)
                .unwrap_or(false);
        }

        let Ok(_process_lock) = self.acquire_background_process_lock(app_id).await else {
            return false;
        };
        let _guard = self.background_task_writes.lock().await;
        let Ok(mut tasks) = local_apps::background::load_tasks(&layout) else {
            return false;
        };
        self.cancel_loaded_background_task(&layout, &mut tasks, task_id)
            .unwrap_or(false)
    }

    fn cancel_loaded_background_task(
        &self,
        layout: &local_apps::AppLayout,
        tasks: &mut [local_apps::BackgroundTaskRecord],
        task_id: &str,
    ) -> Result<bool, String> {
        let task = tasks
            .iter_mut()
            .find(|task| task.task_id == task_id)
            .ok_or_else(|| "background task was not found".to_string())?;
        if matches!(
            task.status,
            BackgroundTaskStatus::Succeeded | BackgroundTaskStatus::Cancelled
        ) {
            local_apps::background::clear_cancellation(layout, task_id)
                .map_err(|error| error.to_string())?;
            return Ok(false);
        }
        local_apps::background::request_cancellation(layout, task_id)
            .map_err(|error| error.to_string())?;
        task.status = BackgroundTaskStatus::Cancelled;
        task.updated_at_ms = super::now_ms();
        let updated_at_ms = task.updated_at_ms;
        let mut journal =
            local_apps::background::load_journal(layout).map_err(|error| error.to_string())?;
        if let Some(entry) = journal.iter_mut().find(|entry| entry.task_id == task_id) {
            entry.next_step_id = None;
            entry.next_run_at_ms = None;
            entry.last_error = Some("cancelled by the host".into());
            entry.updated_at_ms = updated_at_ms;
        }
        local_apps::background::save_state(layout, tasks, &journal)
            .map_err(|error| error.to_string())?;
        local_apps::background::clear_cancellation(layout, task_id)
            .map_err(|error| error.to_string())?;
        Ok(true)
    }

    fn background_schedule_authorized(layout: &local_apps::AppLayout) -> Result<bool, String> {
        let manifest = local_apps::load_manifest(layout).map_err(|error| error.to_string())?;
        let permissions =
            local_apps::load_permissions(layout).map_err(|error| error.to_string())?;
        Ok(manifest
            .capabilities
            .contains(&AppCapability::BackgroundSchedule)
            && permissions.allows(AppCapability::BackgroundSchedule))
    }

    pub(crate) async fn cancel_background_tasks_for_revoked_schedule(
        &self,
        app_id: &str,
        reason: &str,
    ) -> Result<Vec<LocalAppBackgroundRunDto>, String> {
        let layout = self.layout(app_id)?;
        let _process_lock = self.acquire_background_process_lock(app_id).await?;
        let _guard = self.background_task_writes.lock().await;
        let mut tasks =
            local_apps::background::load_tasks(&layout).map_err(|error| error.to_string())?;
        let mut journal =
            local_apps::background::load_journal(&layout).map_err(|error| error.to_string())?;
        let now = super::now_ms();
        let mut outcomes = Vec::new();
        let mut changed_task_ids = Vec::new();
        for task in &mut tasks {
            if !matches!(
                task.status,
                BackgroundTaskStatus::Scheduled
                    | BackgroundTaskStatus::Running
                    | BackgroundTaskStatus::WaitingForSystem
            ) {
                continue;
            }
            task.status = BackgroundTaskStatus::Cancelled;
            task.updated_at_ms = now;
            if let Some(entry) = journal
                .iter_mut()
                .find(|entry| entry.task_id == task.task_id)
            {
                entry.next_step_id = None;
                entry.next_run_at_ms = None;
                entry.last_error = Some(reason.to_string());
                entry.updated_at_ms = now;
            }
            changed_task_ids.push(task.task_id.clone());
            outcomes.push(outcome(
                app_id,
                &task.task_id,
                "cancelled",
                None,
                Some(reason.to_string()),
                false,
            ));
        }
        if changed_task_ids.is_empty() {
            return Ok(outcomes);
        }
        local_apps::background::save_state(&layout, &tasks, &journal)
            .map_err(|error| error.to_string())?;
        for task_id in changed_task_ids {
            let _ = local_apps::background::clear_cancellation(&layout, &task_id);
        }
        Ok(outcomes)
    }

    pub(crate) async fn emit_background_task_changed(&self, outcome: &LocalAppBackgroundRunDto) {
        self.event_sink
            .emit(client::protocol::events::ClientEvent::AppEvent {
                event: AppEventDto::AppBackgroundTaskChanged {
                    app_id: outcome.app_id.clone(),
                    task_id: outcome.task_id.clone(),
                    status: outcome.status.clone(),
                    result_json: outcome.result_json.clone(),
                    error: outcome.error.clone(),
                    retryable: outcome.retryable,
                },
            })
            .await;
    }

    /// Requeue a failed or cancelled task immediately from a trusted app
    /// management surface. A running or already successful task is never
    /// rewound by this operation.
    pub(crate) async fn retry_background_task(
        &self,
        app_id: &str,
        task_id: &str,
    ) -> Result<bool, String> {
        let layout = self.layout(app_id)?;
        let _process_lock = self.acquire_background_process_lock(app_id).await?;
        let _guard = self.background_task_writes.lock().await;
        let mut tasks =
            local_apps::background::load_tasks(&layout).map_err(|error| error.to_string())?;
        let task = tasks
            .iter_mut()
            .find(|task| task.task_id == task_id)
            .ok_or_else(|| "background task was not found".to_string())?;
        if !matches!(
            task.status,
            BackgroundTaskStatus::Failed
                | BackgroundTaskStatus::WaitingForSystem
                | BackgroundTaskStatus::Cancelled
        ) {
            return Ok(false);
        }
        let now = super::now_ms();
        task.status = BackgroundTaskStatus::Scheduled;
        task.updated_at_ms = now;
        let mut journal =
            local_apps::background::load_journal(&layout).map_err(|error| error.to_string())?;
        let entry = journal
            .iter_mut()
            .find(|entry| entry.task_id == task_id)
            .ok_or_else(|| "background task has no journal entry".to_string())?;
        entry.next_step_id = entry
            .next_step_id
            .clone()
            .or_else(|| task.flow.steps.first().map(|step| step.step_id.clone()));
        entry.next_run_at_ms = Some(now);
        entry.last_error = None;
        entry.updated_at_ms = now;
        local_apps::background::save_state(&layout, &tasks, &journal)
            .map_err(|error| error.to_string())?;
        let _ = local_apps::background::clear_cancellation(&layout, task_id);
        Ok(true)
    }

    /// Build a bounded, app-scoped view of persisted background task state.
    /// Flow inputs are deliberately omitted; callers receive only lifecycle
    /// metadata and the bounded last result.
    pub(crate) fn background_task_summaries(
        layout: &local_apps::AppLayout,
        task_id: Option<&str>,
        status: Option<BackgroundTaskStatus>,
        limit: usize,
    ) -> Result<Vec<Value>, String> {
        let tasks =
            local_apps::background::load_tasks(layout).map_err(|error| error.to_string())?;
        let journal =
            local_apps::background::load_journal(layout).map_err(|error| error.to_string())?;
        Ok(tasks
            .iter()
            .filter(|task| task_id.is_none_or(|value| value == task.task_id))
            .filter(|task| status.is_none_or(|value| value == task.status))
            .take(limit)
            .map(|task| {
                let entry = journal.iter().find(|entry| entry.task_id == task.task_id);
                json!({
                    "task_id": task.task_id,
                    "flow_id": task.flow_id,
                    "trigger": task.trigger,
                    "status": task.status,
                    "updated_at_ms": task.updated_at_ms,
                    "next_step_id": entry.and_then(|entry| entry.next_step_id.clone()),
                    "next_run_at_ms": entry.and_then(|entry| entry.next_run_at_ms),
                    "attempt": entry.map(|entry| entry.attempt).unwrap_or(0),
                    "last_error": entry.and_then(|entry| entry.last_error.clone()),
                    "last_result_json": entry.and_then(|entry| entry.last_result_json.clone()),
                })
            })
            .collect())
    }

    async fn run_background_task(
        &self,
        app_id: &str,
        task_id: &str,
        now_ms: u64,
        force: bool,
    ) -> LocalAppBackgroundRunDto {
        let key = format!("{app_id}:{task_id}");
        {
            let mut inflight = self.background_inflight.lock().await;
            if !inflight.insert(key.clone()) {
                return outcome(app_id, task_id, "already_running", None, None, true);
            }
        }
        let process_lock = match self.acquire_background_process_lock(app_id).await {
            Ok(lock) => lock,
            Err(error) => {
                self.background_inflight.lock().await.remove(&key);
                return outcome(app_id, task_id, "failed", None, Some(error), false);
            }
        };
        let result = self
            .run_background_task_inner(app_id, task_id, now_ms, force)
            .await;
        drop(process_lock);
        self.background_inflight.lock().await.remove(&key);
        result
    }

    async fn run_background_task_inner(
        &self,
        app_id: &str,
        task_id: &str,
        now_ms: u64,
        force: bool,
    ) -> LocalAppBackgroundRunDto {
        let claimed = match self
            .claim_background_task(app_id, task_id, now_ms, force)
            .await
        {
            Ok(Some(value)) => value,
            Ok(None) => return outcome(app_id, task_id, "not_due", None, None, false),
            Err(error) => return outcome(app_id, task_id, "failed", None, Some(error), false),
        };
        let (task, mut journal) = claimed;
        if task.status == BackgroundTaskStatus::Cancelled {
            if let Ok(layout) = self.layout(app_id) {
                let _ = local_apps::background::clear_cancellation(&layout, task_id);
            }
            return outcome(app_id, task_id, "cancelled", None, None, false);
        }
        let start_index = task
            .flow
            .steps
            .iter()
            .position(|step| journal.next_step_id.as_deref() == Some(step.step_id.as_str()))
            .unwrap_or(0);
        let mut outputs = Map::new();
        for offset in start_index..task.flow.steps.len() {
            let layout = match self.layout(app_id) {
                Ok(layout) => layout,
                Err(error) => return outcome(app_id, task_id, "failed", None, Some(error), false),
            };
            let cancellation_requested =
                match local_apps::background::cancellation_requested(&layout, task_id) {
                    Ok(value) => value,
                    Err(error) => {
                        return outcome(
                            app_id,
                            task_id,
                            "failed",
                            None,
                            Some(error.to_string()),
                            false,
                        )
                    }
                };
            if cancellation_requested {
                return self
                    .finish_background_cancelled(app_id, task, journal, task_id)
                    .await;
            }
            let step = task.flow.steps[offset].clone();
            let input = match step_input(app_id, &step.input_json) {
                Ok(input) => input,
                Err(error) => {
                    return self
                        .finish_background_failure(
                            app_id,
                            task_id,
                            task,
                            journal,
                            step.step_id.clone(),
                            error,
                            false,
                        )
                        .await;
                }
            };
            let step_result = tokio::time::timeout(
                BACKGROUND_STEP_TIMEOUT,
                self.execute_background_step(
                    app_id,
                    &step.capability,
                    input,
                    task_id,
                    &step.step_id,
                ),
            )
            .await
            .map_err(|_| "background capability step timed out".to_string())
            .and_then(|result| result);
            let value = match step_result {
                Ok(value) => value,
                Err(error) => {
                    return self
                        .finish_background_failure(
                            app_id,
                            task_id,
                            task,
                            journal,
                            step.step_id.clone(),
                            error.clone(),
                            is_retryable(&error),
                        )
                        .await;
                }
            };
            outputs.insert(step.step_id.clone(), value);
            journal.next_step_id = task
                .flow
                .steps
                .get(offset + 1)
                .map(|next| next.step_id.clone());
            journal.updated_at_ms = super::now_ms();
            if let Err(error) = self.persist_journal(app_id, &journal).await {
                return outcome(app_id, task_id, "failed", None, Some(error), false);
            }
        }
        self.finish_background_success(app_id, task, journal, outputs, now_ms)
            .await
    }

    async fn claim_background_task(
        &self,
        app_id: &str,
        task_id: &str,
        now_ms: u64,
        force: bool,
    ) -> Result<
        Option<(
            local_apps::BackgroundTaskRecord,
            local_apps::BackgroundJournalEntry,
        )>,
        String,
    > {
        let layout = self.layout(app_id)?;
        let _guard = self.background_task_writes.lock().await;
        let mut tasks =
            local_apps::background::load_tasks(&layout).map_err(|error| error.to_string())?;
        let task = tasks
            .iter_mut()
            .find(|task| task.task_id == task_id)
            .ok_or_else(|| "background task was not found".to_string())?;
        if matches!(
            task.status,
            BackgroundTaskStatus::Succeeded | BackgroundTaskStatus::Cancelled
        ) {
            return Ok(None);
        }
        let mut journal =
            local_apps::background::load_journal(&layout).map_err(|error| error.to_string())?;
        let entry = journal
            .iter_mut()
            .find(|entry| entry.task_id == task_id)
            .ok_or_else(|| "background task has no journal entry".to_string())?;
        if local_apps::background::cancellation_requested(&layout, task_id)
            .map_err(|error| error.to_string())?
        {
            task.status = BackgroundTaskStatus::Cancelled;
            task.updated_at_ms = now_ms;
            entry.next_step_id = None;
            entry.next_run_at_ms = None;
            entry.last_error = Some("cancelled by the host".into());
            entry.updated_at_ms = now_ms;
            let task_snapshot = task.clone();
            let entry_snapshot = entry.clone();
            local_apps::background::save_state(&layout, &tasks, &journal)
                .map_err(|error| error.to_string())?;
            let _ = local_apps::background::clear_cancellation(&layout, task_id);
            return Ok(Some((task_snapshot, entry_snapshot)));
        }
        if task.status == BackgroundTaskStatus::Running && !force {
            task.status = BackgroundTaskStatus::Failed;
            task.updated_at_ms = now_ms;
            entry.next_run_at_ms = None;
            entry.last_error = Some(
                "background task was interrupted while a capability result was unknown; retry requires explicit confirmation".into(),
            );
            entry.updated_at_ms = now_ms;
            local_apps::background::save_state(&layout, &tasks, &journal)
                .map_err(|error| error.to_string())?;
            return Err(
                "background task was interrupted while a capability result was unknown; task marked failed to prevent replay".into(),
            );
        }
        if !force && entry.next_run_at_ms.is_some_and(|next| next > now_ms) {
            return Ok(None);
        }
        if entry.next_step_id.is_none() {
            entry.next_step_id = task.flow.steps.first().map(|step| step.step_id.clone());
        }
        task.status = BackgroundTaskStatus::Running;
        task.updated_at_ms = now_ms;
        entry.attempt = entry.attempt.saturating_add(1);
        entry.next_run_at_ms = None;
        entry.last_error = None;
        entry.updated_at_ms = now_ms;
        let task_snapshot = task.clone();
        let entry_snapshot = entry.clone();
        local_apps::background::save_state(&layout, &tasks, &journal)
            .map_err(|error| error.to_string())?;
        Ok(Some((task_snapshot, entry_snapshot)))
    }

    async fn finish_background_success(
        &self,
        app_id: &str,
        mut task: local_apps::BackgroundTaskRecord,
        mut journal: local_apps::BackgroundJournalEntry,
        outputs: Map<String, Value>,
        now_ms: u64,
    ) -> LocalAppBackgroundRunDto {
        if self
            .background_cancellation_requested(app_id, &task.task_id)
            .await
        {
            let task_id = task.task_id.clone();
            return self
                .finish_background_cancelled(app_id, task, journal, &task_id)
                .await;
        }
        let next_run = match task.trigger {
            local_apps::BackgroundTrigger::Schedule { interval_ms } => {
                task.status = BackgroundTaskStatus::Scheduled;
                journal.next_step_id = task.flow.steps.first().map(|step| step.step_id.clone());
                Some(now_ms.saturating_add(interval_ms))
            }
            local_apps::BackgroundTrigger::Event { .. } => {
                task.status = BackgroundTaskStatus::Succeeded;
                journal.next_step_id = None;
                None
            }
        };
        task.updated_at_ms = now_ms;
        let result = Value::Object(outputs);
        let result_json = bounded_json(&result);
        journal.next_run_at_ms = next_run;
        journal.last_result_json = result_json.clone();
        journal.last_error = None;
        journal.updated_at_ms = now_ms;
        match self
            .persist_terminal_background_state(app_id, &task, &journal)
            .await
        {
            Ok(true) => {
                if let Ok(layout) = self.layout(app_id) {
                    let _ = local_apps::background::clear_cancellation(&layout, &task.task_id);
                }
            }
            Ok(false) => {
                return outcome(
                    app_id,
                    &task.task_id,
                    "cancelled",
                    None,
                    Some("background task was cancelled while it was running".into()),
                    false,
                );
            }
            Err(error) => {
                return outcome(app_id, &task.task_id, "failed", None, Some(error), false);
            }
        }
        outcome(app_id, &task.task_id, "succeeded", result_json, None, false)
    }

    async fn finish_background_cancelled(
        &self,
        app_id: &str,
        mut task: local_apps::BackgroundTaskRecord,
        mut journal: local_apps::BackgroundJournalEntry,
        task_id: &str,
    ) -> LocalAppBackgroundRunDto {
        let now_ms = super::now_ms();
        task.status = BackgroundTaskStatus::Cancelled;
        task.updated_at_ms = now_ms;
        journal.next_step_id = None;
        journal.next_run_at_ms = None;
        journal.last_error = Some("cancelled by the host".into());
        journal.updated_at_ms = now_ms;
        let layout = self.layout(app_id).ok();
        let result = self
            .persist_terminal_background_state(app_id, &task, &journal)
            .await;
        match result {
            Ok(true) => {
                if let Some(layout) = layout {
                    let _ = local_apps::background::clear_cancellation(&layout, task_id);
                }
                outcome(app_id, task_id, "cancelled", None, None, false)
            }
            Ok(false) => outcome(
                app_id,
                task_id,
                "cancelled",
                None,
                Some("background task state changed before cancellation was persisted".into()),
                false,
            ),
            Err(error) => outcome(app_id, task_id, "failed", None, Some(error), false),
        }
    }

    async fn background_cancellation_requested(&self, app_id: &str, task_id: &str) -> bool {
        self.layout(app_id)
            .ok()
            .and_then(|layout| {
                local_apps::background::cancellation_requested(&layout, task_id).ok()
            })
            .unwrap_or(false)
    }

    async fn finish_background_failure(
        &self,
        app_id: &str,
        task_id: &str,
        mut task: local_apps::BackgroundTaskRecord,
        mut journal: local_apps::BackgroundJournalEntry,
        step_id: String,
        error: String,
        retryable: bool,
    ) -> LocalAppBackgroundRunDto {
        if self
            .background_cancellation_requested(app_id, task_id)
            .await
        {
            return self
                .finish_background_cancelled(app_id, task, journal, task_id)
                .await;
        }
        let now_ms = super::now_ms();
        task.status = if retryable {
            BackgroundTaskStatus::WaitingForSystem
        } else {
            BackgroundTaskStatus::Failed
        };
        task.updated_at_ms = now_ms;
        journal.next_step_id = Some(step_id);
        journal.next_run_at_ms =
            retryable.then_some(now_ms.saturating_add(BACKGROUND_RETRY_DELAY_MS));
        journal.last_error = Some(error.clone());
        journal.updated_at_ms = now_ms;
        match self
            .persist_terminal_background_state(app_id, &task, &journal)
            .await
        {
            Ok(true) => {
                if let Ok(layout) = self.layout(app_id) {
                    let _ = local_apps::background::clear_cancellation(&layout, task_id);
                }
            }
            Ok(false) => {
                return outcome(
                    app_id,
                    task_id,
                    "cancelled",
                    None,
                    Some("background task was cancelled while it was running".into()),
                    false,
                );
            }
            Err(persist_error) => {
                return outcome(app_id, task_id, "failed", None, Some(persist_error), false);
            }
        }
        outcome(
            app_id,
            task_id,
            if retryable {
                "waiting_for_system"
            } else {
                "failed"
            },
            None,
            Some(error),
            retryable,
        )
    }

    async fn persist_journal(
        &self,
        app_id: &str,
        journal: &local_apps::BackgroundJournalEntry,
    ) -> Result<(), String> {
        let layout = self.layout(app_id)?;
        let _guard = self.background_task_writes.lock().await;
        let mut entries =
            local_apps::background::load_journal(&layout).map_err(|error| error.to_string())?;
        let entry = entries
            .iter_mut()
            .find(|entry| entry.task_id == journal.task_id)
            .ok_or_else(|| "background task has no journal entry".to_string())?;
        *entry = journal.clone();
        local_apps::background::save_journal(&layout, &entries).map_err(|error| error.to_string())
    }

    async fn persist_terminal_background_state(
        &self,
        app_id: &str,
        task: &local_apps::BackgroundTaskRecord,
        journal: &local_apps::BackgroundJournalEntry,
    ) -> Result<bool, String> {
        let layout = self.layout(app_id)?;
        let _guard = self.background_task_writes.lock().await;
        let mut tasks =
            local_apps::background::load_tasks(&layout).map_err(|error| error.to_string())?;
        let current = tasks
            .iter_mut()
            .find(|candidate| candidate.task_id == task.task_id)
            .ok_or_else(|| "background task was removed while it was running".to_string())?;
        if !matches!(current.status, BackgroundTaskStatus::Running) {
            return Ok(false);
        }
        *current = task.clone();

        let mut journal_entries =
            local_apps::background::load_journal(&layout).map_err(|error| error.to_string())?;
        let current_journal = journal_entries
            .iter_mut()
            .find(|candidate| candidate.task_id == journal.task_id)
            .ok_or_else(|| {
                "background task journal was removed while it was running".to_string()
            })?;
        *current_journal = journal.clone();

        local_apps::background::save_state(&layout, &tasks, &journal_entries)
            .map_err(|error| error.to_string())?;
        Ok(true)
    }

    async fn execute_background_step(
        &self,
        app_id: &str,
        capability: &CapabilityId,
        mut input: Value,
        task_id: &str,
        step_id: &str,
    ) -> Result<Value, String> {
        let object = input
            .as_object_mut()
            .ok_or_else(|| "background step input must be a JSON object".to_string())?;
        object.insert("app_id".into(), Value::String(app_id.to_string()));
        let request_id = format!("background:{task_id}:{step_id}");
        match capability {
            CapabilityId::DataQuery => self.query_data_value(input).await,
            CapabilityId::DataMutate => {
                self.ensure_background_step_authorized(app_id, *capability, &input)
                    .await?;
                self.mutate_data_value(input, false, None).await
            }
            CapabilityId::RuntimeStatus => {
                let runtime = self
                    .service()?
                    .runtime_record(app_id)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(json!(runtime))
            }
            CapabilityId::NetworkRequest => {
                self.ensure_background_step_authorized(app_id, *capability, &input)
                    .await?;
                self.network_request(app_id, input).await
            }
            CapabilityId::Notifications => {
                self.ensure_background_step_authorized(app_id, *capability, &input)
                    .await?;
                self.post_notification_value(app_id, &input)
                    .await
                    .map_err(|failure| failure.message)
            }
            CapabilityId::LlmComplete => {
                self.ensure_background_step_authorized(app_id, *capability, &input)
                    .await?;
                self.llm_chat_value(app_id, &input)
                    .await
                    .map_err(|failure| failure.message)
            }
            CapabilityId::AgentSessionCreate => {
                self.ensure_background_step_authorized(app_id, *capability, &input)
                    .await?;
                self.agent_session_create_value(input).await
            }
            CapabilityId::AgentSessionList => {
                self.ensure_background_step_authorized(app_id, *capability, &input)
                    .await?;
                self.agent_session_list_value(input).await
            }
            CapabilityId::AgentSessionResume | CapabilityId::AgentSessionClose => {
                self.ensure_background_step_authorized(app_id, *capability, &input)
                    .await?;
                self.agent_session_update_value(input).await
            }
            CapabilityId::AgentSend => {
                self.ensure_background_step_authorized(app_id, *capability, &input)
                    .await?;
                self.agent_send_value(app_id, &request_id, &input)
                    .await
                    .map_err(|failure| failure.message)
            }
            CapabilityId::AgentEmit => {
                self.ensure_background_step_authorized(app_id, *capability, &input)
                    .await?;
                self.agent_post_value(app_id, &input)
                    .await
                    .map_err(|failure| failure.message)
            }
            _ => Err(format!(
                "background capability {} has no headless executor",
                capability.as_str()
            )),
        }
    }

    /// Called while registering a schedule. It may show the normal foreground
    /// approval prompt, but only durable grants are accepted for later wakes.
    pub(crate) async fn authorize_background_schedule_step(
        &self,
        app_id: &str,
        capability: CapabilityId,
        input_json: &str,
    ) -> Result<(), String> {
        let input = step_input(app_id, input_json)?;
        let (grant, wire, reason) = match capability {
            CapabilityId::DataMutate => (
                Some(AppCapability::DataMutation),
                Some(AppCapabilityKindDto::DataMutation),
                "后台流程请求修改应用数据。",
            ),
            CapabilityId::Notifications => (
                Some(AppCapability::Notifications),
                Some(AppCapabilityKindDto::Notifications),
                "后台流程请求发送应用通知。",
            ),
            CapabilityId::LlmComplete
            | CapabilityId::AgentSessionCreate
            | CapabilityId::AgentSessionList
            | CapabilityId::AgentSessionResume
            | CapabilityId::AgentSessionClose
            | CapabilityId::AgentSend => (
                Some(AppCapability::Llm),
                Some(AppCapabilityKindDto::Llm),
                "后台流程请求使用应用 Agent/LLM。",
            ),
            CapabilityId::AgentEmit => (
                Some(AppCapability::AgentNotify),
                Some(AppCapabilityKindDto::AgentNotify),
                "后台流程请求向 Agent 投递事件。",
            ),
            CapabilityId::NetworkRequest => {
                let url = input
                    .get("url")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "background network step requires url".to_string())?;
                let parsed = reqwest::Url::parse(url)
                    .map_err(|error| format!("invalid background network URL: {error}"))?;
                let domain = parsed
                    .host_str()
                    .ok_or_else(|| "background network URL has no hostname".to_string())?
                    .to_string();
                self.authorize_domain(app_id, &domain).await?;
                let layout = self.layout(app_id)?;
                let permissions =
                    local_apps::load_permissions(&layout).map_err(|error| error.to_string())?;
                if !permissions.allows_domain(&domain) {
                    return Err("background network access requires durable approval".into());
                }
                return Ok(());
            }
            CapabilityId::DataQuery | CapabilityId::RuntimeStatus => return Ok(()),
            _ => {
                return Err(format!(
                    "background capability {} is not supported",
                    capability.as_str()
                ));
            }
        };
        if let (Some(grant), Some(wire)) = (grant, wire) {
            let layout = self.layout(app_id)?;
            let manifest = local_apps::load_manifest(&layout).map_err(|error| error.to_string())?;
            if !manifest.capabilities.contains(&grant) {
                return Err(format!(
                    "background capability {} is not declared in the app manifest",
                    capability.as_str()
                ));
            }
            self.authorize_capability(app_id, grant, wire, reason)
                .await?;
            let permissions =
                local_apps::load_permissions(&layout).map_err(|error| error.to_string())?;
            if !permissions.allows(grant) {
                return Err("background capabilities require durable approval".into());
            }
        }
        Ok(())
    }

    async fn ensure_background_step_authorized(
        &self,
        app_id: &str,
        capability: CapabilityId,
        input: &Value,
    ) -> Result<(), String> {
        let layout = self.layout(app_id)?;
        let manifest = local_apps::load_manifest(&layout).map_err(|error| error.to_string())?;
        let permissions =
            local_apps::load_permissions(&layout).map_err(|error| error.to_string())?;
        let grant = match capability {
            CapabilityId::DataMutate => Some(AppCapability::DataMutation),
            CapabilityId::Notifications => Some(AppCapability::Notifications),
            CapabilityId::LlmComplete
            | CapabilityId::AgentSessionCreate
            | CapabilityId::AgentSessionList
            | CapabilityId::AgentSessionResume
            | CapabilityId::AgentSessionClose
            | CapabilityId::AgentSend => Some(AppCapability::Llm),
            CapabilityId::AgentEmit => Some(AppCapability::AgentNotify),
            CapabilityId::NetworkRequest => None,
            CapabilityId::DataQuery | CapabilityId::RuntimeStatus => return Ok(()),
            _ => {
                return Err(format!(
                    "background capability {} is not supported",
                    capability.as_str()
                ))
            }
        };
        if let Some(grant) = grant {
            if !manifest.capabilities.contains(&grant) || !permissions.allows(grant) {
                return Err(format!(
                    "background capability {} does not have a durable app grant",
                    capability.as_str()
                ));
            }
            return Ok(());
        }
        let url = input
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| "background network step requires url".to_string())?;
        let parsed = reqwest::Url::parse(url).map_err(|error| error.to_string())?;
        let domain = parsed
            .host_str()
            .ok_or_else(|| "background network URL has no hostname".to_string())?;
        if !manifest
            .allowed_domains
            .iter()
            .any(|allowed| allowed == domain)
            || !permissions.allows_domain(domain)
        {
            return Err(format!(
                "background network domain {domain:?} does not have a durable app grant"
            ));
        }
        Ok(())
    }
}

fn step_input(app_id: &str, input_json: &str) -> Result<Value, String> {
    let mut input: Value = serde_json::from_str(input_json)
        .map_err(|error| format!("invalid background step input: {error}"))?;
    let object = input
        .as_object_mut()
        .ok_or_else(|| "background step input must be a JSON object".to_string())?;
    object.insert("app_id".into(), Value::String(app_id.to_string()));
    Ok(input)
}

fn bounded_json(value: &Value) -> Option<String> {
    let body = value.to_string();
    (body.len() <= MAX_BACKGROUND_RESULT_BYTES).then_some(body)
}

fn is_retryable(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    [
        "timeout",
        "timed out",
        "network",
        "connection",
        "dns",
        "429",
        "rate limit",
        "http 5",
        "temporarily unavailable",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn outcome(
    app_id: &str,
    task_id: &str,
    status: &str,
    result_json: Option<String>,
    error: Option<String>,
    retryable: bool,
) -> LocalAppBackgroundRunDto {
    LocalAppBackgroundRunDto {
        app_id: app_id.to_string(),
        task_id: task_id.to_string(),
        status: status.to_string(),
        result_json,
        error,
        retryable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mobile::local_apps_mcp::LocalAppsMcpHost;
    use async_trait::async_trait;
    use client::adapter::ClientEventSink;
    use client::protocol::events::ClientEvent;
    use local_apps::test_support::FixedClock;
    use local_apps::{
        AppService, BackgroundJournalEntry, BackgroundTaskRecord, BackgroundTrigger,
        NoopAppEventObserver, RUNTIME_CONTRACT_SCHEMA_VERSION,
    };
    use std::sync::Arc;
    use tempfile::TempDir;

    #[derive(Default)]
    struct Sink;

    #[async_trait]
    impl ClientEventSink for Sink {
        async fn emit(&self, _event: ClientEvent) {}
    }

    async fn harness() -> (TempDir, Arc<AppService>, Arc<LocalAppsHostBroker>, String) {
        let root = tempfile::tempdir().expect("tempdir");
        let service = Arc::new(
            AppService::load(
                root.path(),
                Arc::new(FixedClock::new(1)),
                Arc::new(NoopAppEventObserver),
            )
            .await
            .expect("service"),
        );
        let broker =
            LocalAppsHostBroker::new(root.path().to_path_buf(), Arc::new(Sink), None, false, None);
        assert!(
            broker.attach_service(service.clone()).is_ok(),
            "attach service"
        );
        let record = service
            .create_app(Some("Background"), "background app", None)
            .await
            .expect("create app");
        let layout = local_apps::AppLayout::new(root.path(), record.id.clone()).expect("layout");
        let mut manifest = local_apps::load_manifest(&layout).expect("manifest");
        manifest
            .capabilities
            .push(AppCapability::BackgroundSchedule);
        local_apps::save_manifest(&layout, &manifest).expect("manifest grant");
        let mut permissions = local_apps::load_permissions(&layout).expect("permissions");
        permissions
            .always_allowed_capabilities
            .insert(AppCapability::BackgroundSchedule);
        local_apps::save_permissions(&layout, &permissions).expect("permission grant");
        (root, service, broker, record.id)
    }

    #[tokio::test]
    async fn scheduled_runtime_status_flow_advances_and_rearms() {
        let (root, _service, broker, app_id) = harness().await;
        let layout = local_apps::AppLayout::new(root.path(), app_id.clone()).expect("layout");
        let flow = local_apps::FlowDefinition {
            flow_id: "flow-status".into(),
            version: 1,
            steps: vec![local_apps::FlowStep {
                step_id: "status".into(),
                capability: CapabilityId::RuntimeStatus,
                depends_on: Vec::new(),
                input_json: "{}".into(),
            }],
        };
        local_apps::background::save_tasks(
            &layout,
            &[BackgroundTaskRecord {
                schema_version: RUNTIME_CONTRACT_SCHEMA_VERSION,
                task_id: "task-status".into(),
                app_id: app_id.clone(),
                flow_id: flow.flow_id.clone(),
                flow,
                trigger: BackgroundTrigger::Schedule {
                    interval_ms: 900_000,
                },
                status: BackgroundTaskStatus::Scheduled,
                updated_at_ms: 1,
            }],
        )
        .expect("tasks");
        local_apps::background::save_journal(
            &layout,
            &[BackgroundJournalEntry {
                task_id: "task-status".into(),
                flow_id: "flow-status".into(),
                next_step_id: Some("status".into()),
                next_run_at_ms: Some(1),
                last_result_json: None,
                attempt: 0,
                last_error: None,
                updated_at_ms: 1,
            }],
        )
        .expect("journal");

        let first = broker.run_due_background_tasks(1_000).await;
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].status, "succeeded");
        let tasks = local_apps::background::load_tasks(&layout).expect("load tasks");
        assert_eq!(tasks[0].status, BackgroundTaskStatus::Scheduled);
        let journal = local_apps::background::load_journal(&layout).expect("load journal");
        assert!(journal[0].next_run_at_ms.unwrap_or_default() > 1_000);
        let result_json = journal[0]
            .last_result_json
            .as_deref()
            .expect("successful result is persisted in the journal");
        let result: serde_json::Value =
            serde_json::from_str(result_json).expect("persisted result is valid JSON");
        assert!(result.get("status").is_some());

        let second = broker.run_due_background_tasks(1_000).await;
        assert!(
            second.is_empty(),
            "a rearmed task must not run before its interval"
        );
    }

    #[tokio::test]
    async fn finishing_one_task_preserves_sibling_tasks_and_journals() {
        let (root, _service, broker, app_id) = harness().await;
        let layout = local_apps::AppLayout::new(root.path(), app_id.clone()).expect("layout");
        let mut tasks = Vec::new();
        let mut journal = Vec::new();
        for index in 0..2 {
            let task_id = format!("task-{index}");
            let flow_id = format!("flow-{index}");
            let step_id = format!("status-{index}");
            let flow = local_apps::FlowDefinition {
                flow_id: flow_id.clone(),
                version: 1,
                steps: vec![local_apps::FlowStep {
                    step_id: step_id.clone(),
                    capability: CapabilityId::RuntimeStatus,
                    depends_on: Vec::new(),
                    input_json: "{}".into(),
                }],
            };
            tasks.push(BackgroundTaskRecord {
                schema_version: RUNTIME_CONTRACT_SCHEMA_VERSION,
                task_id: task_id.clone(),
                app_id: app_id.clone(),
                flow_id: flow_id.clone(),
                flow,
                trigger: BackgroundTrigger::Schedule {
                    interval_ms: 900_000,
                },
                status: BackgroundTaskStatus::Scheduled,
                updated_at_ms: 1,
            });
            journal.push(BackgroundJournalEntry {
                task_id,
                flow_id,
                next_step_id: Some(step_id),
                next_run_at_ms: Some(1),
                last_result_json: None,
                attempt: 0,
                last_error: None,
                updated_at_ms: 1,
            });
        }
        local_apps::background::save_tasks(&layout, &tasks).expect("tasks");
        local_apps::background::save_journal(&layout, &journal).expect("journal");

        let outcomes = broker.run_due_background_tasks(1_000).await;
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes.iter().all(|outcome| outcome.status == "succeeded"));
        let persisted_tasks = local_apps::background::load_tasks(&layout).expect("load tasks");
        let persisted_journal =
            local_apps::background::load_journal(&layout).expect("load journal");
        assert_eq!(persisted_tasks.len(), 2);
        assert_eq!(persisted_journal.len(), 2);
        assert!(persisted_tasks
            .iter()
            .all(|task| task.status == BackgroundTaskStatus::Scheduled));
    }

    #[tokio::test]
    async fn task_management_exposes_bounded_results_and_requeues_failures() {
        let (root, _service, broker, app_id) = harness().await;
        let layout = local_apps::AppLayout::new(root.path(), app_id.clone()).expect("layout");
        let flow = local_apps::FlowDefinition {
            flow_id: "flow-management".into(),
            version: 1,
            steps: vec![local_apps::FlowStep {
                step_id: "status".into(),
                capability: CapabilityId::RuntimeStatus,
                depends_on: Vec::new(),
                input_json: "{}".into(),
            }],
        };
        local_apps::background::save_tasks(
            &layout,
            &[BackgroundTaskRecord {
                schema_version: RUNTIME_CONTRACT_SCHEMA_VERSION,
                task_id: "task-management".into(),
                app_id: app_id.clone(),
                flow_id: flow.flow_id.clone(),
                flow,
                trigger: BackgroundTrigger::Schedule {
                    interval_ms: 900_000,
                },
                status: BackgroundTaskStatus::Failed,
                updated_at_ms: 1,
            }],
        )
        .expect("tasks");
        local_apps::background::save_journal(
            &layout,
            &[BackgroundJournalEntry {
                task_id: "task-management".into(),
                flow_id: "flow-management".into(),
                next_step_id: Some("status".into()),
                next_run_at_ms: Some(9_000),
                last_result_json: Some("{\"status\":\"old\"}".into()),
                attempt: 2,
                last_error: Some("temporary failure".into()),
                updated_at_ms: 1,
            }],
        )
        .expect("journal");

        let summaries = LocalAppsHostBroker::background_task_summaries(
            &layout,
            Some("task-management"),
            Some(BackgroundTaskStatus::Failed),
            10,
        )
        .expect("summaries");
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0]["status"], "failed");
        assert_eq!(summaries[0]["last_result_json"], "{\"status\":\"old\"}");

        assert!(broker
            .retry_background_task(&app_id, "task-management")
            .await
            .expect("retry"));
        let tasks = local_apps::background::load_tasks(&layout).expect("reloaded tasks");
        assert_eq!(tasks[0].status, BackgroundTaskStatus::Scheduled);
        let journal = local_apps::background::load_journal(&layout).expect("reloaded journal");
        assert!(journal[0].next_run_at_ms.is_some());
        assert!(journal[0].last_error.is_none());
    }

    #[tokio::test]
    async fn scheduling_reclaims_terminal_tasks_and_avoids_reused_ids() {
        let (root, service, broker, app_id) = harness().await;
        let flow = serde_json::json!({
            "flowId": "flow-schedule",
            "version": 1,
            "steps": [{
                "stepId": "status",
                "capability": "runtime.status",
                "dependsOn": [],
                "inputJson": "{}"
            }]
        });
        let request = || {
            serde_json::json!({
                "app_id": app_id.clone(),
                "interval_ms": 900_000,
                "flow": flow.clone()
            })
        };
        let first = broker
            .background_schedule(request())
            .await
            .expect("first schedule");
        let first_id = first["task"]["taskId"].as_str().expect("first task id");

        let restarted =
            LocalAppsHostBroker::new(root.path().to_path_buf(), Arc::new(Sink), None, false, None);
        assert!(
            restarted.attach_service(service).is_ok(),
            "attach restarted service"
        );
        let second = restarted
            .background_schedule(request())
            .await
            .expect("restart schedule");
        let second_id = second["task"]["taskId"].as_str().expect("second task id");
        assert_ne!(first_id, second_id, "restart must not reuse a task id");

        for _ in 2..MAX_BACKGROUND_TASKS {
            broker
                .background_schedule(request())
                .await
                .expect("fill active task capacity");
        }
        let layout = local_apps::AppLayout::new(root.path(), app_id.clone()).expect("layout");
        let mut tasks = local_apps::background::load_tasks(&layout).expect("tasks");
        assert_eq!(tasks.len(), MAX_BACKGROUND_TASKS);
        let evicted_id = tasks[0].task_id.clone();
        tasks[0].status = BackgroundTaskStatus::Cancelled;
        local_apps::background::save_tasks(&layout, &tasks).expect("cancel terminal task");

        broker
            .background_schedule(request())
            .await
            .expect("schedule after terminal task");
        let tasks = local_apps::background::load_tasks(&layout).expect("tasks");
        assert_eq!(tasks.len(), MAX_BACKGROUND_TASKS);
        assert!(tasks.iter().all(|task| task.task_id != evicted_id));
    }

    #[tokio::test]
    async fn running_task_does_not_schedule_an_immediate_wakeup() {
        let (root, _service, broker, app_id) = harness().await;
        let layout = local_apps::AppLayout::new(root.path(), app_id.clone()).expect("layout");
        let flow = local_apps::FlowDefinition {
            flow_id: "flow-running".into(),
            version: 1,
            steps: vec![local_apps::FlowStep {
                step_id: "status".into(),
                capability: CapabilityId::RuntimeStatus,
                depends_on: Vec::new(),
                input_json: "{}".into(),
            }],
        };
        local_apps::background::save_tasks(
            &layout,
            &[BackgroundTaskRecord {
                schema_version: RUNTIME_CONTRACT_SCHEMA_VERSION,
                task_id: "task-running".into(),
                app_id: app_id.clone(),
                flow_id: flow.flow_id.clone(),
                flow,
                trigger: BackgroundTrigger::Schedule {
                    interval_ms: 900_000,
                },
                status: BackgroundTaskStatus::Running,
                updated_at_ms: 1,
            }],
        )
        .expect("tasks");
        local_apps::background::save_journal(
            &layout,
            &[BackgroundJournalEntry {
                task_id: "task-running".into(),
                flow_id: "flow-running".into(),
                next_step_id: Some("status".into()),
                next_run_at_ms: None,
                last_result_json: None,
                attempt: 1,
                last_error: None,
                updated_at_ms: 1,
            }],
        )
        .expect("journal");

        assert_eq!(
            broker.next_background_wake_ms(1_000).await,
            Some(1_000 + BACKGROUND_RECOVERY_DELAY_MS)
        );

        // Cancellation must not wait for the long-running executor lock.
        let process_lock = broker
            .acquire_background_process_lock(&app_id)
            .await
            .expect("background process lock");
        let cancelled = tokio::time::timeout(
            Duration::from_secs(1),
            broker.cancel_background_task(&app_id, "task-running"),
        )
        .await
        .expect("cancellation should return while lock is held");
        assert!(cancelled);
        assert!(
            local_apps::background::cancellation_requested(&layout, "task-running")
                .expect("cancellation marker")
        );
        drop(process_lock);
    }

    #[tokio::test]
    async fn interrupted_running_task_fails_closed_instead_of_replaying() {
        let (root, _service, broker, app_id) = harness().await;
        let layout = local_apps::AppLayout::new(root.path(), app_id.clone()).expect("layout");
        let flow = local_apps::FlowDefinition {
            flow_id: "flow-interrupted".into(),
            version: 1,
            steps: vec![local_apps::FlowStep {
                step_id: "status".into(),
                capability: CapabilityId::RuntimeStatus,
                depends_on: Vec::new(),
                input_json: "{}".into(),
            }],
        };
        local_apps::background::save_tasks(
            &layout,
            &[BackgroundTaskRecord {
                schema_version: RUNTIME_CONTRACT_SCHEMA_VERSION,
                task_id: "task-interrupted".into(),
                app_id: app_id.clone(),
                flow_id: flow.flow_id.clone(),
                flow,
                trigger: BackgroundTrigger::Schedule {
                    interval_ms: 900_000,
                },
                status: BackgroundTaskStatus::Running,
                updated_at_ms: 1,
            }],
        )
        .expect("tasks");
        local_apps::background::save_journal(
            &layout,
            &[BackgroundJournalEntry {
                task_id: "task-interrupted".into(),
                flow_id: "flow-interrupted".into(),
                next_step_id: Some("status".into()),
                next_run_at_ms: None,
                last_result_json: None,
                attempt: 1,
                last_error: None,
                updated_at_ms: 1,
            }],
        )
        .expect("journal");

        let outcomes = broker.run_due_background_tasks(1_000).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].status, "failed");
        assert!(outcomes[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("unknown")));
        let tasks = local_apps::background::load_tasks(&layout).expect("tasks");
        assert_eq!(tasks[0].status, BackgroundTaskStatus::Failed);
        let journal = local_apps::background::load_journal(&layout).expect("journal");
        assert!(journal[0]
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("retry requires explicit confirmation")));
    }

    #[tokio::test]
    async fn revoked_background_schedule_cancels_existing_tasks() {
        let (root, service, broker, app_id) = harness().await;
        let layout = local_apps::AppLayout::new(root.path(), app_id.clone()).expect("layout");
        let mut permissions = local_apps::load_permissions(&layout).expect("permissions");
        permissions.always_allowed_capabilities.clear();
        local_apps::save_permissions(&layout, &permissions).expect("revoke permissions");
        let flow = local_apps::FlowDefinition {
            flow_id: "flow-revoked".into(),
            version: 1,
            steps: vec![local_apps::FlowStep {
                step_id: "status".into(),
                capability: CapabilityId::RuntimeStatus,
                depends_on: Vec::new(),
                input_json: "{}".into(),
            }],
        };
        local_apps::background::save_tasks(
            &layout,
            &[BackgroundTaskRecord {
                schema_version: RUNTIME_CONTRACT_SCHEMA_VERSION,
                task_id: "task-revoked".into(),
                app_id: app_id.clone(),
                flow_id: flow.flow_id.clone(),
                flow,
                trigger: BackgroundTrigger::Schedule {
                    interval_ms: 900_000,
                },
                status: BackgroundTaskStatus::Scheduled,
                updated_at_ms: 1,
            }],
        )
        .expect("tasks");
        local_apps::background::save_journal(
            &layout,
            &[BackgroundJournalEntry {
                task_id: "task-revoked".into(),
                flow_id: "flow-revoked".into(),
                next_step_id: Some("status".into()),
                next_run_at_ms: Some(1),
                last_result_json: None,
                attempt: 0,
                last_error: None,
                updated_at_ms: 1,
            }],
        )
        .expect("journal");

        let outcomes = broker.run_due_background_tasks(1_000).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].status, "cancelled");
        let tasks = local_apps::background::load_tasks(&layout).expect("tasks");
        assert_eq!(tasks[0].status, BackgroundTaskStatus::Cancelled);
        // The service remains attached and the cancellation is durable, not an
        // in-memory scheduler decision.
        assert!(service.record(&app_id).await.is_ok());
    }
}
