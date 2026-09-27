//! Desktop management surface over the scheduler's authoritative task file.
use client_protocol::{commands::CronRequestDto, events::CronJobDto};
use cron::tasks_file::{CronTask, ScheduledTasks};
use platform_api::task_registry::TaskRegistryHandle;
use platform_api::{FileSystem, FsError};
use std::{path::Path, sync::Arc};

/// Upgrade old durable tasks once before a scheduler loads them. The write is
/// atomic and takes the scheduler's locks; repeated boots retain fixed settings.
pub async fn migrate_legacy(
    fs: &dyn FileSystem,
    cwd: &Path,
    model: Option<String>,
    reasoning: serde_json::Value,
) -> Result<(), String> {
    let _process = cron::lock_cron_file().await;
    let body = match cron::tasks_file::read_automation_tasks_body(fs, cwd).await {
        Ok(body) => body,
        Err(FsError::NotFound(_)) => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    let _disk = cron::tasks_file::lock_automation_tasks(fs, cwd)
        .await
        .map_err(|e| e.to_string())?;
    // Re-read after the cross-process lock; another host may have migrated.
    // Falling back to the PRE-lock snapshot would defeat the lock: the write
    // below would replay a stale document over whatever the other host just
    // committed. `NotFound` is the one benign outcome (no store yet).
    let body = match cron::tasks_file::read_automation_tasks_body(fs, cwd).await {
        Ok(body) => body,
        Err(platform_api::FsError::NotFound(_)) => body,
        Err(error) => return Err(error.to_string()),
    };
    let mut doc =
        cron::tasks_file::parse_automation_tasks_strict(&body).map_err(|e| e.to_string())?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let mut changed = false;
    for task in &mut doc.tasks {
        // Tool-created /loop jobs retain their session ownership and seven-day
        // lifetime. Only legacy task-center records migrate to v2 automation.
        if task.automation.is_some()
            || task.permanent == Some(true)
            || cron::is_loop_default_sentinel(&task.prompt)
            || task.creator.created_by_session_id.is_some()
            || task.creator.created_by_pid.is_some()
        {
            continue;
        }
        changed = true;
        let expired = task.expires_at.is_some_and(|expiry| expiry <= now);
        task.automation = Some(cron::automation::CronAutomation {
            name: None,
            version: 2,
            status: if expired {
                cron::automation::AutomationStatus::Completed
            } else if model.is_some() {
                cron::automation::AutomationStatus::Active
            } else {
                cron::automation::AutomationStatus::Paused
            },
            status_reason: model
                .is_none()
                .then(|| "Choose a model to enable this migrated task".into()),
            model: model.clone().unwrap_or_default(),
            reasoning: reasoning.clone(),
            run_mode: cron::automation::RunMode::NewSession,
            target_session_id: None,
            owned_session_id: None,
            notification_policy: cron::automation::NotificationPolicy::All,
            runs: Vec::new(),
        });
        // Rebase the fire clock, exactly as the `resume` and `update` arms of
        // `manage` do. A legacy recurring task whose last fire is weeks old
        // becomes Active here; leaving `lastFiredAt` untouched makes the very
        // next tick see an overdue occurrence and run it unattended.
        if task.automation.as_ref().is_some_and(|automation| {
            automation.status == cron::automation::AutomationStatus::Active
        }) {
            task.last_fired_at = Some(now);
        }
    }
    if changed {
        cron::tasks_file::write_automation_tasks_body(fs, cwd, &cron::serialize_tasks(&doc))
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Apply a management operation under the same locks as scheduler firing.
/// Updates retain IDs and firing timestamps; all filesystem operations reject symlinks.
pub async fn manage(
    fs: &dyn FileSystem,
    cwd: &Path,
    request: CronRequestDto,
    registry: &Arc<dyn TaskRegistryHandle>,
    session_id: &str,
) -> Result<Vec<CronJobDto>, String> {
    if !tool_cron::cron_tools_enabled() {
        return Err("Scheduled tasks are disabled by CLAUDE_CODE_DISABLE_CRON".into());
    }
    if !matches!(
        request.action.as_str(),
        "list"
            | "history"
            | "create"
            | "update"
            | "delete"
            | "pause"
            | "resume"
            | "complete"
            | "prune_history"
    ) {
        return Err("Unknown scheduled task operation".into());
    }
    if request.durable == Some(false) {
        return Err("Scheduled task management supports durable workspace tasks only".into());
    }
    if !matches!(request.action.as_str(), "list" | "history") {
        cron::session_jobs(registry)
            .await
            .map_err(|e| format!("Scheduler unavailable: {e}"))?;
    }
    let _process_guard = cron::lock_cron_file().await;
    let _file_guard = if matches!(request.action.as_str(), "list" | "history") {
        None
    } else {
        Some(
            cron::tasks_file::lock_automation_tasks(fs, cwd)
                .await
                .map_err(|e| e.to_string())?,
        )
    };
    // Per-entry tolerance, same as the scheduler's authoritative reads: a single
    // malformed record must not make list / create / update / DELETE fail for
    // every other task while the scheduler keeps firing them. `parse_tasks_strict`
    // still rejects a body that is not `{ "tasks": [...] }`, and it carries the
    // entries it skips so this read-modify-write does not erase them.
    let mut doc: ScheduledTasks = match cron::tasks_file::read_automation_tasks_body(fs, cwd).await
    {
        Ok(body) => cron::tasks_file::parse_automation_tasks_strict(&body)
            .map_err(|e| format!("Cannot read scheduled tasks: {e}"))?,
        Err(FsError::NotFound(_)) => ScheduledTasks::default(),
        Err(e) => return Err(e.to_string()),
    };
    if !matches!(request.action.as_str(), "list" | "history") {
        let before = doc.clone();
        let deleted = if request.action == "delete" {
            request.id.clone()
        } else {
            None
        };
        let updated_id = if matches!(
            request.action.as_str(),
            "update" | "pause" | "resume" | "complete"
        ) {
            request.id.clone()
        } else {
            None
        };
        let creating = request.action == "create";
        let pruning = request.action == "prune_history";
        apply(&mut doc, request)?;
        if creating {
            doc.tasks.last_mut().unwrap().session_id = Some(session_id.to_string());
        }
        cron::tasks_file::write_automation_tasks_body(fs, cwd, &cron::serialize_tasks(&doc))
            .await
            .map_err(|e| e.to_string())?;
        if pruning {
            return Ok(doc.tasks.into_iter().map(task_dto).collect());
        }
        let live_result = if let Some(id) = deleted {
            cron::unregister_live_job(registry, &id, None)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        } else {
            let task = updated_id
                .as_ref()
                .and_then(|id| doc.tasks.iter().find(|t| &t.id == id))
                .or_else(|| doc.tasks.last())
                .ok_or("Missing saved task")?;
            // Versioned tasks are claimed directly from disk by the host firer.
            if task.automation.is_some() {
                cron::unregister_live_job(registry, &task.id, None)
                    .await
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            } else {
                cron::register_live_job(
                    registry,
                    cron::SessionCronTask {
                        id: task.id.clone(),
                        cron: task.cron.clone(),
                        prompt: task.prompt.clone(),
                        created_at: std::time::UNIX_EPOCH
                            + std::time::Duration::from_millis(task.created_at),
                        last_fired_at: task
                            .last_fired_at
                            .map(|ms| std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms)),
                        recurring: task.recurring.unwrap_or(false),
                        owner: None,
                    },
                    true,
                )
                .await
                .map_err(|e| e.to_string())
            }
        };
        if let Err(error) = live_result {
            cron::tasks_file::write_automation_tasks_body(fs, cwd, &cron::serialize_tasks(&before))
                .await
                .map_err(|rollback| {
                    format!("Scheduler update failed: {error}; rollback failed: {rollback}")
                })?;
            return Err(format!("Scheduler update failed: {error}"));
        }
    }
    Ok(doc.tasks.into_iter().map(task_dto).collect())
}

pub fn task_dto(task: CronTask) -> CronJobDto {
    let next_run_at = if task
        .automation
        .as_ref()
        .is_none_or(|a| a.status == cron::automation::AutomationStatus::Active)
    {
        task.automation
            .as_ref()
            .and_then(|automation| {
                automation
                    .runs
                    .iter()
                    .find(|run| run.status == cron::automation::AutomationRunStatus::Queued)
                    .map(|run| run.scheduled_at)
            })
            .or_else(|| {
                cron::next_fire_epoch_ms_for_persisted_task(&task, std::time::SystemTime::now())
            })
            .filter(|next| task.expires_at.is_none_or(|expiry| *next < expiry))
    } else {
        None
    };
    CronJobDto {
        next_run_at,
        automation: task
            .automation
            .and_then(|value| serde_json::to_value(value).ok())
            .and_then(|value| serde_json::from_value(value).ok()),
        id: task.id,
        cron: task.cron,
        prompt: task.prompt,
        recurring: task.recurring.unwrap_or(false),
        durable: true,
        permanent: task.permanent.unwrap_or(false),
        created_at: task.created_at,
        last_fired_at: task.last_fired_at,
        expires_at: task.expires_at,
        session_id: task.session_id.map(|id| {
            protocol::SessionId::parse_prefixed(&id)
                .map_or(id, |session| session.as_uuid().to_string())
        }),
    }
}

fn apply(doc: &mut ScheduledTasks, request: CronRequestDto) -> Result<(), String> {
    let action = request.action.as_str();
    if action == "prune_history" {
        let id = request.id.as_deref().ok_or("A task ID is required")?;
        let removal_ids: std::collections::HashSet<&str> = request
            .automation
            .as_ref()
            .ok_or("History removal IDs are required")?
            .runs
            .iter()
            .map(|run| run.id.as_str())
            .collect();
        // The host supplies deletion candidates, never a replacement snapshot.
        // Re-evaluate terminal state under the scheduler locks so stale host
        // snapshots cannot delete a running occurrence or overwrite new history.
        if let Some(automation) = doc
            .tasks
            .iter_mut()
            .find(|task| task.id == id)
            .and_then(|task| task.automation.as_mut())
        {
            automation
                .runs
                .retain(|run| !run.status.is_terminal() || !removal_ids.contains(run.id.as_str()));
        }
        return Ok(());
    }
    let incoming_automation = request
        .automation
        .clone()
        .filter(|_| matches!(action, "create" | "update"))
        .map(|value| {
            let value = serde_json::to_value(value).map_err(|e| e.to_string())?;
            let automation: cron::automation::CronAutomation = serde_json::from_value(value)
                .map_err(|e| format!("Invalid automation settings: {e}"))?;
            if automation.version != 2
                || !automation
                    .model
                    .split_once('/')
                    .is_some_and(|(provider, model)| !provider.is_empty() && !model.is_empty())
            {
                return Err("A supported automation version and model are required".to_string());
            }
            if automation.run_mode == cron::automation::RunMode::SelectedSession
                && automation
                    .target_session_id
                    .as_deref()
                    .and_then(protocol::SessionId::parse_prefixed)
                    .is_none()
            {
                return Err("Choose a target session".to_string());
            }
            Ok(automation)
        })
        .transpose()?;
    if request.no_expiry == Some(true) && request.expires_at.is_some() {
        return Err("Choose either an expiration date or no expiry".into());
    }
    // Only the two arms that WRITE an expiry validate it. `pause`/`resume`/
    // `complete`/`delete` echo back the task DTO the host is holding, expiry
    // included, so validating here would refuse every management action on an
    // already-expired task — with an error about a field the user never edited.
    if matches!(action, "create" | "update")
        && request.expires_at.is_some_and(|ms| {
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms)
                <= std::time::SystemTime::now()
        })
    {
        return Err("Expiration must be in the future".into());
    }
    if action == "create" || action == "update" {
        let expression = request
            .cron
            .as_deref()
            .ok_or("A cron schedule is required")?;
        let schedule = cron::parse_cron(expression).map_err(|e| e.to_string())?;
        if schedule
            .next_match_after(std::time::SystemTime::now())
            .is_none()
        {
            return Err("Cron schedule does not match a calendar date in the next year".into());
        }
        if request
            .prompt
            .as_deref()
            .is_none_or(|prompt| prompt.trim().is_empty())
        {
            return Err("Task instructions are required".into());
        }
    }
    if action == "create" {
        if doc
            .tasks
            .iter()
            .filter(|t| {
                t.automation
                    .as_ref()
                    .is_none_or(|a| a.status != cron::automation::AutomationStatus::Completed)
            })
            .count()
            >= 50
        {
            return Err("Too many scheduled jobs (max 50). Cancel one first.".into());
        }
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_millis()
            .try_into()
            .map_err(|_| "Clock timestamp overflow")?;
        let automation = incoming_automation.map(|mut automation| {
            automation.runs.clear();
            automation.owned_session_id = None;
            automation.status_reason = None;
            automation
        });
        doc.tasks.push(CronTask {
            creator: Default::default(),
            automation,
            id: tool_cron::schedule_cron::generate_cron_task_id(),
            cron: request.cron.unwrap(),
            prompt: request.prompt.unwrap(),
            created_at,
            last_fired_at: None,
            // PARITY `nCe`: the `recurring` key is written only when true.
            recurring: request.recurring.unwrap_or(true).then_some(true),
            permanent: None,
            expires_at: request.expires_at,
            session_id: None,
        });
    } else {
        let id = request.id.as_deref().ok_or("A task ID is required")?;
        let index = doc
            .tasks
            .iter()
            .position(|task| task.id == id)
            .ok_or("Scheduled task no longer exists; refresh the list")?;
        if doc.tasks[index].permanent == Some(true) {
            return Err("System scheduled tasks cannot be edited or deleted".into());
        }
        if action == "delete" {
            doc.tasks.remove(index);
        } else {
            let task = &mut doc.tasks[index];
            if matches!(action, "pause" | "resume" | "complete") {
                let automation = task
                    .automation
                    .as_mut()
                    .ok_or("Upgrade task settings before changing its status")?;
                if action == "resume"
                    && (automation.status == cron::automation::AutomationStatus::Completed
                        || automation.model.is_empty())
                {
                    return Err("Save valid future settings before reactivating this task".into());
                }
                if action == "resume"
                    && task.expires_at.is_some_and(|expiry| {
                        expiry
                            <= std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis() as u64
                    })
                {
                    return Err("Choose a future expiration before resuming".into());
                }
                automation.status = match action {
                    "pause" => cron::automation::AutomationStatus::Paused,
                    "resume" => cron::automation::AutomationStatus::Active,
                    _ => cron::automation::AutomationStatus::Completed,
                };
                // A host lifecycle action may explain why a dependency became
                // unavailable. It must not replace settings from a stale snapshot.
                automation.status_reason = if action == "pause" {
                    request
                        .automation
                        .as_ref()
                        .and_then(|value| value.status_reason.clone())
                } else {
                    None
                };
                if action != "resume" {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    for run in &mut automation.runs {
                        if run.status == cron::automation::AutomationRunStatus::Queued {
                            run.status = cron::automation::AutomationRunStatus::Cancelled;
                            run.finished_at = Some(now);
                        }
                    }
                }
                if action == "resume" {
                    task.last_fired_at = Some(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64,
                    );
                }
                return Ok(());
            }
            if let Some(mut automation) = incoming_automation {
                if let Some(old) = &task.automation {
                    automation.runs = old.runs.clone();
                    automation.owned_session_id = old.owned_session_id.clone();
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    if old.status != cron::automation::AutomationStatus::Active
                        && automation.status == cron::automation::AutomationStatus::Active
                    {
                        let expiry = if request.no_expiry == Some(true) {
                            None
                        } else {
                            request.expires_at.or(task.expires_at)
                        };
                        if expiry.is_some_and(|expiry| expiry <= now) {
                            return Err("Choose a future expiration before resuming".into());
                        }
                        task.last_fired_at = Some(now);
                    }
                    if automation.status != cron::automation::AutomationStatus::Active {
                        for run in &mut automation.runs {
                            if run.status == cron::automation::AutomationRunStatus::Queued {
                                run.status = cron::automation::AutomationRunStatus::Cancelled;
                                run.finished_at = Some(now);
                            }
                        }
                    }
                } else {
                    automation.runs.clear();
                    automation.owned_session_id = None;
                }
                automation.status_reason = None;
                task.automation = Some(automation);
            }
            task.cron = request.cron.unwrap();
            task.prompt = request.prompt.unwrap();
            if request.no_expiry == Some(true) {
                task.expires_at = None;
            } else if request.expires_at.is_some() {
                task.expires_at = request.expires_at;
            }
            if let Some(recurring) = request.recurring {
                task.recurring = recurring.then_some(true);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(action: &str) -> CronRequestDto {
        CronRequestDto {
            automation: None,
            action: action.into(),
            id: None,
            cron: Some("0 9 * * 1".into()),
            prompt: Some("Weekly report".into()),
            recurring: Some(true),
            durable: Some(true),
            expires_at: None,
            no_expiry: None,
        }
    }
    fn automation() -> client_protocol::commands::CronAutomationDto {
        serde_json::from_value(serde_json::json!({"version":2,"status":"active","model":"openai/gpt-test","reasoning":{"type":"automatic"},"runMode":"new_session","notificationPolicy":"all"})).unwrap()
    }
    #[test]
    fn history_pruning_removes_only_named_terminal_runs_from_current_state() {
        let mut doc = ScheduledTasks::default();
        let mut create = request("create");
        create.automation = Some(automation());
        apply(&mut doc, create).unwrap();
        let task_id = doc.tasks[0].id.clone();
        let saved = doc.tasks[0].automation.as_mut().unwrap();
        saved.owned_session_id = Some("owned-session".into());
        for (id, status) in [
            ("old", "succeeded"),
            ("queued", "queued"),
            ("running", "running"),
            ("new", "failed"),
        ] {
            saved.runs.push(
                serde_json::from_value(serde_json::json!({
                    "id":id, "taskId":task_id, "scheduledAt":1, "startedAt":null, "finishedAt":null,
                    "status":status, "model":"openai/gpt-test", "reasoning":{"type":"automatic"},
                    "sessionId":null, "summary":null, "error":null
                }))
                .unwrap(),
            );
        }
        let before = saved.clone();
        let mut candidates = automation();
        for id in ["old", "queued", "running", "unknown"] {
            let mut run = serde_json::to_value(&before.runs[0]).unwrap();
            run["id"] = serde_json::json!(id);
            candidates.runs.push(serde_json::from_value(run).unwrap());
        }
        // Other submitted configuration is deliberately untrusted/stale.
        candidates.model = "different/model".into();
        let mut prune = request("prune_history");
        prune.id = Some(task_id);
        prune.automation = Some(candidates);
        apply(&mut doc, prune.clone()).unwrap();
        let mut expected = before;
        expected.runs.remove(0);
        assert_eq!(doc.tasks[0].automation.as_ref().unwrap(), &expected);
        apply(&mut doc, prune).unwrap();
        assert_eq!(doc.tasks[0].automation.as_ref().unwrap(), &expected);
    }

    #[test]
    fn host_pause_accepts_only_reason_without_replacing_execution_settings() {
        let mut doc = ScheduledTasks::default();
        let mut create = request("create");
        create.automation = Some(automation());
        apply(&mut doc, create).unwrap();
        let before = doc.tasks[0].clone();
        let mut pause = request("pause");
        pause.id = Some(before.id.clone());
        pause.cron = None;
        pause.prompt = None;
        let mut stale = automation();
        stale.model = String::new();
        stale.status_reason = Some("Target chat was archived".into());
        pause.automation = Some(stale);
        apply(&mut doc, pause).unwrap();
        let saved = doc.tasks[0].automation.as_ref().unwrap();
        assert_eq!(saved.status, cron::automation::AutomationStatus::Paused);
        assert_eq!(
            saved.status_reason.as_deref(),
            Some("Target chat was archived")
        );
        assert_eq!(saved.model, before.automation.unwrap().model);
        assert_eq!(doc.tasks[0].cron, before.cron);
        assert_eq!(doc.tasks[0].prompt, before.prompt);
        let mut resume = request("resume");
        resume.id = Some(before.id);
        apply(&mut doc, resume).unwrap();
        assert!(doc.tasks[0]
            .automation
            .as_ref()
            .unwrap()
            .status_reason
            .is_none());
    }

    #[test]
    fn lifecycle_and_settings_round_trip_preserve_history() {
        let mut doc = ScheduledTasks::default();
        let mut create = request("create");
        create.automation = Some(automation());
        apply(&mut doc, create).unwrap();
        let id = doc.tasks[0].id.clone();
        for (action, expected) in [
            ("pause", "paused"),
            ("resume", "active"),
            ("complete", "completed"),
        ] {
            let mut change = request(action);
            change.id = Some(id.clone());
            apply(&mut doc, change).unwrap();
            assert_eq!(
                serde_json::to_value(&doc.tasks[0].automation).unwrap()["status"],
                expected
            );
            assert_eq!(
                task_dto(doc.tasks[0].clone()).automation.unwrap().model,
                "openai/gpt-test"
            );
        }
        assert!(doc.tasks[0].last_fired_at.is_some());
    }
    #[test]
    fn one_shot_resume_and_completed_reactivation_preview_future_occurrences() {
        for completed in [false, true] {
            let mut doc = ScheduledTasks::default();
            let mut create = request("create");
            create.cron = Some("* * * * *".into());
            create.recurring = Some(false);
            create.automation = Some(automation());
            apply(&mut doc, create).unwrap();
            let task = &mut doc.tasks[0];
            task.created_at -= 120_000;
            let created = task.created_at;
            task.automation.as_mut().unwrap().status = if completed {
                cron::AutomationStatus::Completed
            } else {
                cron::AutomationStatus::Paused
            };
            let mut change = request(if completed { "update" } else { "resume" });
            change.id = Some(task.id.clone());
            change.cron = Some(task.cron.clone());
            change.recurring = Some(false);
            if completed {
                change.automation = Some(automation());
            }
            apply(&mut doc, change).unwrap();
            let task = &doc.tasks[0];
            assert_eq!(task.created_at, created);
            let anchor = task.last_fired_at.unwrap();
            assert!(task_dto(task.clone()).next_run_at.unwrap() > anchor);
        }
    }

    #[tokio::test]
    async fn migration_preserves_tool_created_loop_lifetime_and_ownership() {
        let temp = tempfile::tempdir().unwrap();
        let fs = platform_posix::PosixFileSystem::new(temp.path().into());
        let mut doc = ScheduledTasks::default();
        apply(&mut doc, request("create")).unwrap();
        doc.tasks[0].creator.created_by_session_id = Some("loop-owner".into());
        doc.tasks[0].creator.created_by_pid = Some(std::process::id());
        let original = doc.tasks[0].clone();
        cron::tasks_file::write_automation_tasks_body(
            &fs,
            temp.path(),
            &cron::serialize_tasks(&doc),
        )
        .await
        .unwrap();
        migrate_legacy(
            &fs,
            temp.path(),
            Some("provider/model".into()),
            serde_json::json!({"type":"automatic"}),
        )
        .await
        .unwrap();
        let restored = cron::tasks_file::parse_automation_tasks(
            &cron::tasks_file::read_automation_tasks_body(&fs, temp.path())
                .await
                .unwrap(),
        );
        assert_eq!(restored.tasks, vec![original]);
        assert!(restored.tasks[0].automation.is_none());
    }

    #[tokio::test]
    async fn migration_does_not_revive_old_loop_sentinels_as_task_center_jobs() {
        let temp = tempfile::tempdir().unwrap();
        let fs = platform_posix::PosixFileSystem::new(temp.path().into());
        let mut doc = ScheduledTasks::default();
        for sentinel in [
            cron::AUTONOMOUS_LOOP_SENTINEL,
            cron::AUTONOMOUS_LOOP_DYNAMIC_SENTINEL,
            cron::LOOP_FILE_SENTINEL,
            cron::LOOP_FILE_DYNAMIC_SENTINEL,
        ] {
            let mut create = request("create");
            create.prompt = Some(sentinel.into());
            apply(&mut doc, create).unwrap();
        }
        let old_loop_tasks = doc.tasks.clone();
        assert!(old_loop_tasks
            .iter()
            .all(|task| task.creator.created_by_session_id.is_none()
                && task.creator.created_by_pid.is_none()));
        apply(&mut doc, request("create")).unwrap();
        let ordinary_id = doc.tasks.last().unwrap().id.clone();
        cron::tasks_file::write_automation_tasks_body(
            &fs,
            temp.path(),
            &cron::serialize_tasks(&doc),
        )
        .await
        .unwrap();
        migrate_legacy(
            &fs,
            temp.path(),
            Some("provider/model".into()),
            serde_json::json!({"type":"automatic"}),
        )
        .await
        .unwrap();
        let restored = cron::tasks_file::parse_automation_tasks(
            &cron::tasks_file::read_automation_tasks_body(&fs, temp.path())
                .await
                .unwrap(),
        );
        assert_eq!(restored.tasks.len(), 5);
        for original in old_loop_tasks {
            assert_eq!(
                restored.tasks.iter().find(|task| task.id == original.id),
                Some(&original)
            );
        }
        let ordinary = restored
            .tasks
            .iter()
            .find(|task| task.id == ordinary_id)
            .unwrap();
        assert_eq!(
            ordinary.automation.as_ref().unwrap().model,
            "provider/model"
        );
        assert!(!cron::tasks_file::session_scheduled_tasks_path(temp.path()).exists());
    }

    #[tokio::test]
    async fn migration_is_idempotent_and_captures_defaults_once() {
        let temp = tempfile::tempdir().unwrap();
        let fs = platform_posix::PosixFileSystem::new(temp.path().into());
        let mut doc = ScheduledTasks::default();
        apply(&mut doc, request("create")).unwrap();
        cron::tasks_file::write_automation_tasks_body(
            &fs,
            temp.path(),
            &cron::serialize_tasks(&doc),
        )
        .await
        .unwrap();
        migrate_legacy(
            &fs,
            temp.path(),
            Some("openai/gpt-test".into()),
            serde_json::json!({"type":"automatic"}),
        )
        .await
        .unwrap();
        let first = cron::tasks_file::read_automation_tasks_body(&fs, temp.path())
            .await
            .unwrap();
        migrate_legacy(
            &fs,
            temp.path(),
            Some("different/model".into()),
            serde_json::json!({"type":"level","id":"high"}),
        )
        .await
        .unwrap();
        assert_eq!(
            first,
            cron::tasks_file::read_automation_tasks_body(&fs, temp.path())
                .await
                .unwrap()
        );
        let migrated = cron::tasks_file::parse_automation_tasks(&first);
        assert_eq!(
            migrated.tasks[0].automation.as_ref().unwrap().run_mode,
            cron::automation::RunMode::NewSession
        );
    }
    #[test]
    fn edits_preserve_identity_and_firing_history() {
        let mut doc = ScheduledTasks::default();
        apply(&mut doc, request("create")).unwrap();
        doc.tasks[0].last_fired_at = Some(123);
        let before = doc.tasks[0].clone();
        let mut edit = request("update");
        edit.id = Some(before.id.clone());
        edit.prompt = Some("Updated instructions".into());
        apply(&mut doc, edit).unwrap();
        assert_eq!(doc.tasks[0].id, before.id);
        assert_eq!(doc.tasks[0].created_at, before.created_at);
        assert_eq!(doc.tasks[0].last_fired_at, Some(123));
        assert_eq!(doc.tasks[0].prompt, "Updated instructions");
        let mut delete = request("delete");
        delete.id = Some(before.id);
        apply(&mut doc, delete).unwrap();
        assert!(doc.tasks.is_empty());
    }
    #[test]
    fn invalid_edits_do_not_modify_tasks() {
        let mut doc = ScheduledTasks::default();
        apply(&mut doc, request("create")).unwrap();
        let before = doc.clone();
        let mut edit = request("update");
        edit.id = Some(doc.tasks[0].id.clone());
        edit.cron = Some("invalid".into());
        assert!(apply(&mut doc, edit).is_err());
        assert_eq!(doc, before);
        let mut delete = request("delete");
        delete.id = Some(doc.tasks[0].id.clone());
        doc.tasks[0].permanent = Some(true);
        assert!(apply(&mut doc, delete).is_err());
        assert_eq!(doc.tasks.len(), 1);
    }
    async fn live_scheduler(
        root: &Path,
    ) -> (Arc<dyn TaskRegistryHandle>, Arc<cron::CronScheduler>) {
        let fs = Arc::new(platform_posix::PosixFileSystem::new(root.into()));
        let runtime = Arc::new(platform_posix::PosixRuntime::new());
        let registry = Arc::new(tasks::registry::TaskRegistry::new(
            runtime.clone(),
            fs.clone(),
            Arc::new(tasks::output_manager::TaskOutputManager::new(
                root.join("output"),
                fs.clone(),
            )),
        ));
        let scheduler = Arc::new(cron::CronScheduler::new(
            registry.clone(),
            fs,
            Arc::new(platform_posix::PosixClock),
            runtime,
            cron::scheduled_tasks_path(root),
        ));
        scheduler.clone().start().await.unwrap();
        (registry, scheduler)
    }

    #[tokio::test]
    async fn persisted_crud_and_corruption_preservation() {
        let temp = tempfile::tempdir().unwrap();
        let fs = platform_posix::PosixFileSystem::new(temp.path().into());
        let (registry, scheduler) = live_scheduler(temp.path()).await;
        let jobs = manage(
            &fs,
            temp.path(),
            request("create"),
            &registry,
            "session-test",
        )
        .await
        .unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].session_id.as_deref(), Some("session-test"));
        let id = jobs[0].id.clone();
        let legacy_owner = "11111111-2222-3333-4444-555555555555";
        let body = cron::tasks_file::read_automation_tasks_body(&fs, temp.path())
            .await
            .unwrap();
        let mut legacy_doc: ScheduledTasks = serde_json::from_str(&body).unwrap();
        legacy_doc.tasks[0].session_id = Some(format!("sess:{legacy_owner}"));
        cron::tasks_file::write_automation_tasks_body(
            &fs,
            temp.path(),
            &cron::serialize_tasks(&legacy_doc),
        )
        .await
        .unwrap();
        let listed = manage(&fs, temp.path(), request("list"), &registry, "session-test")
            .await
            .unwrap();
        assert_eq!(listed[0].session_id.as_deref(), Some(legacy_owner));
        let mut edit = request("update");
        edit.id = Some(id.clone());
        edit.prompt = Some("Changed".into());
        let jobs = manage(&fs, temp.path(), edit, &registry, "session-test")
            .await
            .unwrap();
        assert_eq!(jobs[0].id, id);
        assert_eq!(jobs[0].prompt, "Changed");
        let mut delete = request("delete");
        delete.id = Some(id);
        assert!(manage(&fs, temp.path(), delete, &registry, "session-test")
            .await
            .unwrap()
            .is_empty());
        let jobs = manage(
            &fs,
            temp.path(),
            request("create"),
            &registry,
            "session-test",
        )
        .await
        .unwrap();
        assert!(
            cron::unregister_live_job(&registry, &jobs[0].id, None)
                .await
                .unwrap(),
            "management must register jobs in the running scheduler"
        );
        let path = cron::scheduled_tasks_path(temp.path());
        std::fs::write(&path, "broken JSON").unwrap();
        assert!(manage(
            &fs,
            temp.path(),
            request("create"),
            &registry,
            "session-test"
        )
        .await
        .is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "broken JSON");
        scheduler.stop().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refuses_symlinked_task_storage() {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), temp.path().join(".lingxi")).unwrap();
        let fs = platform_posix::PosixFileSystem::new(temp.path().into());
        let (registry, scheduler) = live_scheduler(temp.path()).await;
        assert!(manage(
            &fs,
            temp.path(),
            request("create"),
            &registry,
            "session-test"
        )
        .await
        .is_err());
        assert!(!outside.path().join("scheduled_tasks.json").exists());
        scheduler.stop().await.unwrap();
    }
    #[tokio::test]
    async fn unavailable_scheduler_never_persists_a_task() {
        let temp = tempfile::tempdir().unwrap();
        let fs = platform_posix::PosixFileSystem::new(temp.path().into());
        let (registry, scheduler) = live_scheduler(temp.path()).await;
        scheduler.stop().await.unwrap();
        assert!(manage(
            &fs,
            temp.path(),
            request("create"),
            &registry,
            "session-test"
        )
        .await
        .is_err());
        assert!(!cron::scheduled_tasks_path(temp.path()).exists());
    }
    #[test]
    fn expiration_can_be_set_preserved_and_cleared() {
        let mut doc = ScheduledTasks::default();
        let mut create = request("create");
        let expiry = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64)
            + 86_400_000;
        create.expires_at = Some(expiry);
        apply(&mut doc, create).unwrap();
        let mut edit = request("update");
        edit.id = Some(doc.tasks[0].id.clone());
        apply(&mut doc, edit.clone()).unwrap();
        assert_eq!(doc.tasks[0].expires_at, Some(expiry));
        edit.no_expiry = Some(true);
        apply(&mut doc, edit).unwrap();
        assert_eq!(doc.tasks[0].expires_at, None);
    }
}
