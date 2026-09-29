//! Two-phase shell ownership transfer. Included as a child of registry so row
//! publication and removal use the same lock as notification claims.
use super::*;
use platform_api::process::{BackgroundExitSink, ProcessHandle, ShellProcessHandoff};
use platform_api::shell_handoff::ShellTaskHandoff;

struct AdoptedExitSink {
    status: Arc<dyn crate::handlers::TaskStatusSink>,
    activated: tokio::sync::watch::Receiver<bool>,
}
impl AdoptedExitSink {
    async fn wait_until_activated(&self) -> bool {
        let mut activated = self.activated.clone();
        while !*activated.borrow_and_update() {
            if activated.changed().await.is_err() {
                return false;
            }
        }
        true
    }
}
#[async_trait::async_trait]
impl BackgroundExitSink for AdoptedExitSink {
    async fn on_exit(&self, id: &str, code: Option<i32>) {
        if !self.wait_until_activated().await {
            return;
        }
        if let Some(code) = code {
            self.status.set_exit_code(id, code).await;
        }
        self.status
            .set_status(
                id,
                if code == Some(0) {
                    TaskStatus::Completed
                } else {
                    TaskStatus::Failed
                },
            )
            .await;
    }
    async fn on_stall(&self, id: &str, tail: &str) {
        if self.wait_until_activated().await {
            if let Some(registry) = self.status.task_registry() {
                registry.notify_bash_stall(id, tail).await;
            }
        }
    }
    async fn on_memory_pressure(&self, id: &str) -> bool {
        if !self.wait_until_activated().await {
            return false;
        }
        match self.status.task_registry() {
            Some(registry) => registry.claim_bash_memory_pressure_stop(id).await,
            None => false,
        }
    }
}
struct AdoptedKiller {
    process: Arc<dyn ProcessRunner>,
    handoff: ShellProcessHandoff,
}
#[async_trait::async_trait]
impl platform_api::task_registry::TaskKiller for AdoptedKiller {
    async fn kill(&self) {
        // Validation proves the live supervisor still holds this child. The
        // native runner routes kill through that supervisor, never a raw PID.
        if self.process.validate_shell(&self.handoff).await.is_ok() {
            let _ = self
                .process
                .kill(&ProcessHandle {
                    task_id: self.handoff.task_id.clone(),
                    pid: self.handoff.pid,
                })
                .await;
        }
    }
}

struct ShellExportFenceGuard {
    fences: Arc<std::sync::Mutex<HashSet<String>>>,
    ids: Vec<String>,
    revision: tokio::sync::watch::Sender<u64>,
    armed: bool,
}
impl Drop for ShellExportFenceGuard {
    fn drop(&mut self) {
        if self.armed {
            let mut fences = self.fences.lock().unwrap();
            for id in &self.ids {
                fences.remove(id);
            }
            self.revision
                .send_modify(|revision| *revision = revision.wrapping_add(1));
        }
    }
}

struct StagedAdoptionGuard {
    process: Arc<dyn ProcessRunner>,
    output: Arc<crate::output_manager::TaskOutputManager>,
    attached: Vec<ShellProcessHandoff>,
    outputs: Vec<(std::path::PathBuf, bool)>,
    gate: Option<tokio::sync::OwnedMutexGuard<()>>,
    armed: bool,
}
impl Drop for StagedAdoptionGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let process = self.process.clone();
        let output = self.output.clone();
        let attached = std::mem::take(&mut self.attached);
        let outputs = std::mem::take(&mut self.outputs);
        let gate = self.gate.take();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                // Retain exclusivity until old observers and temporary links
                // are gone. A cancelled attempt cannot later cancel its retry.
                // Moves the guard into this scope so it drops at the END of it. The lint sees
                // a `_`-binding with no side effect; the side effect is the Drop deadline.
                #[allow(clippy::no_effect_underscore_binding)]
                let _gate = gate;
                for native in attached {
                    let _ = process.release_shell(&native).await;
                }
                for (path, _) in outputs {
                    // A cancelled install may have completed its final link.
                    // Preserve it for authenticated retry; never unlink a leaf
                    // whose creation result the cancelled future did not see.
                    output.release_output_state(&path).await;
                }
            });
        }
    }
}

impl TaskRegistry {
    fn shell_runtime(
        &self,
    ) -> Result<
        (
            Arc<dyn ProcessRunner>,
            Arc<dyn crate::handlers::TaskStatusSink>,
        ),
        TaskError,
    > {
        self.shell_adoption_runtime
            .clone()
            .ok_or(TaskError::Unsupported)
    }

    pub(crate) async fn output_completion_status(&self, id: &str) -> Result<TaskStatus, TaskError> {
        let id = self.canonical_or_raw(id).await;
        let tasks = self.tasks.read().await;
        let state = tasks
            .get(&id)
            .ok_or_else(|| TaskError::NotFound(id.clone()))?;
        Ok(
            if self.shell_transfer_fences.lock().unwrap().contains(&id) {
                TaskStatus::Running
            } else {
                state.base().status
            },
        )
    }

    fn release_shell_transfer_fences(&self, ids: &[String]) {
        let mut fences = self.shell_transfer_fences.lock().unwrap();
        for id in ids {
            fences.remove(id);
        }
        drop(fences);
        self.bump_notification_revision();
    }

    pub async fn export_shell_handoff(&self) -> Result<Vec<ShellTaskHandoff>, TaskError> {
        let shells: Vec<_> = {
            let tasks = self.tasks.write().await;
            let shells: Vec<_> = tasks
                .values()
                .filter_map(|state| match state {
                    // iF transfers complete owner trees. Agent trees without a
                    // checkpoint contract stay under the source's stop/wait policy.
                    TaskState::LocalBash(shell)
                        if shell.base.status == TaskStatus::Running
                            && shell.is_backgrounded == Some(true)
                            && shell.base.creator_agent_id.is_none() =>
                    {
                        Some(shell.clone())
                    }
                    _ => None,
                })
                .collect();
            let mut fences = self.shell_transfer_fences.lock().unwrap();
            if shells.iter().any(|shell| fences.contains(&shell.base.id)) {
                return Err(TaskError::Internal("shell transfer already pending".into()));
            }
            fences.extend(shells.iter().map(|shell| shell.base.id.clone()));
            shells
        };
        if shells.is_empty() {
            return Ok(Vec::new());
        }
        let mut guard = ShellExportFenceGuard {
            fences: self.shell_transfer_fences.clone(),
            ids: shells.iter().map(|shell| shell.base.id.clone()).collect(),
            revision: self.notification_revision.clone(),
            armed: true,
        };
        let (process, _) = self.shell_runtime()?;
        let mut exported = Vec::new();
        for shell in shells {
            let pid = shell.pid.ok_or_else(|| {
                TaskError::Internal(format!(
                    "running shell {} has no supervised process identity",
                    shell.base.id
                ))
            })?;
            let native = match process
                .export_shell(&ProcessHandle {
                    task_id: shell.base.id.clone(),
                    pid,
                })
                .await
            {
                Ok(native) => native,
                Err(mobile_linux_api::ProcessError::Unsupported) => {
                    self.release_shell_transfer_fences(&[shell.base.id]);
                    continue;
                }
                Err(error) => return Err(TaskError::Io(error.to_string())),
            };
            exported.push(ShellTaskHandoff {
                task_id: shell.base.id,
                command: shell.command,
                description: shell.base.description,
                tool_use_id: shell.base.tool_use_id,
                creator_agent_id: shell.base.creator_agent_id,
                cwd: shell.cwd,
                caller: shell.caller,
                output_offset: shell.base.output_offset,
                process: native,
            });
        }
        guard.armed = false;
        Ok(exported)
    }

    pub async fn prepare_shell_handoff(
        &self,
        records: &[ShellTaskHandoff],
    ) -> Result<(), TaskError> {
        if records.is_empty() {
            return Ok(());
        }
        let (process, _) = self.shell_runtime()?;
        let mut seen = HashSet::new();
        for record in records {
            if record.task_id != record.process.task_id || !seen.insert(&record.task_id) {
                return Err(TaskError::Internal(
                    "duplicate or mismatched shell handoff identity".into(),
                ));
            }
            self.output_manager
                .path_for(&record.task_id)
                .map_err(|e| TaskError::Io(e.to_string()))?;
            if !std::path::Path::new(&record.process.output_path).is_absolute() {
                return Err(TaskError::Internal(
                    "shell handoff output must be absolute".into(),
                ));
            }
            {
                let tasks = self.tasks.read().await;
                if tasks.contains_key(&record.task_id) {
                    return Err(TaskError::Internal(
                        "shell handoff task already registered".into(),
                    ));
                }
                if let Some(owner) = record.creator_agent_id {
                    if !tasks.values().any(|state| matches!(state,
                        TaskState::LocalAgent(agent) if agent.agent_id == owner && !state.is_terminated()) || matches!(state,
                        TaskState::InProcessTeammate(agent) if agent.agent_id == owner && !state.is_terminated())) {
                        return Err(TaskError::Internal(format!("shell owner {owner} was not restored")));
                    }
                }
            }
            process
                .validate_shell(&record.process)
                .await
                .map_err(|e| TaskError::Io(e.to_string()))?;
        }
        Ok(())
    }

    /// Remove only rows whose source completion has not won the race. The
    /// caller persists this accepted subset before the destination activates.
    pub async fn commit_shell_handoff(&self, ids: &[String]) -> Result<Vec<String>, TaskError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let _mutation = self.shell_transfer_mutation.lock().await;
        let (process, _) = self.shell_runtime()?;
        let mut accepted = Vec::new();
        for id in ids {
            let row = self.tasks.read().await.get(id).cloned();
            let Some(TaskState::LocalBash(shell)) = row else {
                continue;
            };
            if shell.base.status != TaskStatus::Running
                || self.stopping_shells.lock().unwrap().contains_key(id)
            {
                continue;
            }
            let Some(pid) = shell.pid else { continue };
            let Ok(native) = process
                .export_shell(&ProcessHandle {
                    task_id: id.clone(),
                    pid,
                })
                .await
            else {
                continue;
            };
            if process.release_shell(&native).await.is_err() {
                continue;
            }
            let removed = {
                let mut tasks = self.tasks.write().await;
                if tasks
                    .get(id)
                    .is_some_and(|state| state.base().status == TaskStatus::Running)
                    && !self.stopping_shells.lock().unwrap().contains_key(id)
                {
                    let mut routes = self.spawned.write().await;
                    let mut cleanups = self.cleanups.lock().await;
                    let mut aliases = self.aliases.write().await;
                    routes.remove(id);
                    cleanups.remove(id);
                    aliases.retain(|_, target| target != id);
                    tasks.remove(id)
                } else {
                    None
                }
            };
            if let Some(row) = removed {
                self.backgrounders.lock().await.remove(id);
                self.pending_monitor_events
                    .lock()
                    .unwrap()
                    .retain(|notice| notice.task_id != *id);
                self.output_manager
                    .release_output_state(&row.base().output_file)
                    .await;
                accepted.push(id.clone());
            }
        }
        // Rejected terminal rows stay fenced until the host persists the
        // accepted decision, then explicitly rolls back that rejected subset.
        self.release_shell_transfer_fences(&accepted);
        Ok(accepted)
    }

    pub async fn adopt_shell_handoff(&self, records: &[ShellTaskHandoff]) -> Result<(), TaskError> {
        if records.is_empty() {
            return Ok(());
        }
        let gate = self.shell_adoption_gate.clone().lock_owned().await;
        self.prepare_shell_handoff(records).await?;
        let (process, status) = self.shell_runtime()?;
        let mut cleanup = StagedAdoptionGuard {
            process: process.clone(),
            output: self.output_manager.clone(),
            attached: Vec::new(),
            outputs: Vec::new(),
            gate: Some(gate),
            armed: true,
        };
        let (activate, activated) = tokio::sync::watch::channel(false);
        let sink: Arc<dyn BackgroundExitSink> = Arc::new(AdoptedExitSink { status, activated });
        let mut attached = Vec::new();
        let mut staged = Vec::new();
        let result: Result<(), TaskError> = async {
            for record in records {
                let output = self.output_manager.path_for(&record.task_id).map_err(|e| TaskError::Io(e.to_string()))?;
                let allocated = output != std::path::Path::new(&record.process.output_path);
                // Install a final link directly: no empty spool can be left
                // behind by a crash between allocation and linking.
                cleanup.outputs.push((output.clone(), allocated));
                if allocated { self.output_manager.adopt_output(&output, std::path::Path::new(&record.process.output_path)).await.map_err(|error| TaskError::Io(error.to_string()))?; }
                self.mark_shell_supervised(&record.task_id).await;
                let state = TaskState::LocalBash(crate::state::LocalBashTaskState {
                    is_adopted: true, caller: record.caller.clone(), command: record.command.clone(),
                    pid: Some(record.process.pid), exit_code: None, cwd: record.cwd.clone(), is_backgrounded: Some(true),
                    base: TaskStateBase {
                        id: record.task_id.clone(), task_type: TaskType::LocalBash, status: TaskStatus::Running,
                        description: record.description.clone(), tool_use_id: record.tool_use_id.clone(),
                        start_time: SystemTime::now(), end_time: None, total_paused_ms: 0, output_file: output.clone(),
                        evict_after: None, output_offset: record.output_offset, notified: false,
                        creator_teammate_name: None, creator_team_name: None, creator_agent_id: record.creator_agent_id,
                    },
                });
                staged.push((record.clone(), state, output, allocated));
                cleanup.attached.push(record.process.clone());
                process.adopt_shell(&record.process, sink.clone()).await.map_err(|e| TaskError::Io(e.to_string()))?;
                attached.push(record.process.clone());
            }
            // Install teardown handles before publishing any row. The handles
            // alone are unaddressable; no UI/SDK/model task exists until the
            // single row-lock commit below.
            let mut tasks = self.tasks.write().await;
            let mut cleanups = self.cleanups.lock().await;
            for (record, _, _, _) in &staged {
                if tasks.contains_key(&record.task_id) { return Err(TaskError::Internal("shell task appeared during adoption".into())); }
                if let Some(owner) = record.creator_agent_id {
                    if !tasks.values().any(|state| matches!(state,
                        TaskState::LocalAgent(agent) if agent.agent_id == owner && !state.is_terminated()) || matches!(state,
                        TaskState::InProcessTeammate(agent) if agent.agent_id == owner && !state.is_terminated())) {
                        return Err(TaskError::Internal("shell owner stopped during adoption".into()));
                    }
                }
            }
            for (record, state, _, _) in &staged {
                let killer = Arc::new(AdoptedKiller { process: process.clone(), handoff: record.process.clone() });
                let cleanup: TaskCleanup = Arc::new(move || {
                    let killer = killer.clone();
                    tokio::spawn(async move { platform_api::task_registry::TaskKiller::kill(killer.as_ref()).await; });
                });
                cleanups.insert(record.task_id.clone(), cleanup);
                tasks.insert(record.task_id.clone(), state.clone());
            }
            Ok(())
        }.await;
        if let Err(error) = result {
            for native in &attached {
                let _ = process.release_shell(native).await;
            }
            // The batch never published rows. Dropping its gate discards an
            // already-arrived durable receipt without consuming it upstream.
            drop(activate);
            for (_, _, output, allocated) in staged {
                if allocated {
                    let _ = self.output_manager.discard(&output).await;
                } else {
                    self.output_manager.release_output_state(&output).await;
                }
            }
            cleanup.armed = false;
            return Err(error);
        }
        cleanup.armed = false;
        activate.send_replace(true);
        for record in records {
            self.fire_task_created(&record.task_id, TaskType::LocalBash, &record.description)
                .await;
        }
        self.bump_notification_revision();
        Ok(())
    }

    pub async fn rollback_shell_handoff(
        &self,
        records: &[ShellTaskHandoff],
    ) -> Result<(), TaskError> {
        if records.is_empty() {
            return Ok(());
        }
        let _mutation = self.shell_transfer_mutation.lock().await;
        let (missing, running): (Vec<_>, Vec<_>) = {
            let tasks = self.tasks.read().await;
            (
                records
                    .iter()
                    .filter(|record| !tasks.contains_key(&record.task_id))
                    .cloned()
                    .collect(),
                records
                    .iter()
                    .filter(|record| {
                        tasks
                            .get(&record.task_id)
                            .is_some_and(|state| state.base().status == TaskStatus::Running)
                    })
                    .cloned()
                    .collect(),
            )
        };
        if !running.is_empty() {
            let (process, status) = self.shell_runtime()?;
            let (_, active) = tokio::sync::watch::channel(true);
            let sink: Arc<dyn BackgroundExitSink> = Arc::new(AdoptedExitSink {
                status,
                activated: active,
            });
            for record in &running {
                // Cancellation can occur after native release but before the
                // row-removal commit. Re-ensure observation even when the row
                // survived. Native adopt joins any prior observer before its
                // replacement, so rollback cannot create duplicate callbacks.
                process
                    .adopt_shell(&record.process, sink.clone())
                    .await
                    .map_err(|error| TaskError::Io(error.to_string()))?;
            }
        }
        self.adopt_shell_handoff(&missing).await?;
        self.release_shell_transfer_fences(
            &records
                .iter()
                .map(|record| record.task_id.clone())
                .collect::<Vec<_>>(),
        );
        Ok(())
    }
}
