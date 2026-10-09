//! Workflow-on-mobile composition pieces (plan v3 Phase 1).
//!
//! Everything the `Workflow` tool needs to run on the mobile engine — a real
//! `TaskRegistry`, a `PoolSubagentSpawner`, and the launcher that spawns
//! `LocalWorkflow` background tasks — adapted from the desktop composition
//! root (`harness-runtime::desktop/src/lib.rs`; the launcher mirrors
//! `TaskRegistryWorkflowLauncher`, the invoker mirrors `DeferredToolInvoker`).
//! Deliberately omitted desktop seams: worktree isolation (an
//! `isolation:"worktree"` agent degrades to a plain spawn — the documented
//! fallback), LSP, and the coordinator. Subagents keep upstream interactivity
//! semantics: a one-shot spawn is `is_async=false`, so `AskUserQuestion`
//! inside a workflow agent rides the SAME shared `Arc<ToolRegistry>` (and
//! therefore the same `TuiBridgeResolver` → broker → client channel) the
//! main session uses.

use std::sync::Arc;

#[derive(Debug, Clone)]
struct WorkflowCheckpoint {
    task_id: String,
    workflow_run_id: String,
    workflow_id: String,
    script_path: String,
    script_sha256: Option<String>,
    args_json: Option<String>,
    description: String,
    start_time: Option<u64>,
    transcript_dir: String,
}

/// Events emitted by a workflow worker before its launcher has finished
/// persisting the task/session ownership checkpoint.  The task registry starts
/// the handler before returning the generated task id, so this short handoff
/// window is real even though the worker waits for registry registration.
#[derive(Debug, Clone)]
enum PendingWorkflowEvent {
    Status {
        status: tasks::TaskStatus,
        /// Terminal failure reason, when `finish_workflow_terminal` supplied
        /// one. Buffered alongside the status so a transition that lands
        /// before the ownership checkpoint does not lose its reason.
        error: Option<String>,
    },
    Progress {
        run_id: String,
        progress: tasks::handlers::local_workflow::WorkflowProgressUpdate,
    },
}

#[derive(Debug, Clone)]
struct WorkflowAdoptFile {
    written_at_ms: u64,
    origin: String,
    shells: Vec<serde_json::Value>,
    cron: Vec<serde_json::Value>,
    workflows: Vec<WorkflowCheckpoint>,
    agents: Vec<serde_json::Value>,
}

/// Session-scoped workflow handoff store. Mobile cannot rely on a graceful
/// process-exit hook, so it keeps Claude Code's `adopt.json` payload current
/// from launch until a terminal status. A later process adopts it as `Paused`.
pub(crate) struct MobileWorkflowCheckpointStore {
    lingxi_home: std::path::PathBuf,
    cwd: std::path::PathBuf,
    lock: std::sync::Mutex<()>,
    task_runs: std::sync::Mutex<std::collections::HashMap<String, (String, String)>>,
    pending_events: std::sync::Mutex<std::collections::HashMap<String, Vec<PendingWorkflowEvent>>>,
}

impl MobileWorkflowCheckpointStore {
    pub(crate) fn new(lingxi_home: std::path::PathBuf, cwd: std::path::PathBuf) -> Self {
        Self {
            lingxi_home,
            cwd,
            lock: std::sync::Mutex::new(()),
            task_runs: std::sync::Mutex::new(std::collections::HashMap::new()),
            pending_events: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn session_dir(&self, session_uuid: &str) -> std::path::PathBuf {
        orchestrator::transcript_paths::subagents_dir(
            &self.lingxi_home,
            &self.cwd.to_string_lossy(),
            session_uuid,
        )
        .parent()
        .expect("subagents directory has a session parent")
        .to_path_buf()
    }

    fn path(&self, session_uuid: &str) -> std::path::PathBuf {
        self.session_dir(session_uuid).join("adopt.json")
    }

    fn track_task_owner(
        &self,
        task_id: &str,
        session_uuid: &str,
        run_id: &str,
    ) -> Vec<PendingWorkflowEvent> {
        let mut task_runs = self
            .task_runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        task_runs.retain(|_, (existing_session, existing_run)| {
            existing_session != session_uuid || existing_run != run_id
        });
        task_runs.insert(
            task_id.to_string(),
            (session_uuid.to_string(), run_id.to_string()),
        );
        self.pending_events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(task_id)
            .unwrap_or_default()
    }

    fn buffer_event(&self, task_id: &str, event: PendingWorkflowEvent) {
        self.pending_events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(task_id.to_string())
            .or_default()
            .push(event);
    }

    fn task_owner(&self, task_id: &str) -> Option<(String, String)> {
        self.task_runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(task_id)
            .cloned()
    }

    fn read(&self, session_uuid: &str) -> WorkflowAdoptFile {
        let empty = || WorkflowAdoptFile {
            written_at_ms: unix_time_ms(),
            origin: "exit".to_string(),
            shells: Vec::new(),
            cron: Vec::new(),
            workflows: Vec::new(),
            agents: Vec::new(),
        };
        let Some(value) = std::fs::read_to_string(self.path(session_uuid))
            .ok()
            .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
        else {
            return empty();
        };
        let workflows = value
            .get("workflows")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|checkpoint| {
                Some(WorkflowCheckpoint {
                    task_id: checkpoint.get("taskId")?.as_str()?.to_string(),
                    workflow_run_id: checkpoint.get("workflowRunId")?.as_str()?.to_string(),
                    script_path: checkpoint.get("scriptPath")?.as_str()?.to_string(),
                    script_sha256: checkpoint
                        .get("scriptSha256")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                    args_json: checkpoint
                        .get("argsJson")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                    workflow_id: checkpoint
                        .get("workflowId")
                        .and_then(serde_json::Value::as_str)
                        // Checkpoints written before workflow identity was
                        // split used description for both fields.
                        .or_else(|| {
                            checkpoint
                                .get("description")
                                .and_then(serde_json::Value::as_str)
                        })?
                        .to_string(),
                    description: checkpoint.get("description")?.as_str()?.to_string(),
                    start_time: checkpoint
                        .get("startTime")
                        .and_then(serde_json::Value::as_u64),
                    transcript_dir: checkpoint.get("transcriptDir")?.as_str()?.to_string(),
                })
            })
            .collect();
        WorkflowAdoptFile {
            written_at_ms: value
                .get("writtenAtMs")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_else(unix_time_ms),
            origin: value
                .get("origin")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("exit")
                .to_string(),
            shells: value
                .get("shells")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default(),
            cron: value
                .get("cron")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default(),
            workflows,
            agents: value
                .get("agents")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default(),
        }
    }

    fn write(&self, session_uuid: &str, mut file: WorkflowAdoptFile) -> std::io::Result<()> {
        let path = self.path(session_uuid);
        if file.workflows.is_empty()
            && file.shells.is_empty()
            && file.cron.is_empty()
            && file.agents.is_empty()
        {
            return match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            };
        }
        file.written_at_ms = unix_time_ms();
        file.origin = "exit".to_string();
        let parent = path.parent().expect("adopt file has parent");
        std::fs::create_dir_all(parent)?;
        let temp = parent.join("adopt.json.tmp");
        let workflows = file
            .workflows
            .into_iter()
            .map(|checkpoint| {
                let mut value = serde_json::Map::new();
                value.insert("taskId".into(), checkpoint.task_id.into());
                value.insert("workflowRunId".into(), checkpoint.workflow_run_id.into());
                value.insert("workflowId".into(), checkpoint.workflow_id.into());
                value.insert("scriptPath".into(), checkpoint.script_path.into());
                if let Some(hash) = checkpoint.script_sha256 {
                    value.insert("scriptSha256".into(), hash.into());
                }
                if let Some(args) = checkpoint.args_json {
                    value.insert("argsJson".into(), args.into());
                }
                value.insert("description".into(), checkpoint.description.into());
                if let Some(start_time) = checkpoint.start_time {
                    value.insert("startTime".into(), start_time.into());
                }
                value.insert("transcriptDir".into(), checkpoint.transcript_dir.into());
                serde_json::Value::Object(value)
            })
            .collect::<Vec<_>>();
        let json = serde_json::to_vec_pretty(&serde_json::json!({
            "writtenAtMs": file.written_at_ms,
            "origin": file.origin,
            "shells": file.shells,
            "cron": file.cron,
            "workflows": workflows,
            "agents": file.agents,
        }))
        .map_err(std::io::Error::other)?;
        std::fs::write(&temp, json)?;
        std::fs::rename(temp, path)
    }

    fn upsert(
        &self,
        session_uuid: &str,
        checkpoint: WorkflowCheckpoint,
    ) -> std::io::Result<Vec<PendingWorkflowEvent>> {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut file = self.read(session_uuid);
        file.workflows
            .retain(|existing| existing.workflow_run_id != checkpoint.workflow_run_id);
        let task_id = checkpoint.task_id.clone();
        let workflow_run_id = checkpoint.workflow_run_id.clone();
        file.workflows.push(checkpoint);
        match self.write(session_uuid, file) {
            Ok(()) => Ok(self.track_task_owner(&task_id, session_uuid, &workflow_run_id)),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn remove_task(&self, task_id: &str) {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some((session_uuid, run_id)) = self
            .task_runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(task_id)
            .cloned()
        else {
            return;
        };
        let mut file = self.read(&session_uuid);
        file.workflows
            .retain(|checkpoint| checkpoint.workflow_run_id != run_id);
        match self.write(&session_uuid, file) {
            Ok(()) => {
                // Remove the in-memory owner only after the durable file has
                // been updated. If the write failed, a later terminal event
                // or shutdown retry can still find the checkpoint and clean
                // it up instead of silently orphaning it for adoption.
                self.task_runs
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(task_id);
            }
            Err(error) => {
                tracing::warn!(%error, task_id, "could not remove workflow checkpoint; retaining owner for retry");
            }
        }
    }

    pub(crate) async fn adopt_session(
        &self,
        session_uuid: &str,
        registry: &tasks::registry::TaskRegistry,
    ) {
        self.adopt_matching(session_uuid, None, registry).await;
    }

    /// Recover a task whose live row was evicted (terminal task GC) while its
    /// validated recovery checkpoint remains durable, without reviving
    /// unrelated terminal rows.
    pub(crate) async fn adopt_task(
        &self,
        session_uuid: &str,
        task_id: &str,
        registry: &tasks::registry::TaskRegistry,
    ) {
        // A filtered-out or terminal row is still owned by the live registry.
        // Never overwrite it while recovering a missing current-session row.
        if registry.get(task_id).await.is_some() {
            return;
        }
        self.adopt_matching(session_uuid, Some(task_id), registry)
            .await;
    }

    async fn adopt_matching(
        &self,
        session_uuid: &str,
        task_id: Option<&str>,
        registry: &tasks::registry::TaskRegistry,
    ) {
        let checkpoints = {
            let _guard = self
                .lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.read(session_uuid).workflows
        };
        let expected_root = self
            .session_dir(session_uuid)
            .join("subagents")
            .join("workflows");
        for checkpoint in checkpoints {
            if task_id.is_some_and(|id| checkpoint.task_id != id) {
                continue;
            }
            let valid_task_id = checkpoint.task_id.len() == 9
                && checkpoint.task_id.starts_with('w')
                && checkpoint
                    .task_id
                    .bytes()
                    .skip(1)
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
            let valid_run_id = tool_workflow::is_valid_run_id(&checkpoint.workflow_run_id);
            let script = std::fs::read(&checkpoint.script_path).ok();
            let valid_hash = match (&script, checkpoint.script_sha256.as_deref()) {
                (Some(script), Some(expected)) => sha256_hex(script) == expected,
                (Some(_), None) => true,
                _ => false,
            };
            let transcript_dir = std::path::PathBuf::from(&checkpoint.transcript_dir);
            let valid_transcript = std::fs::canonicalize(&expected_root)
                .ok()
                .zip(std::fs::canonicalize(&transcript_dir).ok())
                .is_some_and(|(root, directory)| directory.starts_with(root))
                && transcript_dir.join("journal.jsonl").is_file();
            let valid_args = checkpoint
                .args_json
                .as_deref()
                .is_none_or(|json| serde_json::from_str::<serde_json::Value>(json).is_ok());
            if !(valid_task_id && valid_run_id && valid_hash && valid_transcript && valid_args) {
                tracing::warn!(
                    task_id = %checkpoint.task_id,
                    run_id = %checkpoint.workflow_run_id,
                    "ignored invalid workflow checkpoint"
                );
                continue;
            }
            let start_time = checkpoint
                .start_time
                .map(|millis| std::time::UNIX_EPOCH + std::time::Duration::from_millis(millis))
                .unwrap_or_else(std::time::SystemTime::now);
            let adopted = tasks::registry::AdoptedWorkflow {
                task_id: checkpoint.task_id.clone(),
                session_uuid: Some(session_uuid.to_string()),
                workflow_id: checkpoint.workflow_id.clone(),
                run_id: checkpoint.workflow_run_id.clone(),
                script_path: checkpoint.script_path.clone(),
                args: checkpoint.args_json.clone(),
                transcript_dir: checkpoint.transcript_dir.clone(),
                description: checkpoint.description.clone(),
                start_time,
            };
            if let Err(error) = registry.register_adopted_workflow(adopted).await {
                tracing::warn!(%error, "could not register adopted workflow");
                continue;
            }
            self.task_runs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(
                    checkpoint.task_id,
                    (session_uuid.to_string(), checkpoint.workflow_run_id),
                );
        }
    }
}

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

/// Mobile workflow status adapter: persist every worker transition in the task
/// registry, then publish the same transition to the native conversation UI.
///
/// The registry sink remains the source of truth. This wrapper only closes the
/// previously missing engine-to-client leg; without it a completed workflow was
/// visible to `TaskList` but never produced the existing `TaskStatusChanged`
/// event unless the user stopped it manually.
pub(crate) struct MobileWorkflowStatusSink {
    registry: Arc<tasks::registry_status_sink::RegistryStatusSink>,
    event_sink: Arc<dyn client::adapter::ClientEventSink>,
    listener: Arc<dyn client::adapter::ClientEventListener>,
    checkpoints: Arc<MobileWorkflowCheckpointStore>,
    active_session_uuid: Arc<std::sync::Mutex<String>>,
}

impl MobileWorkflowStatusSink {
    pub(crate) fn new(
        listener: Arc<dyn client::adapter::ClientEventListener>,
        checkpoints: Arc<MobileWorkflowCheckpointStore>,
        active_session_uuid: Arc<std::sync::Mutex<String>>,
    ) -> Self {
        Self {
            registry: Arc::new(tasks::registry_status_sink::RegistryStatusSink::new()),
            event_sink: client::adapter::ListenerSink::arc(listener.clone()),
            listener,
            checkpoints,
            active_session_uuid,
        }
    }

    pub(crate) fn bind(&self, registry: Arc<tasks::registry::TaskRegistry>) {
        self.registry.bind(registry);
    }

    fn is_active_origin(&self, origin_session_id: &str) -> bool {
        self.active_session_uuid
            .lock()
            .map(|active| active.as_str() == origin_session_id)
            .unwrap_or(false)
    }

    async fn emit_status_for_owner(
        &self,
        task_id: &str,
        status: tasks::TaskStatus,
        origin_session_id: String,
        error: Option<String>,
    ) {
        if !self.is_active_origin(&origin_session_id) {
            return;
        }
        self.event_sink
            .emit(client::protocol::events::ClientEvent::TaskStatusChanged {
                task_id: task_id.to_string(),
                status: client::adapter::lowering::lower_task_status(match status {
                    tasks::TaskStatus::Pending => "pending",
                    tasks::TaskStatus::Running => "running",
                    tasks::TaskStatus::Paused => "paused",
                    tasks::TaskStatus::Completed => "completed",
                    tasks::TaskStatus::Failed => "failed",
                    tasks::TaskStatus::Killed => "killed",
                }),
                origin_session_id: Some(origin_session_id),
                // Only a real failure carries a reason; a completed/killed
                // transition must never inherit a stale one.
                error: if matches!(status, tasks::TaskStatus::Failed) {
                    error
                } else {
                    None
                },
            })
            .await;
    }

    async fn emit_progress_for_owner(
        &self,
        task_id: &str,
        run_id: &str,
        origin_session_id: String,
        progress: tasks::handlers::local_workflow::WorkflowProgressUpdate,
    ) {
        if !self.is_active_origin(&origin_session_id) {
            return;
        }
        self.listener
            .on_workflow_progress(
                origin_session_id,
                task_id.to_string(),
                run_id.to_string(),
                client::protocol::listings::WorkflowProgressDto {
                    kind: progress.kind,
                    index: progress.index,
                    title: progress.title,
                    message: progress.message,
                    label: progress.label,
                    phase_index: progress.phase_index,
                    phase_title: progress.phase_title,
                    agent_id: progress.agent_id,
                    agent_type: progress.agent_type,
                    model: progress.model,
                    fallback_model: progress.fallback_model,
                    state: progress.state,
                    error: progress.error,
                    tool_use_id: progress.tool_use_id,
                    queued_at_ms: progress.queued_at_ms,
                    started_at_ms: progress.started_at_ms,
                    last_progress_at_ms: progress.last_progress_at_ms,
                    attempt: progress.attempt,
                    last_attempt_reason: progress.last_attempt_reason,
                    tokens: progress.tokens,
                    tool_calls: progress.tool_calls,
                    last_tool_name: progress.last_tool_name,
                    last_tool_summary: progress.last_tool_summary,
                    prompt_preview: progress.prompt_preview,
                },
            )
            .await;
    }

    /// Persist a transition in the registry, then publish it to the owning
    /// session with its optional failure reason. Shared by `set_status`,
    /// `set_failed` and `finish_workflow_terminal` so all three buffer and
    /// de-register identically.
    async fn publish_status(
        &self,
        task_id: &str,
        status: tasks::TaskStatus,
        error: Option<String>,
    ) {
        tasks::handlers::TaskStatusSink::set_status(&*self.registry, task_id, status).await;
        self.deliver_status(task_id, status, error).await;
    }

    /// The client-facing half of [`Self::publish_status`], for callers that
    /// already persisted the transition in the registry (the atomic
    /// `finish_workflow_terminal` path).
    async fn deliver_status(
        &self,
        task_id: &str,
        status: tasks::TaskStatus,
        error: Option<String>,
    ) {
        let Some((origin_session_id, _)) = self.checkpoints.task_owner(task_id) else {
            self.checkpoints
                .buffer_event(task_id, PendingWorkflowEvent::Status { status, error });
            return;
        };
        if status.is_terminal() {
            self.checkpoints.remove_task(task_id);
        }
        self.emit_status_for_owner(task_id, status, origin_session_id, error)
            .await;
    }

    async fn flush_pending_events(&self, task_id: &str, pending: Vec<PendingWorkflowEvent>) {
        let Some((origin_session_id, _)) = self.checkpoints.task_owner(task_id) else {
            return;
        };
        for event in pending {
            match event {
                PendingWorkflowEvent::Status { status, error } => {
                    self.emit_status_for_owner(task_id, status, origin_session_id.clone(), error)
                        .await;
                    if status.is_terminal() {
                        self.checkpoints.remove_task(task_id);
                    }
                }
                PendingWorkflowEvent::Progress { run_id, progress } => {
                    self.emit_progress_for_owner(
                        task_id,
                        &run_id,
                        origin_session_id.clone(),
                        progress,
                    )
                    .await;
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl tasks::handlers::TaskStatusSink for MobileWorkflowStatusSink {
    async fn set_status(&self, task_id: &str, status: tasks::TaskStatus) {
        self.publish_status(task_id, status, None).await;
    }

    /// Forward the failure reason so a `failed` transition reported through the
    /// dedicated `set_failed` seam reaches the client event, not just the log.
    async fn set_failed(&self, task_id: &str, error: &str) {
        self.publish_status(task_id, tasks::TaskStatus::Failed, Some(error.to_string()))
            .await;
    }

    /// Persist the workflow's terminal payload in the registry — the default
    /// trait method is a no-op, so without this override a mobile workflow's
    /// result/error never reached `TaskRecord` and the `TaskList` rows showed a
    /// reason-less failure.
    async fn set_workflow_outcome(
        &self,
        task_id: &str,
        outcome: lingxi_core::host::task_registry::WorkflowTerminalOutcome,
    ) {
        tasks::handlers::TaskStatusSink::set_workflow_outcome(&*self.registry, task_id, outcome)
            .await;
    }

    /// Atomic terminal publish: store the payload in the registry (so the
    /// `TaskList` row carries `error`), then push the status transition WITH
    /// the reason attached (so the conversation notice can name it).
    async fn finish_workflow_terminal(
        &self,
        task_id: &str,
        outcome: lingxi_core::host::task_registry::WorkflowTerminalOutcome,
        status: tasks::TaskStatus,
    ) {
        let error = outcome.error.clone();
        // Keep the registry's ATOMIC payload+status publish (it closes the
        // outcome/status race a concurrent kill would otherwise win).
        tasks::handlers::TaskStatusSink::finish_workflow_terminal(
            &*self.registry,
            task_id,
            outcome,
            status,
        )
        .await;
        self.deliver_status(task_id, status, error).await;
    }

    async fn is_registered(&self, task_id: &str) -> bool {
        tasks::handlers::TaskStatusSink::is_registered(&*self.registry, task_id).await
    }

    async fn is_terminal(&self, task_id: &str) -> bool {
        tasks::handlers::TaskStatusSink::is_terminal(&*self.registry, task_id).await
    }
}

#[async_trait::async_trait]
impl tasks::handlers::local_workflow::WorkflowProgressSink for MobileWorkflowStatusSink {
    async fn emit_workflow_progress(
        &self,
        task_id: &str,
        run_id: &str,
        progress: tasks::handlers::local_workflow::WorkflowProgressUpdate,
    ) {
        let Some((origin_session_id, _)) = self.checkpoints.task_owner(task_id) else {
            self.checkpoints.buffer_event(
                task_id,
                PendingWorkflowEvent::Progress {
                    run_id: run_id.to_string(),
                    progress,
                },
            );
            return;
        };
        self.emit_progress_for_owner(task_id, run_id, origin_session_id, progress)
            .await;
    }
}

/// Late-bound [`lingxi_core::host::tool_invoker::ToolInvoker`] resolving the composition
/// cycle: the `LocalWorkflowHandler` is registered into the `TaskRegistry`
/// (needs `&mut` — BEFORE the registry is `Arc`-wrapped), yet must dispatch
/// tools through the parent's `Arc<ToolRegistry>`, which is assembled AFTER
/// the task registry exists (its `BuiltinToolContext` carries
/// `task_registry.clone()`). Constructed empty, filled exactly once with the
/// real `RegistryToolInvoker` after `tools` is built; no workflow can
/// dispatch a tool before the build returns. Mirror of the desktop
/// `DeferredToolInvoker`.
pub(crate) struct DeferredToolInvoker {
    inner: std::sync::OnceLock<Arc<dyn lingxi_core::host::tool_invoker::ToolInvoker>>,
}

impl DeferredToolInvoker {
    pub(crate) fn new() -> Self {
        Self {
            inner: std::sync::OnceLock::new(),
        }
    }

    /// Fill the cell with the real invoker. A second call is a no-op (the
    /// first binding wins), matching the build-once semantics.
    pub(crate) fn set(&self, invoker: Arc<dyn lingxi_core::host::tool_invoker::ToolInvoker>) {
        let _ = self.inner.set(invoker);
    }
}

#[async_trait::async_trait]
impl lingxi_core::host::tool_invoker::ToolInvoker for DeferredToolInvoker {
    async fn cleanup_computer_inputs(
        &self,
        agent_id: lingxi_core::types::AgentId,
        origin_session_id: Option<lingxi_core::types::SessionId>,
    ) -> Result<(), lingxi_core::host::tool_invoker::ToolInvokerError> {
        match self.inner.get() {
            Some(inner) => {
                inner
                    .cleanup_computer_inputs(agent_id, origin_session_id)
                    .await
            }
            None => Ok(()),
        }
    }

    fn tool_is_concurrency_safe(&self, name: &str, input: &serde_json::Value) -> Option<bool> {
        self.inner
            .get()
            .and_then(|invoker| invoker.tool_is_concurrency_safe(name, input))
    }

    fn permission_mode(&self) -> Option<String> {
        self.inner
            .get()
            .and_then(|invoker| invoker.permission_mode())
    }

    async fn invoke_supplied_detailed(
        &self,
        name: &str,
        input: serde_json::Value,
        ctx: lingxi_core::host::tool_invoker::SubagentInvocationContext,
        supplied: Arc<dyn std::any::Any + Send + Sync>,
    ) -> Result<
        lingxi_core::host::tool_invoker::ToolInvocationResult,
        lingxi_core::host::tool_invoker::ToolInvokerError,
    > {
        match self.inner.get() {
            Some(invoker) => {
                invoker
                    .invoke_supplied_detailed(name, input, ctx, supplied)
                    .await
            }
            None => Err(lingxi_core::host::tool_invoker::ToolInvokerError::Internal(
                "DeferredToolInvoker: tool dispatch attempted before build() bound the registry"
                    .to_string(),
            )),
        }
    }

    async fn invoke_detailed(
        &self,
        name: &str,
        input: serde_json::Value,
        ctx: lingxi_core::host::tool_invoker::SubagentInvocationContext,
    ) -> Result<
        lingxi_core::host::tool_invoker::ToolInvocationResult,
        lingxi_core::host::tool_invoker::ToolInvokerError,
    > {
        match self.inner.get() {
            Some(invoker) => {
                invoker
                    .invoke_detailed(name, input, ctx)
                    .await
            }
            None => Err(lingxi_core::host::tool_invoker::ToolInvokerError::Internal(
                "DeferredToolInvoker: tool dispatch attempted before build() bound the registry"
                    .to_string(),
            )),
        }
    }

    async fn invoke(
        &self,
        name: &str,
        input: serde_json::Value,
        ctx: lingxi_core::host::tool_invoker::SubagentInvocationContext,
    ) -> Result<serde_json::Value, lingxi_core::host::tool_invoker::ToolInvokerError> {
        match self.inner.get() {
            Some(invoker) => invoker.invoke(name, input, ctx).await,
            None => Err(lingxi_core::host::tool_invoker::ToolInvokerError::Internal(
                "DeferredToolInvoker: tool dispatch attempted before build() bound the registry"
                    .to_string(),
            )),
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// The mobile [`tool_workflow::WorkflowLauncher`]: resolves + validates the
/// script, mints/reuses the run id, persists the script for
/// re-runnability, and spawns the `LocalWorkflow` task through the mobile
/// `TaskRegistry`. Adapted from the desktop `TaskRegistryWorkflowLauncher`
/// with mobile path anchors (`std::fs` is fine here — every path is inside
/// the app sandbox).
pub(crate) struct MobileWorkflowLauncher {
    pub(crate) registry: Arc<tasks::registry::TaskRegistry>,
    /// Project cwd that owns the persisted session directory; fixed for the session.
    pub(crate) project_cwd: std::path::PathBuf,
    /// Live cwd shared with the session and sampled at each launch.
    pub(crate) current_cwd: Arc<std::sync::Mutex<std::path::PathBuf>>,
    /// The lingxi home (`<app_files_root>/.claude`), anchoring
    /// `transcriptDir = <projectDir>/<sessionId>/subagents/workflows/<runId>`.
    pub(crate) lingxi_home: std::path::PathBuf,
    /// The LIVE current-session uuid (bare uuid, no `sess:` prefix), read at
    /// launch time. Mobile retargets sessions inside ONE engine — New/Resume/
    /// Clear swap the id while the orchestrator and this launcher live on —
    /// so a boot-time snapshot would anchor every later workflow's transcript
    /// under a session the user has already left.
    pub(crate) session_uuid: Arc<std::sync::Mutex<String>>,
    /// Live provider-qualified session model selection.
    pub(crate) default_model_selection_provider:
        Arc<std::sync::OnceLock<agent::handle::DefaultModelSelectionProvider>>,
    pub(crate) checkpoints: Arc<MobileWorkflowCheckpointStore>,
    pub(crate) status_sink: Arc<MobileWorkflowStatusSink>,
    /// Same live registry written by the mobile PluginManager and read by the
    /// Workflow tool plus nested resolver.
    pub(crate) plugin_workflows: Arc<workflow::PluginWorkflowRegistry>,
}

#[async_trait::async_trait]
impl tool_workflow::WorkflowLauncher for MobileWorkflowLauncher {
    async fn launch(
        &self,
        mut spec: tool_workflow::WorkflowLaunchSpec,
    ) -> Result<tool_workflow::WorkflowLaunched, tool_workflow::WorkflowLaunchError> {
        let cwd = self
            .current_cwd
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let abs = |p: &str| -> std::path::PathBuf {
            let path = std::path::Path::new(p);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                cwd.join(path)
            }
        };
        let script = tool_workflow::resolve_script_at(
            &cwd,
            &spec,
            |p| std::fs::read_to_string(abs(p)),
            Some(self.plugin_workflows.as_ref()),
        )?;
        // Reject a malformed `meta` block at the tool boundary; the byte-exact
        // message surfaces to the model as the tool error (desktop parity).
        workflow::validate_meta(&script).map_err(|e| {
            let msg = match e {
                workflow::WorkflowError::Script(m) => m,
                other => other.to_string(),
            };
            tool_workflow::WorkflowLaunchError(msg)
        })?;
        // Determinism gate: an INLINE `script` may not use
        // Date.now()/Math.random()/new Date() (breaks resume);
        // author-controlled `scriptPath`/`name` files are exempt.
        let is_inline = spec.script.as_deref().is_some_and(|s| !s.is_empty())
            && spec
                .script_path
                .as_deref()
                .filter(|s| !s.is_empty())
                .is_none();
        if is_inline {
            if let Err(workflow::WorkflowError::Script(m)) = workflow::check_determinism(&script) {
                return Err(tool_workflow::WorkflowLaunchError(m));
            }
        }
        if let Err(error) = workflow::validate_body(&script) {
            let error = match error {
                workflow::WorkflowError::Engine(message)
                | workflow::WorkflowError::Script(message) => message,
            };
            let run_id = tool_workflow::mint_run_id(spec.resume_from_run_id.as_deref());
            let workflow_name = workflow::meta_string_value(&script, "name");
            let summary = workflow::meta_string_value(&script, "description");
            return Ok(tool_workflow::WorkflowLaunched {
                task_id: tasks::generate_task_id(tasks::TaskType::LocalWorkflow),
                run_id: Some(run_id),
                workflow_name,
                summary,
                error: Some(error),
                ..Default::default()
            });
        }
        // One launch belongs to exactly one session. Capture the live session
        // before consulting resume provenance so a concurrent retarget cannot
        // split its task row, checkpoint, and transcript directory across two
        // conversations.
        let session_uuid = spec
            .session_uuid
            .take()
            .filter(|session| !session.is_empty())
            .or_else(|| self.session_uuid.lock().ok().map(|guard| guard.clone()))
            .unwrap_or_default();
        // Resume gate (errorCode 3): a `resumeFromRunId` naming a
        // STILL-RUNNING workflow is rejected — two runs sharing a run id
        // would race on the same journal.
        if let Some(rid) = spec.resume_from_run_id.as_deref().filter(|s| !s.is_empty()) {
            // The tool schema advertises `^wf_[a-z0-9-]{6,}$`, but nothing in
            // the workspace validates JSON-Schema `pattern` — and this id
            // becomes a FILENAME (the persisted script below, and the journal
            // in the task handler). An unchecked `../…` or absolute value
            // would write outside the scratch dir, e.g. over the workspace's
            // host-managed `lib/lingxi-bridge.js`.
            if !tool_workflow::is_valid_run_id(rid) {
                return Err(tool_workflow::WorkflowLaunchError(format!(
                    "resumeFromRunId {rid:?} is not a workflow run id (expected wf_ followed by \
                     at least 6 lowercase alphanumerics or dashes)"
                )));
            }
            if let Some(task_id) = self.registry.find_running_workflow_by_run_id(rid).await {
                return Err(tool_workflow::WorkflowLaunchError(format!(
                    "Workflow {rid} is still running (task {task_id}). Stop it first with \
                     TaskStop({{taskId: \"{task_id}\"}}) before resuming."
                )));
            }
        }
        // Mint the run id up front so every run-scoped path derives from the
        // id the task receives.
        let run_id = tool_workflow::mint_run_id(spec.resume_from_run_id.as_deref());
        let workflow_name = workflow::meta_string_value(&script, "name");
        let summary = workflow::meta_string_value(&script, "description");
        let task_description = summary
            .clone()
            .unwrap_or_else(|| "Dynamic workflow".to_string());
        // Reserve before the first run-id-derived filesystem write. The async
        // block below collects every later error so the reservation is always
        // released exactly once.
        let reservation = self
            .registry
            .try_reserve_workflow_run_id(&run_id)
            .await
            .map_err(|error| tool_workflow::WorkflowLaunchError(error.to_string()))?;
        let launch_result = async {
            let subagents = orchestrator::transcript_paths::subagents_dir(
                &self.lingxi_home,
                &self.project_cwd.to_string_lossy(),
                &session_uuid,
            );
            // Persist the script so it is editable + re-runnable via `scriptPath`.
            // A `scriptPath` input is already on disk → returned as-is; an
            // inline/`name` script is written under the session-owned workflow dir.
            let script_path = if let Some(p) = spec.script_path.as_deref().filter(|s| !s.is_empty())
            {
                abs(p).to_str().map(str::to_string).ok_or_else(|| {
                    tool_workflow::WorkflowLaunchError(
                        "workflow script path is not valid UTF-8".to_string(),
                    )
                })?
            } else {
                let session_dir = subagents.parent().ok_or_else(|| {
                    tool_workflow::WorkflowLaunchError(
                        "cannot derive workflow session directory".to_string(),
                    )
                })?;
                let dir = session_dir.join("workflows");
                let file = dir.join(format!("{run_id}.js"));
                std::fs::create_dir_all(&dir).map_err(|error| {
                    tool_workflow::WorkflowLaunchError(format!(
                        "cannot create workflow script directory '{}': {error}",
                        dir.display()
                    ))
                })?;
                std::fs::write(&file, &script).map_err(|error| {
                    tool_workflow::WorkflowLaunchError(format!(
                        "cannot persist workflow script '{}': {error}",
                        file.display()
                    ))
                })?;
                file.to_str().map(str::to_string).ok_or_else(|| {
                    tool_workflow::WorkflowLaunchError(
                        "workflow script path is not valid UTF-8".to_string(),
                    )
                })?
            };
            let transcript_dir = { subagents.join("workflows").join(&run_id) };
            std::fs::create_dir_all(&transcript_dir).map_err(|error| {
                tool_workflow::WorkflowLaunchError(format!(
                    "cannot create workflow transcript directory '{}': {error}",
                    transcript_dir.display()
                ))
            })?;
            let journal_path = transcript_dir.join("journal.jsonl");
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&journal_path)
                .map_err(|error| {
                    tool_workflow::WorkflowLaunchError(format!(
                        "cannot create workflow journal '{}': {error}",
                        journal_path.display()
                    ))
                })?;
            let transcript_dir_wire = transcript_dir.to_str().map(str::to_string);
            let has_script_path = spec
                .script_path
                .as_deref()
                .filter(|s| !s.is_empty())
                .is_some();
            let has_name = spec.name.as_deref().filter(|s| !s.is_empty()).is_some();
            let named_source = spec
                .name
                .as_deref()
                .filter(|s| !s.is_empty())
                .and_then(|name| {
                    tool_workflow::workflow_source_for_name(
                        &cwd,
                        name,
                        Some(self.plugin_workflows.as_ref()),
                    )
                });
            let (invocation_mode, workflow_source) = if has_script_path {
                ("scriptPath".to_string(), "scriptPath".to_string())
            } else if has_name {
                (
                    "named".to_string(),
                    if named_source.is_some() {
                        named_source.unwrap_or("custom").to_string()
                    } else {
                        "custom".to_string()
                    },
                )
            } else {
                ("inline".to_string(), "inline".to_string())
            };
            let script_sha256 = sha256_hex(script.as_bytes());
            let workflow_id = workflow_name
                .clone()
                .filter(|name| !name.is_empty())
                .or_else(|| spec.name.clone())
                .unwrap_or_default();
            let task_id = self
                .registry
                .spawn(
                    tasks::TaskType::LocalWorkflow,
                    tasks::TaskSpawnInput::LocalWorkflow {
                        session_uuid: Some(session_uuid.clone()),
                        workflow_id: workflow_id.clone(),
                        script,
                        resume_from_run_id: spec.resume_from_run_id.clone(),
                        args: spec
                            .args
                            .as_ref()
                            .map(|v| serde_json::to_string(v).unwrap_or_default()),
                        run_id: Some(run_id.clone()),
                        invocation_mode: Some(invocation_mode),
                        workflow_source: Some(workflow_source),
                        script_is_verbatim_builtin: None,
                        transcript_subdir: Some(transcript_dir.clone()),
                        launched_from_subagent: spec.launched_from_subagent,
                        tool_use_id: spec.tool_use_id.clone(),
                        creator_teammate_name: spec.creator_teammate_name.clone(),
                        creator_team_name: spec.creator_team_name.clone(),
                        creator_agent_id: spec
                            .creator_agent_id
                            .as_deref()
                            .and_then(lingxi_core::types::AgentId::parse_prefixed),
                    },
                    task_description,
                )
                .await
                .map_err(|e| tool_workflow::WorkflowLaunchError(e.to_string()))?;
            self.registry
                .set_workflow_resume_metadata(&task_id, script_path.clone(), transcript_dir.clone())
                .await
                .map_err(|error| tool_workflow::WorkflowLaunchError(error.to_string()))?;
            let args_json = spec
                .args
                .as_ref()
                .map(|value| serde_json::to_string(value).unwrap_or_default());
            let pending_events = match self.checkpoints.upsert(
                &session_uuid,
                WorkflowCheckpoint {
                    task_id: task_id.clone(),
                    workflow_run_id: run_id.clone(),
                    workflow_id: workflow_id.clone(),
                    script_path: script_path.clone(),
                    script_sha256: Some(script_sha256),
                    args_json,
                    description: summary
                        .clone()
                        .or_else(|| workflow_name.clone())
                        .unwrap_or_else(|| "Workflow".to_string()),
                    start_time: Some(unix_time_ms()),
                    transcript_dir: transcript_dir.to_string_lossy().into_owned(),
                },
            ) {
                Ok(pending_events) => pending_events,
                Err(error) => {
                    let _ = self.registry.kill(&task_id).await;
                    return Err(tool_workflow::WorkflowLaunchError(format!(
                        "cannot persist workflow checkpoint: {error}"
                    )));
                }
            };
            self.status_sink
                .flush_pending_events(&task_id, pending_events)
                .await;
            if let Some(state) = self.registry.get(&task_id).await {
                if state.base().status.is_terminal() {
                    self.checkpoints.remove_task(&task_id);
                }
            }
            if spec.resume_from_run_id.is_some() {
                self.registry
                    .remove_paused_workflow_by_run_id(&session_uuid, &run_id)
                    .await;
            }
            Ok(tool_workflow::WorkflowLaunched {
                task_id,
                run_id: Some(run_id.clone()),
                script_path: Some(script_path),
                workflow_name,
                summary,
                transcript_dir: transcript_dir_wire,
                error: None,
            })
        }
        .await;
        drop(reservation);
        launch_result
    }
}

#[cfg(test)]
mod run_id_tests {
    use std::sync::Arc;

    use tool_workflow::is_valid_run_id;

    use crate::mobile::test_support::FakeListener;

    #[tokio::test]
    async fn checkpoint_round_trip_adopts_a_paused_workflow() {
        let root = tempfile::tempdir().expect("tempdir");
        let home = root.path().join(".claude");
        let session = "00000000-0000-0000-0000-000000000001";
        let store = Arc::new(super::MobileWorkflowCheckpointStore::new(
            home,
            root.path().to_path_buf(),
        ));
        let transcript_dir = store
            .session_dir(session)
            .join("subagents")
            .join("workflows")
            .join("wf_abcdef");
        std::fs::create_dir_all(&transcript_dir).unwrap();
        std::fs::write(transcript_dir.join("journal.jsonl"), "").unwrap();
        let script_path = root.path().join("build.js");
        let script = b"return 1;";
        std::fs::write(&script_path, script).unwrap();
        store
            .upsert(
                session,
                super::WorkflowCheckpoint {
                    task_id: "wabc12345".into(),
                    workflow_run_id: "wf_abcdef".into(),
                    workflow_id: "build-workflow".into(),
                    script_path: script_path.to_string_lossy().into_owned(),
                    script_sha256: Some(super::sha256_hex(script)),
                    args_json: Some(r#"{"topic":"demo"}"#.into()),
                    description: "Build workflow".into(),
                    start_time: Some(1234),
                    transcript_dir: transcript_dir.to_string_lossy().into_owned(),
                },
            )
            .unwrap();

        let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
            platform_posix_minimal::PosixFileSystem::new(root.path().to_path_buf()),
        );
        let output = Arc::new(tasks::output_manager::TaskOutputManager::new(
            root.path().join("task-output"),
            fs.clone(),
        ));
        std::fs::create_dir_all(root.path().join("task-output")).unwrap();
        let registry = tasks::registry::TaskRegistry::new(
            Arc::new(platform_posix_minimal::PosixRuntime::new()),
            fs,
            output,
        );
        store.adopt_session(session, &registry).await;

        let state = registry.get("wabc12345").await.expect("adopted task");
        assert_eq!(state.base().status, tasks::TaskStatus::Paused);
        let tasks::state::TaskState::LocalWorkflow(workflow) = state else {
            panic!("expected workflow")
        };
        assert_eq!(workflow.run_id.as_deref(), Some("wf_abcdef"));
        assert_eq!(
            workflow.workflow_id,
            "build-workflow"
        );
        assert_eq!(workflow.script_path.as_deref(), script_path.to_str());

        store.remove_task("wabc12345");
        assert!(
            !store.path(session).exists(),
            "terminal cleanup removes the last handoff file"
        );
    }

    #[test]
    fn adopt_json_omits_absent_fields_and_preserves_other_handoffs() {
        let root = tempfile::tempdir().expect("tempdir");
        let session = "00000000-0000-0000-0000-000000000002";
        let store = super::MobileWorkflowCheckpointStore::new(
            root.path().join(".claude"),
            root.path().to_path_buf(),
        );
        store
            .upsert(
                session,
                super::WorkflowCheckpoint {
                    task_id: "wabc12345".into(),
                    workflow_run_id: "wf_abcdef".into(),
                    workflow_id: "build-workflow".into(),
                    script_path: "/workspace/build.js".into(),
                    script_sha256: None,
                    args_json: None,
                    description: "Build workflow".into(),
                    start_time: None,
                    transcript_dir: "/workspace/transcript".into(),
                },
            )
            .expect("upsert checkpoint");

        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(store.path(session)).expect("read adopt file"))
                .expect("parse adopt file");
        let workflow = &value["workflows"][0];
        assert_eq!(
            workflow["workflowId"],
            "build-workflow"
        );
        assert!(workflow.get("scriptSha256").is_none());
        assert!(workflow.get("argsJson").is_none());
        assert!(workflow.get("startTime").is_none());

        let mut file = store.read(session);
        file.shells.push(serde_json::json!({ "taskId": "b123" }));
        store.write(session, file).expect("add shell handoff");
        store.remove_task("wabc12345");

        let value: serde_json::Value = serde_json::from_slice(
            &std::fs::read(store.path(session)).expect("shell handoff must remain"),
        )
        .expect("parse preserved adopt file");
        assert_eq!(value["workflows"], serde_json::json!([]));
        assert_eq!(value["shells"], serde_json::json!([{ "taskId": "b123" }]));
    }

    /// The minted shape is accepted; every escape shape a resume id could
    /// carry into `dir.join(format!("{run_id}.js"))` is refused.
    #[test]
    fn run_id_validation_refuses_path_escapes() {
        assert!(is_valid_run_id("wf_1a2b3c4d-0ff"));
        assert!(is_valid_run_id("wf_abcdef"));

        for bad in [
            "",
            "wf_",
            "wf_abc",
            "../../lib/lingxi-bridge",
            "wf_../../lib/lingxi-bridge",
            "/tmp/anywhere",
            "wf_/tmp/anywhere",
            "wf_ABCDEF",
            "wf_abc def",
            "wf_abc.def",
        ] {
            assert!(!is_valid_run_id(bad), "must refuse {bad:?}");
        }
    }

    /// r3-failure-paths-07: a workflow that FAILS must reach the client with
    /// the engine's reason attached, not as a bare task id.
    ///
    /// Both halves are asserted: the terminal payload lands in the registry
    /// (so the `TaskList` row a later session refresh pulls carries `error`),
    /// and the pushed `TaskStatusChanged` carries the same reason. The
    /// completed case is the vacuity guard — it proves the assertion below is
    /// reading a field that is genuinely `None` for a non-failure, i.e. the
    /// test cannot pass by always finding a reason.
    #[tokio::test]
    async fn failed_workflow_terminal_carries_the_reason_to_the_client() {
        use tasks::handlers::TaskStatusSink as _;

        let listener = Arc::new(FakeListener::default());
        let root = tempfile::tempdir().expect("tempdir");
        let checkpoints = Arc::new(super::MobileWorkflowCheckpointStore::new(
            root.path().join(".claude"),
            root.path().to_path_buf(),
        ));
        let active_session = Arc::new(std::sync::Mutex::new("session-a".to_string()));
        let sink = super::MobileWorkflowStatusSink::new(
            listener.clone(),
            checkpoints,
            active_session.clone(),
        );
        let _ = sink
            .checkpoints
            .track_task_owner("w12345678", "session-a", "wf_failing");
        let _ = sink
            .checkpoints
            .track_task_owner("w87654321", "session-a", "wf_ok");

        sink.finish_workflow_terminal(
            "w12345678",
            lingxi_core::host::task_registry::WorkflowTerminalOutcome {
                error: Some("step 2 `build` exited 1".to_string()),
                ..Default::default()
            },
            tasks::TaskStatus::Failed,
        )
        .await;
        sink.finish_workflow_terminal(
            "w87654321",
            lingxi_core::host::task_registry::WorkflowTerminalOutcome {
                result: Some("done".to_string()),
                ..Default::default()
            },
            tasks::TaskStatus::Completed,
        )
        .await;

        let events = listener.received.lock().await.clone();
        let statuses: Vec<(String, Option<String>)> = events
            .iter()
            .filter_map(|event| match event {
                client::protocol::events::ClientEvent::TaskStatusChanged {
                    task_id, error, ..
                } => Some((task_id.clone(), error.clone())),
                _ => None,
            })
            .collect();
        // Vacuity guard: without BOTH transitions on the wire the assertions
        // below would pass over an empty list.
        assert_eq!(
            statuses.len(),
            2,
            "expected both terminal transitions, got {statuses:?}"
        );
        assert_eq!(
            statuses[0],
            (
                "w12345678".to_string(),
                Some("step 2 `build` exited 1".to_string())
            ),
            "the failed transition must name the reason"
        );
        assert_eq!(
            statuses[1],
            ("w87654321".to_string(), None),
            "a completed transition must not carry a reason"
        );
    }

    #[tokio::test]
    async fn workflow_progress_sink_calls_structured_listener() {
        let listener = Arc::new(FakeListener::default());
        let root = tempfile::tempdir().expect("tempdir");
        let checkpoints = Arc::new(super::MobileWorkflowCheckpointStore::new(
            root.path().join(".claude"),
            root.path().to_path_buf(),
        ));
        let active_session = Arc::new(std::sync::Mutex::new("session-a".to_string()));
        let sink = super::MobileWorkflowStatusSink::new(
            listener.clone(),
            checkpoints,
            active_session.clone(),
        );

        let progress = tasks::handlers::local_workflow::WorkflowProgressUpdate {
            kind: "workflow_agent".to_string(),
            index: 2,
            title: None,
            message: None,
            label: Some("Design".to_string()),
            phase_index: Some(1),
            phase_title: Some("Design".to_string()),
            agent_id: Some("agent:123".to_string()),
            agent_type: Some("workflow-subagent".to_string()),
            model: Some("gpt-5.4".to_string()),
            fallback_model: None,
            state: Some("running".to_string()),
            error: None,
            tool_use_id: Some("workflow_agent_2_agent:123".to_string()),
            queued_at_ms: Some(10),
            started_at_ms: Some(20),
            last_progress_at_ms: Some(30),
            attempt: Some(1),
            last_attempt_reason: None,
            tokens: Some(40),
            tool_calls: Some(3),
            last_tool_name: Some("Read".to_string()),
            last_tool_summary: Some("Read".to_string()),
            prompt_preview: Some("design screen".to_string()),
        };

        // The worker can emit before launch() has persisted the checkpoint.
        // That event must be retained and replayed once ownership is known.
        tasks::handlers::local_workflow::WorkflowProgressSink::emit_workflow_progress(
            &sink,
            "task_123",
            "wf_abcdef",
            progress.clone(),
        )
        .await;
        assert!(listener.workflow_progress.lock().await.is_empty());

        let pending = sink
            .checkpoints
            .track_task_owner("task_123", "session-a", "wf_abcdef");
        sink.flush_pending_events("task_123", pending).await;

        let events = listener.workflow_progress.lock().await.clone();
        assert!(events
            .iter()
            .any(|(session_id, task_id, run_id, progress)| {
                session_id == "session-a"
                    && task_id == "task_123"
                    && run_id == "wf_abcdef"
                    && progress.kind == "workflow_agent"
                    && progress.index == 2
                    && progress.label.as_deref() == Some("Design")
                    && progress.agent_type.as_deref() == Some("workflow-subagent")
                    && progress.tokens == Some(40)
            }));

        *active_session.lock().unwrap() = "session-b".to_string();
        tasks::handlers::local_workflow::WorkflowProgressSink::emit_workflow_progress(
            &sink,
            "task_123",
            "wf_abcdef",
            tasks::handlers::local_workflow::WorkflowProgressUpdate {
                kind: "workflow_agent".to_string(),
                index: 3,
                title: None,
                message: None,
                label: None,
                phase_index: None,
                phase_title: None,
                agent_id: None,
                agent_type: None,
                model: None,
                fallback_model: None,
                state: Some("running".to_string()),
                error: None,
                tool_use_id: None,
                queued_at_ms: None,
                started_at_ms: None,
                last_progress_at_ms: None,
                attempt: None,
                last_attempt_reason: None,
                tokens: None,
                tool_calls: None,
                last_tool_name: None,
                last_tool_summary: None,
                prompt_preview: None,
            },
        )
        .await;
        assert_eq!(listener.workflow_progress.lock().await.len(), 1);
    }

    // ── P-1.7 R1: the Host mints the scope, and a genuine build blocks delete ──

    /// A stub `LocalWorkflow` handler.
    ///
    /// `TaskRegistry::spawn` needs a handler to dispatch to, but nothing in
    /// these two tests is about running JavaScript: the registry still builds
    /// the real task row from the real spawn input (`state_for_spawn`), which
    /// is where the launcher's `scope` lands. The stub never completes its
    /// task, so the row stays non-terminal -- which IS the "build still in
    /// flight" state the App delete guard exists for, held still instead of
    /// raced against a real QuickJS run.
    struct StubWorkflowHandler;

    #[async_trait::async_trait]
    impl tasks::task_trait::Task for StubWorkflowHandler {
        fn name(&self) -> &str {
            "stub_local_workflow"
        }

        fn task_type(&self) -> tasks::TaskType {
            tasks::TaskType::LocalWorkflow
        }

        async fn spawn(
            &self,
            _input: tasks::TaskSpawnInput,
            _ctx: tasks::task_trait::TaskContext,
        ) -> Result<tasks::task_trait::TaskHandle, tasks::task_trait::TaskError> {
            Ok(tasks::task_trait::TaskHandle::new(
                tasks::generate_task_id(tasks::TaskType::LocalWorkflow),
                None,
            ))
        }

        async fn kill(
            &self,
            _task_id: &str,
            _ctx: tasks::task_trait::TaskContext,
        ) -> Result<(), tasks::task_trait::TaskError> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod deferred_invoker_tests {
    use std::sync::Arc;

    use lingxi_core::host::tool_invoker::{
        SubagentInvocationContext, ToolInvoker, ToolInvokerError,
    };

    /// Terminal invoker whose results the deferred wrapper must forward intact.
    struct RecordingInvoker;

    #[async_trait::async_trait]
    impl ToolInvoker for RecordingInvoker {
        fn permission_mode(&self) -> Option<String> {
            Some("plan".into())
        }

        async fn invoke_supplied_detailed(
            &self,
            name: &str,
            input: serde_json::Value,
            ctx: SubagentInvocationContext,
            supplied: Arc<dyn std::any::Any + Send + Sync>,
        ) -> Result<lingxi_core::host::tool_invoker::ToolInvocationResult, ToolInvokerError>
        {
            assert_eq!(
                supplied.downcast_ref::<String>().map(String::as_str),
                Some("private child tool")
            );
            let mut result = self.invoke_detailed(name, input, ctx).await?;
            result.turn_end = Some(lingxi_core::host::tool_invoker::ToolResultTurnEnd {
                source: lingxi_core::host::tool_invoker::ToolResultTurnEndSource::Tool,
            });
            Ok(result)
        }

        async fn invoke_detailed(
            &self,
            _name: &str,
            _input: serde_json::Value,
            _ctx: SubagentInvocationContext,
        ) -> Result<lingxi_core::host::tool_invoker::ToolInvocationResult, ToolInvokerError>
        {
            Ok(lingxi_core::host::tool_invoker::ToolInvocationResult { mcp_meta_projection: None, model_content_projection: None, data_projection: None,
                is_error: true,
                data: serde_json::json!({"error": "contract validation failed"}),
                model_content: Some("contract validation failed".into()),
                turn_end: None,
                new_messages: Vec::new(),
                context_modifier: None,
                mcp_meta: None,
                context: lingxi_core::types::utf16_json::Utf16JsonProjection::plain(
                    serde_json::Value::Array(Vec::new()),
                ),
                context_state: None,
            })
        }

        async fn invoke(
            &self,
            _name: &str,
            _input: serde_json::Value,
            _ctx: SubagentInvocationContext,
        ) -> Result<serde_json::Value, ToolInvokerError> {
            Ok(serde_json::json!({}))
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    #[tokio::test]
    async fn deferred_invoker_preserves_error_content() {
        let deferred = super::DeferredToolInvoker::new();
        deferred.set(Arc::new(RecordingInvoker));
        let result = deferred
            .invoke_detailed("Read", serde_json::json!({}), bare_ctx())
            .await
            .unwrap();
        assert!(result.is_error);
        assert_eq!(
            result.data,
            serde_json::json!({"error": "contract validation failed"})
        );
        assert_eq!(
            result.model_content.as_deref(),
            Some("contract validation failed")
        );
    }

    #[tokio::test]
    async fn deferred_invoker_preserves_private_child_tool_and_live_mode() {
        let deferred = super::DeferredToolInvoker::new();
        deferred.set(Arc::new(RecordingInvoker));
        assert_eq!(deferred.permission_mode().as_deref(), Some("plan"));
        let result = deferred
            .invoke_supplied_detailed(
                "SubagentHandback",
                serde_json::json!({}),
                bare_ctx(),
                Arc::new("private child tool".to_string()),
            )
            .await
            .unwrap();
        assert_eq!(
            result.turn_end.unwrap().source,
            lingxi_core::host::tool_invoker::ToolResultTurnEndSource::Tool
        );
    }

    fn bare_ctx() -> SubagentInvocationContext {
        SubagentInvocationContext {
            input_projection: None,
            cancellation_token: lingxi_core::host::CancellationToken::new(),
            permission_pause_observer: None,
            parent_agent_id: None,
            origin_session_id: None,
            instruction_context: None,
            fork_context: None,
            tool_execution_policy: lingxi_core::host::tool_invoker::ToolExecutionPolicy::Ordinary,
            agent_name: None,
            team_name: None,
            is_async: false,
            is_non_interactive_session: false,
            can_show_permission_prompts: false,
            cwd: None,
            tool_use_id: None,
            assistant_message_id: None,
            depth: 0,
            observer: None,
            parent_model: None,
            parent_model_profile: None,
            agent_spawn_provenance: Default::default(),
            tool_context_state: None,
            assistant_message: None,
            same_turn_tool_uses: Vec::new(),
            current_history: Vec::new(),
            mode_override: None,
            request_source: None,
            frozen_command_denies: Vec::new(),
        }
    }
}
