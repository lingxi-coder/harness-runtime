//! `TaskRegistryHandle` trait impl for `TaskRegistry`.
//!
//! Bridges the `lingxi-traits` seam (string-typed wire format) to the
//! concrete `TaskRegistry` (typed `TaskType` + `TaskStatus`) so the six
//! `Task*` tools in `lingxi-tools` can dispatch without taking a cyclic dep
//! on `lingxi-tasks`.
//!
//! Task-type and task-status wire strings here are byte-aligned with the
//! locked constants in `lingxi-tools::builtin::task` (`TASK_TYPES` /
//! `TASK_STATUSES`).

use crate::id::TaskType;
use crate::registry::TaskRegistry;
use crate::state::{TaskState, TaskStatus};
use crate::task_trait::{TaskError, TaskSpawnInput};
use async_trait::async_trait;
use platform_api::task_registry::{
    MonitorRegistration, TaskCreateInput, TaskListFilter, TaskOutputChunk, TaskRecord,
    TaskRegistryError, TaskRegistryHandle, TaskUpdatePatch, WorkflowRecord,
};
use std::path::PathBuf;

fn task_type_from_wire(s: &str) -> Result<TaskType, TaskRegistryError> {
    Ok(match s {
        "local_bash" => TaskType::LocalBash,
        "local_agent" => TaskType::LocalAgent,
        "remote_agent" => TaskType::RemoteAgent,
        "in_process_teammate" => TaskType::InProcessTeammate,
        "local_workflow" => TaskType::LocalWorkflow,
        "monitor_mcp" => TaskType::MonitorMcp,
        "monitor" | "monitor_ws" => TaskType::Monitor,
        "mcp_task" => TaskType::McpTask,
        "dream" => TaskType::Dream,
        "auto_mode_scan" => TaskType::AutoModeScan,
        "local_fusion" => TaskType::LocalFusion,
        other => {
            return Err(TaskRegistryError::InvalidInput(format!(
                "unknown task_type '{other}'"
            )));
        }
    })
}

pub(crate) fn task_type_to_wire(t: TaskType) -> &'static str {
    match t {
        TaskType::LocalBash => "local_bash",
        TaskType::LocalAgent => "local_agent",
        TaskType::RemoteAgent => "remote_agent",
        TaskType::InProcessTeammate => "in_process_teammate",
        TaskType::LocalWorkflow => "local_workflow",
        TaskType::MonitorMcp => "monitor_mcp",
        TaskType::Monitor => "monitor_ws",
        TaskType::McpTask => "mcp_task",
        TaskType::Dream => "dream",
        TaskType::AutoModeScan => "auto_mode_scan",
        TaskType::LocalFusion => "local_fusion",
    }
}

fn status_from_wire(s: &str) -> Result<TaskStatus, TaskRegistryError> {
    Ok(match s {
        "pending" => TaskStatus::Pending,
        "running" => TaskStatus::Running,
        "paused" => TaskStatus::Paused,
        "completed" => TaskStatus::Completed,
        "failed" => TaskStatus::Failed,
        "killed" => TaskStatus::Killed,
        other => {
            return Err(TaskRegistryError::InvalidInput(format!(
                "unknown status '{other}'"
            )));
        }
    })
}

pub(crate) fn status_to_wire(s: TaskStatus) -> &'static str {
    match s {
        TaskStatus::Pending => "pending",
        TaskStatus::Running => "running",
        TaskStatus::Paused => "paused",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::Killed => "killed",
    }
}

pub(crate) fn state_to_record(s: &TaskState) -> TaskRecord {
    let b = s.base();
    let started_at_ms = b
        .start_time
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok());
    // Mirror claude-code `LocalShellTaskState.command`: `local_bash` and the
    // shell-event monitor (`monitor_ws`) both carry a shell command. `TaskStop`
    // prefers `command` over `description` for shell-backed tasks
    // (`stopTask.ts:97`); every other task type reports `None`.
    let command = match s {
        TaskState::LocalBash(bash) => Some(bash.command.clone()),
        TaskState::Monitor(monitor) if b.task_type == TaskType::LocalBash => {
            Some(monitor.command.clone())
        }
        _ => None,
    };
    // Per-task-type extras consumed by the `Stop` / `SubagentStop` hook
    // `background_tasks` builder (claude-code `Lic`). Each variant fills only the
    // subset claude-code's `switch (n.type)` sets; the rest stay `None`.
    // `local_agent` carries `agent_type` (← the dispatched subagent type, sourced
    // from the agent id's prefix label) + `is_backgrounded` (the `wA` filter
    // field); `monitor_mcp` carries `server` (← `server_name`); `local_workflow`
    // carries `name` (← `workflow_id`); `monitor_mcp` carries only `server`
    // (`MonitorMcpTaskState` watches resources, not one tool), while `mcp_task`
    // (`McpTaskState`) carries BOTH `server` and `tool`.
    // claude-code `task.agentId`, the input to `TaskStop`'s ownership guard
    // (`sut`). A `local_agent` reports its OWN id — NOT its creator, which is
    // the oracle's separate `ownerAgentId` and is not what `sut` reads. A
    // `local_bash` reports the agent that SPAWNED it. Every other type carries
    // no `agentId` upstream, so it stays `None`, which `sut` treats as
    // main-session-only.
    let owner_agent_id = match s {
        TaskState::LocalAgent(a) => Some(a.agent_id.to_string()),
        TaskState::LocalBash(bash) => bash.base.creator_agent_id.map(|id| id.to_string()),
        _ => None,
    };
    // Who stopped it, when something did — the port's witness for the oracle's
    // `stoppedByUser`. `local_agent` only; see the field's doc.
    let killed_by = match s {
        TaskState::LocalAgent(a) => a.outcome.killed_by.clone(),
        _ => None,
    };
    let (agent_type, server, tool, name, is_backgrounded, forked_skill_name) = match s {
        TaskState::LocalAgent(a) => (
            Some(a.subagent_type.clone()),
            None,
            None,
            None,
            Some(a.is_backgrounded),
            a.forked_skill_name.clone(),
        ),
        // A backgrounded shell is a task the model can address, so it carries
        // the same `is_backgrounded` flag the `Stop` hook filter reads for
        // agents (claude-code sets `isBackgrounded` on the `local_bash` record).
        TaskState::LocalBash(bash) => (None, None, None, None, bash.is_backgrounded, None),
        TaskState::MonitorMcp(m) => (None, Some(m.server_name.clone()), None, None, None, None),
        TaskState::Monitor(_) => (None, None, None, None, None, None),
        // `mcp_task` surfaces BOTH the server and the single tool it detached
        // (claude-code `Lic` `switch(n.type)`), unlike `monitor_mcp`.
        TaskState::McpTask(m) => (
            None,
            Some(m.server_name.clone()),
            Some(m.tool_name.clone()),
            None,
            None,
            None,
        ),
        TaskState::LocalWorkflow(w) => (None, None, None, Some(w.workflow_id.clone()), None, None),
        _ => (None, None, None, None, None, None),
    };
    // Terminal failure reason. `local_workflow` records the fatal script/engine
    // error on its `WorkflowTerminalOutcome`; `local_agent` stores the runner's
    // reason on the state directly (registry `set_agent_outcome`). Fusion also
    // preserves a publication/storage error independently from its computed
    // answer. Every other task type has no reason seam and stays `None`.
    let error = match s {
        TaskState::LocalWorkflow(w) => w.outcome.error.clone(),
        TaskState::LocalAgent(a) => a.error.clone(),
        TaskState::LocalFusion(f) => f.error.clone().or_else(|| f.publication_error.clone()),
        _ => None,
    };
    // F005: `local_fusion` carries its current progress-stage label (the same
    // text the Agent-tool path forwards as `subagent_activity`) so a
    // `/fusion` task's DTO/list entry can render live progress. Every other
    // task type has no such field, so this stays `None`.
    let stage = match s {
        TaskState::LocalFusion(f) => f.stage.clone(),
        _ => None,
    };
    TaskRecord {
        completed_agent_visible: false,
        notified: b.notified,
        model: match s {
            TaskState::LocalAgent(agent) => agent.outcome.model.clone(),
            _ => None,
        },
        effort: match s {
            TaskState::LocalAgent(agent) => agent.outcome.effort.clone(),
            _ => None,
        },
        is_observer: matches!(s, TaskState::LocalAgent(agent) if agent.is_observer),
        teammate_name: if matches!(s, TaskState::InProcessTeammate(_)) {
            b.creator_teammate_name.clone()
        } else {
            None
        },
        teammate_agent_id: if matches!(s, TaskState::InProcessTeammate(_)) {
            b.creator_teammate_name.as_ref().map(|name| {
                match b
                    .creator_team_name
                    .as_deref()
                    .filter(|team| !team.is_empty())
                {
                    Some(team) => format!("{name}@{team}"),
                    None => name.clone(),
                }
            })
        } else {
            None
        },
        kind: matches!(s, TaskState::Monitor(_) if b.task_type == TaskType::LocalBash)
            .then(|| "monitor".into()),
        is_parked: matches!(s, TaskState::LocalAgent(agent) if agent.is_parked),
        is_idle: matches!(s, TaskState::InProcessTeammate(teammate) if teammate.is_idle),
        awaiting_plan_approval: matches!(s, TaskState::InProcessTeammate(teammate) if teammate.awaiting_plan_approval && !b.status.is_terminal()),
        task_id: b.id.clone(),
        task_type: task_type_to_wire(b.task_type).to_string(),
        status: status_to_wire(b.status).to_string(),
        description: b.description.clone(),
        started_at_ms,
        command,
        agent_type,
        owner_agent_id,
        killed_by,
        server,
        tool,
        name,
        forked_skill_name,
        is_backgrounded,
        is_adopted: matches!(s, TaskState::LocalBash(bash) if bash.is_adopted),
        caller: match s {
            TaskState::LocalBash(bash) => bash.caller.clone(),
            _ => None,
        },
        error,
        stage,
    }
}

/// Port of `extractTextContent(blocks, '\n')` (claude-code
/// `utils/messages.ts:2893-2901`): filter the content blocks whose `type` is
/// `"text"` and join their `text` fields with `\n`.
///
/// The agent's final-message `content` arrives here as a `serde_json::Value`
/// (the spooled `SubagentResult::Completed.content`). A non-array `content`
/// (e.g. a bare string the spawner returned) yields no `text` blocks → `""`,
/// exactly mirroring the TS `.filter(...).map(...).join(...)` over a typed
/// block array.
pub(crate) fn extract_text_content(content: &serde_json::Value) -> String {
    // The runner's completion payload is an OBJECT — `{text, content:[…]}` —
    // and the `local_agent` spool holds exactly that object, pretty-printed.
    // Reading only a bare array meant `as_array()` failed for every production
    // agent, so `TaskOutputChunk.result` was ALWAYS `None` and `TaskOutput`
    // fell back to the raw JSON transcript instead of the agent's answer. The
    // bare-array form is still accepted: it is what a caller holding the
    // content blocks directly passes (the one-shot handler does), and what the
    // fixtures use.
    //
    // `text` wins over `content` when both are present, matching the runner's
    // own `completed_result_text`.
    if let Some(text) = content.get("text").and_then(serde_json::Value::as_str) {
        if !text.is_empty() {
            return text.to_string();
        }
    }
    let blocks = match content.as_array() {
        Some(blocks) => blocks,
        None => match content.get("content").and_then(serde_json::Value::as_array) {
            Some(blocks) => blocks,
            None => return String::new(),
        },
    };
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(serde_json::Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(serde_json::Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Recover the agent's final-message content `Value` from a `local_agent`
/// spool. The `local_agent` handler writes
/// `to_string_pretty(content)` + `"\n<usage><total_tokens>N</total_tokens></usage>\n"`
/// (`handlers/local_agent.rs`). Strip the trailing `<usage>…</usage>` footer
/// (if present) and JSON-parse the remainder back into the original `content`
/// `Value`, so `extract_text_content` can pull the clean answer out of it.
/// Returns `None` when the spool is empty or does not parse (e.g. a `Failed`
/// reason string), in which case the caller falls back to the raw output.
fn agent_content_from_spool(spool: &str) -> Option<serde_json::Value> {
    let trimmed = spool.trim_end();
    // Drop the appended `<usage>…</usage>` footer if it is present, keeping the
    // pretty-printed JSON body that precedes it.
    let body = match trimmed.rfind("\n<usage>") {
        Some(idx) => &trimmed[..idx],
        None => trimmed,
    };
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(body).ok()
}

fn task_err_to_registry_err(e: TaskError) -> TaskRegistryError {
    match e {
        TaskError::NotFound(id) => TaskRegistryError::NotFound(id),
        TaskError::UnknownType => TaskRegistryError::InvalidInput("unknown task type".into()),
        TaskError::TerminatedTask => TaskRegistryError::Internal("task already terminated".into()),
        TaskError::Unsupported => TaskRegistryError::Internal("unsupported".into()),
        TaskError::Io(s) => TaskRegistryError::Internal(format!("io: {s}")),
        TaskError::Internal(s) => TaskRegistryError::Internal(s),
    }
}

fn placeholder_input(task_type: TaskType) -> TaskSpawnInput {
    // Most spawn inputs require fields a `TaskCreate` JSON call cannot supply
    // (e.g. `LocalBash` needs `command`). The registry's `create` only
    // allocates the spool file + state slot, so the input shape is unused
    // in production for the M4-05 tool dispatch path. We pick a minimal
    // variant that matches the task_type so the create call type-checks.
    match task_type {
        TaskType::LocalBash => TaskSpawnInput::LocalBash {
            command: String::new(),
            timeout: None,
            tool_use_id: None,
        },
        TaskType::LocalAgent => TaskSpawnInput::LocalAgent {
            agent_id: protocol::AgentId::nil(),
            subagent_type: String::new(),
            prompt: String::new(),
            is_backgrounded: false,
            tool_use_id: None,
            creator_teammate_name: None,
            creator_team_name: None,
            creator_agent_id: None,
            spawn_request: None,
            inheritance: None,
        },
        TaskType::RemoteAgent => TaskSpawnInput::RemoteAgent {
            endpoint: String::new(),
            prompt: String::new(),
        },
        TaskType::InProcessTeammate => TaskSpawnInput::InProcessTeammate {
            spawn_request: None,
            inheritance: None,
            agent_id: protocol::AgentId::nil(),
            name: String::new(),
            team_name: String::new(),
            description: String::new(),
        },
        TaskType::LocalWorkflow => TaskSpawnInput::LocalWorkflow {
            session_uuid: None,
            workflow_id: String::new(),
            script: String::new(),
            resume_from_run_id: None,
            args: None,
            run_id: None,
            invocation_mode: None,
            workflow_source: None,
            script_is_verbatim_builtin: None,
            transcript_subdir: None,
            launched_from_subagent: false,
            tool_use_id: None,
            creator_teammate_name: None,
            creator_team_name: None,
            creator_agent_id: None,
            // A placeholder carries no Local App authority: `create` only
            // allocates a spool file and a state slot, and the Host mints a
            // scope on the real `spawn` path.
            scope: None,
        },
        TaskType::MonitorMcp => TaskSpawnInput::MonitorMcp {
            server_name: String::new(),
            watch: vec![],
        },
        TaskType::Monitor => TaskSpawnInput::Monitor {
            command: String::new(),
            spawn_command: None,
            timeout: None,
            cwd: None,
            tool_use_id: None,
            creator_teammate_name: None,
            creator_team_name: None,
            creator_agent_id: None,
        },
        TaskType::McpTask => TaskSpawnInput::McpTask {
            server_name: String::new(),
            tool_name: String::new(),
            tool_use_id: None,
            creator_teammate_name: None,
            creator_team_name: None,
            creator_agent_id: None,
        },
        TaskType::AutoModeScan => TaskSpawnInput::AutoModeScan,
        TaskType::Dream => TaskSpawnInput::Dream {
            prompt: String::new(),
            max_iterations: None,
        },
        TaskType::LocalFusion => TaskSpawnInput::LocalFusion {
            request: platform_api::FusionRequest {
                schema_version: platform_api::FUSION_SCHEMA_VERSION,
                origin: platform_api::FusionOrigin::Slash,
                prompt: String::new(),
                preset: platform_api::FusionPreset::Quality,
                models: None,
                dimensions: Vec::new(),
                partial_ok: true,
                max_panel: None,
                cross_provider: true,
                parent_profile: String::new(),
                parent_model: String::new(),
            },
            conversation_id: String::new(),
        },
    }
}

#[async_trait]
impl TaskRegistryHandle for TaskRegistry {
    async fn send_human_task_message(
        &self,
        id: &str,
        message: &str,
    ) -> Result<(), TaskRegistryError> {
        TaskRegistry::send_human_task_message(self, id, message)
            .await
            .map_err(|error| TaskRegistryError::Internal(error.to_string()))
    }
    async fn take_human_task_messages_for(&self, id: protocol::AgentId) -> Vec<String> {
        TaskRegistry::take_human_task_messages_for(self, id).await
    }
    async fn begin_human_task_resume(&self, id: &str, epoch: u64) -> Result<(), TaskRegistryError> {
        TaskRegistry::begin_human_task_resume(self, id, epoch)
            .await
            .map_err(|error| TaskRegistryError::Internal(error.to_string()))
    }
    async fn register_agent_resume_recipe(
        &self,
        id: &str,
        request: platform_api::SubagentSpawnRequest,
        inheritance: platform_api::SubagentInheritance,
    ) -> Result<(), TaskRegistryError> {
        TaskRegistry::register_agent_resume_recipe(self, id, request, inheritance)
            .await
            .map_err(|error| TaskRegistryError::Internal(error.to_string()))
    }

    async fn export_shell_handoff(
        &self,
    ) -> Result<Vec<platform_api::shell_handoff::ShellTaskHandoff>, TaskRegistryError> {
        TaskRegistry::export_shell_handoff(self)
            .await
            .map_err(|error| TaskRegistryError::Internal(error.to_string()))
    }
    async fn prepare_shell_handoff(
        &self,
        records: &[platform_api::shell_handoff::ShellTaskHandoff],
    ) -> Result<(), TaskRegistryError> {
        TaskRegistry::prepare_shell_handoff(self, records)
            .await
            .map_err(|error| TaskRegistryError::Internal(error.to_string()))
    }
    async fn commit_shell_handoff(&self, ids: &[String]) -> Result<Vec<String>, TaskRegistryError> {
        TaskRegistry::commit_shell_handoff(self, ids)
            .await
            .map_err(|error| TaskRegistryError::Internal(error.to_string()))
    }
    async fn adopt_shell_handoff(
        &self,
        records: &[platform_api::shell_handoff::ShellTaskHandoff],
    ) -> Result<(), TaskRegistryError> {
        TaskRegistry::adopt_shell_handoff(self, records)
            .await
            .map_err(|error| TaskRegistryError::Internal(error.to_string()))
    }
    async fn rollback_shell_handoff(
        &self,
        records: &[platform_api::shell_handoff::ShellTaskHandoff],
    ) -> Result<(), TaskRegistryError> {
        TaskRegistry::rollback_shell_handoff(self, records)
            .await
            .map_err(|error| TaskRegistryError::Internal(error.to_string()))
    }
    fn add_permission_paused_ms(&self, agent_id: protocol::AgentId, milliseconds: u64) {
        self.record_permission_pause(agent_id, milliseconds);
    }

    async fn link_agent_output(
        &self,
        id: &str,
        target: &std::path::Path,
    ) -> Result<(), TaskRegistryError> {
        let state = self
            .get(id)
            .await
            .ok_or_else(|| TaskRegistryError::NotFound(id.into()))?;
        self.output_manager
            .link_transcript(&state.base().output_file, target)
            .await
            .map_err(|e| TaskRegistryError::Internal(e.to_string()))
    }

    async fn observe_agent_activity(
        &self,
        request: platform_api::SubagentSpawnRequest,
        inheritance: platform_api::SubagentInheritance,
        observed_agent_id: protocol::AgentId,
        digest: String,
        seed: Option<platform_api::observer_pairing::ObserverPairingSeed>,
    ) -> Result<(), TaskRegistryError> {
        TaskRegistry::observe_agent_activity(
            self,
            request,
            inheritance,
            observed_agent_id,
            digest,
            seed,
        )
        .await
        .map_err(|error| TaskRegistryError::Internal(error.to_string()))
    }
    async fn task_output_directory(&self) -> Option<String> {
        Some(
            self.output_manager
                .output_dir()
                .to_string_lossy()
                .into_owned(),
        )
    }

    // Per-session subagent-spawn counter (claude 2.1.212 `getTotalAgentSpawns` /
    // `incrementTotalAgentSpawns`) — delegate the trait surface the `Agent` tool
    // reads to the concrete registry's atomic counter.
    fn get_total_agent_spawns(&self) -> u64 {
        TaskRegistry::total_agent_spawns(self)
    }

    fn increment_total_agent_spawns(&self) {
        TaskRegistry::increment_total_agent_spawns(self);
    }

    fn try_reserve_total_agent_spawn(&self, cap: u64) -> Result<u64, u64> {
        TaskRegistry::try_reserve_total_agent_spawn(self, cap)
    }

    fn release_total_agent_spawn_reservation(&self) {
        TaskRegistry::release_total_agent_spawn_reservation(self);
    }

    fn try_reserve_total_agent_spawns(&self, n: u64, cap: u64) -> Result<u64, u64> {
        TaskRegistry::try_reserve_total_agent_spawns(self, n, cap)
    }

    fn release_total_agent_spawn_reservations(&self, n: u64) {
        TaskRegistry::release_total_agent_spawn_reservations(self, n);
    }

    async fn create(&self, input: TaskCreateInput) -> Result<TaskRecord, TaskRegistryError> {
        let task_type = task_type_from_wire(&input.task_type)?;
        let id = self
            .create(
                task_type,
                placeholder_input(task_type),
                input.description.clone(),
            )
            .await
            .map_err(task_err_to_registry_err)?;
        Ok(TaskRecord {
            task_id: id,
            task_type: input.task_type,
            status: "pending".into(),
            description: input.description,
            // `create` allocates a placeholder spawn input with an empty command
            // (`placeholder_input`), so no real command is available at this point;
            // a `local_bash` record gets its command once `state_to_record` reads
            // the populated bash state. `None` here matches the pre-spawn shape.
            command: None,
            // The per-type hook-payload extras are likewise unpopulated at the
            // placeholder-create point; they fill in once `state_to_record` reads
            // a real spawned state.
            ..Default::default()
        })
    }

    async fn resolve_stop_target(
        &self,
        requested: &str,
        named_agents: &[(String, String)],
    ) -> Result<platform_api::task_registry::TaskStopResolution, TaskRegistryError> {
        let records = <Self as TaskRegistryHandle>::list(self, TaskListFilter::default()).await?;
        // The `Agent` tool publishes the AGENT uuid to the model and instructs it
        // to use that id (`tools/agent/src/agent.rs:822`); an async agent's task
        // is registered under both uuid spellings as aliases
        // (`registry.rs:5356-5357`). `TaskOutput` resolves them because
        // `registry.get` runs `canonical_or_raw`, but
        // `crate::resolve::resolve_stop_target` matches only `task_id`,
        // `teammate_agent_id` and named agents — so the SAME id the model was
        // told to use worked for TaskOutput and failed for TaskStop.
        //
        // Try the alias map first; a real task id still wins, because
        // `resolve_task_id` checks the task map before the alias map.
        if let Some(task_id) = self.resolve_task_id(requested).await {
            if let Some(record) = records.iter().find(|r| r.task_id == task_id) {
                return Ok(platform_api::task_registry::TaskStopResolution::Found(
                    record.clone(),
                ));
            }
        }
        let mut canonical_names = Vec::new();
        for (name, id) in named_agents {
            if let Some(state) = self.get(id).await {
                canonical_names.push((name.clone(), state.base().id.clone()));
            }
        }
        Ok(crate::resolve::resolve_stop_target(
            requested,
            records,
            &canonical_names,
        ))
    }

    async fn get(&self, id: &str) -> Result<Option<TaskRecord>, TaskRegistryError> {
        Ok(self.get(id).await.as_ref().map(state_to_record))
    }

    async fn list(&self, filter: TaskListFilter) -> Result<Vec<TaskRecord>, TaskRegistryError> {
        let want_status = match filter.status.as_deref() {
            Some(s) => Some(status_from_wire(s)?),
            None => None,
        };
        let mut out = Vec::new();
        for state in self.list().await {
            if !self.workflow_visible_in_current_session(&state) {
                continue;
            }
            if let Some(want) = want_status {
                if state.base().status != want {
                    continue;
                }
            }
            let mut record = state_to_record(&state);
            record.completed_agent_visible = self.completed_agent_visible(&record.task_id).await;
            out.push(record);
        }
        Ok(out)
    }

    /// Rich `local_workflow` projection for the `/workflows` picker: surfaces the
    /// `wf_…` run id, current phase step, and start/end wall-clock (epoch millis)
    /// straight from [`LocalWorkflowTaskState`], which the reduced [`TaskRecord`]
    /// drops. Order is the registry's `HashMap` order (unspecified); the caller
    /// (`cmd_workflows`) sorts newest-first for display.
    async fn list_workflows(&self) -> Result<Vec<WorkflowRecord>, TaskRegistryError> {
        fn epoch_ms(t: std::time::SystemTime) -> Option<u64> {
            t.duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        }
        let mut out = Vec::new();
        for state in self.list().await {
            if !self.workflow_visible_in_current_session(&state) {
                continue;
            }
            if let TaskState::LocalWorkflow(w) = &state {
                out.push(WorkflowRecord {
                    task_id: w.base.id.clone(),
                    run_id: w.run_id.clone(),
                    name: w.workflow_id.clone(),
                    status: status_to_wire(w.base.status).to_string(),
                    description: w.base.description.clone(),
                    current_step: w.current_step,
                    started_at_ms: epoch_ms(w.base.start_time),
                    ended_at_ms: w.base.end_time.and_then(epoch_ms),
                    script: (!w.script.is_empty()).then(|| w.script.clone()),
                    script_path: w.script_path.clone(),
                    args: w.args.clone(),
                    agent_count: w.outcome.agent_count,
                    total_tokens: w.outcome.total_tokens,
                });
            }
        }
        Ok(out)
    }

    async fn update(
        &self,
        id: &str,
        patch: TaskUpdatePatch,
    ) -> Result<TaskRecord, TaskRegistryError> {
        if let Some(ref s) = patch.status {
            return <Self as TaskRegistryHandle>::set_status(self, id, s).await;
        }
        // No-op patch — just return current state.
        match self.get(id).await {
            Some(state) => Ok(state_to_record(&state)),
            None => Err(TaskRegistryError::NotFound(id.into())),
        }
    }

    async fn set_status(&self, id: &str, status: &str) -> Result<TaskRecord, TaskRegistryError> {
        let s = status_from_wire(status)?;
        let state = self
            .set_status(id, s)
            .await
            .map_err(task_err_to_registry_err)?;
        Ok(state_to_record(&state))
    }

    async fn set_exit_code(&self, id: &str, exit_code: i32) -> Result<(), TaskRegistryError> {
        // Shell-backed worker exit-code write-through — see
        // `TaskRegistry::set_bash_exit_code`.
        self.set_bash_exit_code(id, exit_code)
            .await
            .map_err(task_err_to_registry_err)
    }

    async fn set_workflow_outcome(
        &self,
        id: &str,
        outcome: platform_api::task_registry::WorkflowTerminalOutcome,
    ) {
        TaskRegistry::set_workflow_outcome(self, id, outcome).await;
    }

    async fn has_live_task_loop(&self, id: &str) -> bool {
        TaskRegistry::has_live_task_loop(self, id).await
    }

    async fn process_owners_for_task(&self, id: &str) -> Vec<String> {
        TaskRegistry::process_owner_ids(self, id)
            .await
            .into_iter()
            .map(|id| id.to_string())
            .collect()
    }

    async fn has_pending_teammate_departure(&self, id: &str) -> bool {
        TaskRegistry::has_pending_teammate_departure(self, id).await
    }

    async fn kill(&self, id: &str) -> Result<TaskRecord, TaskRegistryError> {
        self.kill(id).await.map_err(task_err_to_registry_err)?;
        // After kill, fetch the (now-killed) state for the record.
        match self.get(id).await {
            Some(state) => Ok(state_to_record(&state)),
            None => Err(TaskRegistryError::NotFound(id.into())),
        }
    }

    async fn kill_with_reason(
        &self,
        id: &str,
        killed_by: &str,
    ) -> Result<TaskRecord, TaskRegistryError> {
        TaskRegistry::kill_with_reason(self, id, killed_by)
            .await
            .map_err(task_err_to_registry_err)?;
        match self.get(id).await {
            Some(state) => Ok(state_to_record(&state)),
            None => Err(TaskRegistryError::NotFound(id.into())),
        }
    }

    async fn set_agent_outcome(
        &self,
        id: &str,
        outcome: platform_api::task_registry::AgentTerminalOutcome,
    ) {
        TaskRegistry::set_agent_outcome(self, id, outcome).await;
    }

    async fn spawn_websocket_monitor(
        &self,
        registration: platform_api::task_registry::WebSocketMonitorRegistration,
        http: std::sync::Arc<dyn platform_api::HttpTransport>,
    ) -> Result<String, TaskRegistryError> {
        let description = registration.task.description.clone();
        TaskRegistry::spawn(
            self,
            TaskType::Monitor,
            TaskSpawnInput::MonitorWs(crate::task_trait::WebSocketMonitorInput {
                registration,
                http,
            }),
            description,
        )
        .await
        .map_err(task_err_to_registry_err)
    }

    async fn spawn_monitor(&self, reg: MonitorRegistration) -> Result<String, TaskRegistryError> {
        let timeout = if reg.persistent || reg.timeout_ms == 0 {
            None
        } else {
            Some(std::time::Duration::from_millis(reg.timeout_ms))
        };
        TaskRegistry::spawn(
            self,
            TaskType::Monitor,
            TaskSpawnInput::Monitor {
                command: reg.command,
                spawn_command: reg.spawn_command,
                timeout,
                cwd: reg.cwd.map(PathBuf::from),
                tool_use_id: reg.tool_use_id,
                creator_teammate_name: reg.creator_teammate_name,
                creator_team_name: reg.creator_team_name,
                creator_agent_id: reg.creator_agent_id,
            },
            reg.description,
        )
        .await
        .map_err(task_err_to_registry_err)
    }

    async fn notify_monitor_event(&self, id: &str, event: &str, housekeeping: bool) {
        let _ = TaskRegistry::enqueue_monitor_event(self, id, event, housekeeping).await;
    }

    async fn register_mcp_task(
        &self,
        reg: platform_api::task_registry::McpTaskRegistration,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<String, TaskRegistryError> {
        TaskRegistry::register_mcp_task_owned(
            self,
            reg.server_name,
            reg.tool_name,
            reg.tool_use_id,
            reg.creator_teammate_name,
            reg.creator_team_name,
            reg.creator_agent_id,
            cancel,
        )
        .await
        .map_err(task_err_to_registry_err)
    }

    async fn settle_mcp_task_with_hint(
        &self,
        id: &str,
        text: &str,
        failed: bool,
        saved_hint: Option<&str>,
    ) -> Result<bool, TaskRegistryError> {
        TaskRegistry::settle_mcp_task_with_hint(self, id, text, failed, saved_hint)
            .await
            .map_err(task_err_to_registry_err)
    }

    async fn settle_mcp_task(
        &self,
        id: &str,
        result_text: &str,
        failed: bool,
    ) -> Result<bool, TaskRegistryError> {
        TaskRegistry::settle_mcp_task(self, id, result_text, failed)
            .await
            .map_err(task_err_to_registry_err)
    }

    async fn output(
        &self,
        id: &str,
        offset: Option<u64>,
    ) -> Result<TaskOutputChunk, TaskRegistryError> {
        let state = self
            .get(id)
            .await
            .ok_or_else(|| TaskRegistryError::NotFound(id.into()))?;
        let status = state.base().status;
        // Mirror TS `bashTask.result?.code ?? null`: only local-bash tasks carry
        // a process exit code; other task types report `None`.
        let exit_code = match &state {
            TaskState::LocalBash(s) => s.exit_code,
            TaskState::Monitor(s) => s.exit_code,
            _ => None,
        };
        // Mirror the TS poll predicate `status !== 'running' && status !==
        // 'pending'`. A paused workflow is complete for output retrieval even
        // though it is intentionally non-terminal in the task lifecycle.
        let done = !matches!(status, TaskStatus::Pending | TaskStatus::Running);
        let output_file = state.base().output_file.clone();
        let opts = crate::output_manager::OutputOptions {
            offset,
            limit: None,
        };
        let out = self
            .output_manager
            .read(&output_file, opts)
            .await
            .map_err(|e| match e {
                crate::output_manager::OutputError::Io(s) => {
                    TaskRegistryError::Internal(format!("io: {s}"))
                }
                crate::output_manager::OutputError::PathEscape(p) => {
                    TaskRegistryError::Internal(format!("path escape: {p}"))
                }
                // Unreachable on the read path (exclusive-create only fires in
                // `allocate`), but the match must stay exhaustive.
                crate::output_manager::OutputError::AlreadyExists(p) => {
                    TaskRegistryError::Internal(format!("spool already allocated: {p}"))
                }
                crate::output_manager::OutputError::SwapRefused(reason) => {
                    TaskRegistryError::Internal(reason)
                }
            })?;
        // Agent-specific fields (TS `getTaskOutputData` `local_agent` branch,
        // `TaskOutputTool.tsx:91-106`): carry the agent's `error` + `prompt`
        // through, and extract the CLEAN final answer from the spooled
        // transcript so the model sees the answer (not the raw JSON blob).
        // `error`/`prompt`/`result` stay `None` for non-agent task types
        // (matching the TS shape, where only `local_agent` adds these keys).
        let (error, prompt, result) = match &state {
            TaskState::LocalAgent(a) => {
                let clean = a
                    .outcome
                    .result
                    .clone()
                    .filter(|s| !s.is_empty())
                    .or_else(|| {
                        agent_content_from_spool(&out.content)
                            .map(|content| extract_text_content(&content))
                            .filter(|s| !s.is_empty())
                    });
                (a.error.clone(), Some(a.prompt.clone()), clean)
            }
            _ => (None, None, None),
        };
        let output_path = out
            .physical_spool_authoritative
            .then(|| output_file.to_str().map(str::to_string))
            .flatten();
        // TO-06: an `mcp_task`'s model-facing output is a SYNTHETIC metadata
        // block, not the spool (claude-code `getTaskOutputData`'s `mcp_task`
        // branch). The registry owns the two timestamps the block's `elapsed:`
        // line needs, so the elapsed value is computed here rather than at the
        // tool, which has neither.
        let mcp = match &state {
            TaskState::McpTask(mcp) => {
                let end = mcp.base.end_time.unwrap_or_else(std::time::SystemTime::now);
                let elapsed_ms = end
                    .duration_since(mcp.base.start_time)
                    .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
                    // `Math.max(0, …)`: a clock that went backwards reads as 0.
                    .unwrap_or(0);
                Some(platform_api::task_registry::McpTaskOutputMeta {
                    server_name: mcp.server_name.clone(),
                    tool_name: mcp.tool_name.clone(),
                    mcp_status: mcp.mcp_status.clone(),
                    status_message: mcp.status_message.clone(),
                    elapsed_ms,
                })
            }
            _ => None,
        };
        let harness_head = match &state {
            TaskState::LocalAgent(agent) if status != TaskStatus::Killed => {
                agent.outcome.max_turns_reached.map(|limit| {
                    platform_api::subagent_output::max_turns_harness_note(
                        limit,
                        &agent.subagent_type,
                        result.as_ref().is_some_and(|text| !text.is_empty()),
                    )
                })
            }
            _ => None,
        };
        // Recheck after the filesystem await. Export fences a running shell
        // under the task-row lock; a completion that arrived during this read
        // must stay waiting until transfer commits or rolls back. A terminal
        // row cannot become newly exportable after this check.
        let effective = self
            .output_completion_status(id)
            .await
            .map_err(task_err_to_registry_err)?;
        let waiting_for_transfer =
            matches!(&state, TaskState::LocalBash(_)) && effective == TaskStatus::Running;
        let status = if waiting_for_transfer {
            TaskStatus::Running
        } else {
            status
        };
        let done = done && !waiting_for_transfer;
        let exit_code = if waiting_for_transfer {
            None
        } else {
            exit_code
        };
        Ok(TaskOutputChunk {
            harness_head,
            task_id: state.base().id.clone(),
            content: out.content,
            total_lines: out.total_lines,
            truncated: out.truncated,
            status: Some(status_to_wire(status).to_string()),
            exit_code,
            done,
            error,
            prompt,
            result,
            // Surface the absolute spool path only while its bytes agree with
            // the authoritative projection. A terminal in-memory override can
            // remain authoritative after a best-effort disk rewrite fails.
            output_path,
            mcp,
        })
    }

    async fn allocate_bash_output(
        &self,
    ) -> Result<platform_api::task_registry::BackgroundBashHandle, TaskRegistryError> {
        let (task_id, path) = TaskRegistry::allocate_bash_output(self)
            .await
            .map_err(task_err_to_registry_err)?;
        let output_path = path.to_str().map(str::to_string).ok_or_else(|| {
            TaskRegistryError::Internal("task output path is not valid UTF-8".into())
        })?;
        Ok(platform_api::task_registry::BackgroundBashHandle {
            task_id,
            output_path,
        })
    }

    async fn register_background_bash(
        &self,
        task_id: &str,
        registration: platform_api::task_registry::BackgroundBashRegistration,
    ) -> Result<(), TaskRegistryError> {
        TaskRegistry::register_background_bash(
            self,
            task_id.to_string(),
            registration.command,
            registration.description,
            registration.tool_use_id,
            registration.cwd,
            registration.creator_agent_id,
        )
        .await
        .map(|_| ())
        .map_err(task_err_to_registry_err)
    }

    async fn discard_bash_output(&self, task_id: &str) {
        TaskRegistry::discard_bash_output(self, task_id).await;
    }

    async fn bind_background_killer(
        &self,
        id: &str,
        killer: std::sync::Arc<dyn platform_api::task_registry::TaskKiller>,
    ) -> Result<(), TaskRegistryError> {
        TaskRegistry::bind_background_killer(self, id, killer)
            .await
            .map_err(task_err_to_registry_err)
    }

    async fn bind_background_process(
        &self,
        id: &str,
        pid: u32,
        killer: std::sync::Arc<dyn platform_api::task_registry::TaskKiller>,
    ) -> Result<(), TaskRegistryError> {
        TaskRegistry::bind_background_bash_process(self, id, Some(pid), killer)
            .await
            .map_err(task_err_to_registry_err)
    }

    async fn kill_background_shells_for_agent(&self, agent_id: protocol::AgentId) -> usize {
        TaskRegistry::kill_background_shells_for_agent(self, agent_id).await
    }

    async fn bind_agent_message_receiver(
        &self,
        id: &str,
        receiver: std::sync::Arc<dyn platform_api::task_registry::TaskMessageReceiver>,
    ) -> Result<(), TaskRegistryError> {
        TaskRegistry::bind_agent_message_receiver(self, id, receiver)
            .await
            .map_err(task_err_to_registry_err)
    }

    async fn set_agent_display(&self, id: &str, model: String, effort: Option<String>) {
        TaskRegistry::set_agent_display(self, id, model, effort).await;
    }

    async fn register_foreground_agent(
        &self,
        registration: platform_api::task_registry::ForegroundAgentRegistration,
    ) -> Result<platform_api::task_registry::ForegroundAgentHandle, TaskRegistryError> {
        TaskRegistry::register_foreground_agent(self, registration)
            .await
            .map(|state| platform_api::task_registry::ForegroundAgentHandle {
                task_id: state.base().id.clone(),
                output_path: state.base().output_file.to_string_lossy().into_owned(),
            })
            .map_err(task_err_to_registry_err)
    }

    async fn unregister_foreground_agent(&self, id: &str) {
        TaskRegistry::unregister_foreground_agent(self, id).await;
    }

    async fn register_foreground_bash(
        &self,
        task_id: &str,
        registration: platform_api::task_registry::BackgroundBashRegistration,
        auto_background_armed: bool,
    ) -> Result<(), TaskRegistryError> {
        TaskRegistry::register_foreground_bash(self, task_id, registration, auto_background_armed)
            .await
            .map_err(task_err_to_registry_err)
    }

    async fn unregister_foreground_bash(&self, task_id: &str) {
        TaskRegistry::unregister_foreground_bash(self, task_id).await;
    }

    async fn bind_background_requester(
        &self,
        task_id: &str,
        requester: std::sync::Arc<dyn platform_api::task_registry::TaskBackgrounder>,
    ) -> Result<(), TaskRegistryError> {
        TaskRegistry::bind_background_requester(self, task_id, requester)
            .await
            .map_err(task_err_to_registry_err)
    }

    async fn background_task(&self, task_id: &str) -> bool {
        TaskRegistry::background_task(self, task_id).await
    }

    async fn not_found_rosters(
        &self,
        caller_agent_id: Option<&str>,
        named_agent_ids: &[String],
    ) -> platform_api::task_registry::TaskNotFoundRosters {
        TaskRegistry::not_found_rosters(self, caller_agent_id, named_agent_ids).await
    }

    async fn background_task_for_tool_use(&self, tool_use_id: &str) -> bool {
        TaskRegistry::background_task_for_tool_use(self, tool_use_id).await
    }

    async fn background_all_tasks(&self) -> usize {
        TaskRegistry::background_all_tasks(self).await
    }
    async fn background_all_tasks_with_reason(
        &self,
        reason: platform_api::task_registry::TaskBackgroundReason,
    ) -> usize {
        TaskRegistry::background_all_tasks_with_reason(self, reason).await
    }

    async fn has_backgroundable_tasks(&self) -> bool {
        TaskRegistry::has_backgroundable_tasks(self).await
    }

    async fn append_bash_output(
        &self,
        task_id: &str,
        content: &str,
    ) -> Result<(), TaskRegistryError> {
        let path = self
            .output_manager
            .path_for(task_id)
            .map_err(|error| TaskRegistryError::Internal(error.to_string()))?;
        self.output_manager
            .append(&path, content)
            .await
            .map_err(|error| TaskRegistryError::Internal(error.to_string()))
    }

    async fn finalize_persisted_output(
        &self,
        id: &str,
        max_bytes: u64,
    ) -> Result<Option<u64>, TaskRegistryError> {
        let path = self
            .output_manager
            .path_for(id)
            .map_err(|error| TaskRegistryError::Internal(error.to_string()))?;
        self.output_manager
            .finalize_persisted_output(&path, max_bytes)
            .await
            .map(Some)
            .map_err(|error| TaskRegistryError::Internal(error.to_string()))
    }

    async fn flush_bash_output(&self, task_id: &str) -> Result<(), TaskRegistryError> {
        let path = self
            .output_manager
            .path_for(task_id)
            .map_err(|error| TaskRegistryError::Internal(error.to_string()))?;
        self.output_manager
            .flush_writer(&path)
            .await
            .map_err(|error| TaskRegistryError::Internal(error.to_string()))
    }

    fn subscribe_task_lifecycle(
        &self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>> {
        Some(TaskRegistry::subscribe_task_lifecycle(self))
    }

    fn update_shell_session_activity(&self, interactive: bool, busy: bool, user_interaction: bool) {
        TaskRegistry::update_shell_session_activity(self, interactive, busy, user_interaction);
    }

    async fn notify_bash_stall(&self, task_id: &str, tail: &str) {
        TaskRegistry::notify_bash_stall(self, task_id, tail).await;
    }

    async fn claim_bash_memory_pressure_stop(&self, task_id: &str) -> bool {
        TaskRegistry::claim_bash_memory_pressure_stop(self, task_id).await
    }

    async fn mark_shell_supervised(&self, id: &str) {
        TaskRegistry::mark_shell_supervised(self, id).await;
    }

    async fn settle_background_bash(
        &self,
        id: &str,
        exit_code: Option<i32>,
        killed: bool,
    ) -> Result<(), TaskRegistryError> {
        TaskRegistry::settle_background_bash(self, id, exit_code, killed)
            .await
            .map_err(task_err_to_registry_err)
    }

    async fn mark_notified(&self, id: &str) -> Result<(), TaskRegistryError> {
        // Dispatch to the inherent `TaskRegistry::mark_notified`, which sets the
        // `notified` flag while keeping the task addressable.
        TaskRegistry::mark_notified(self, id)
            .await
            .map_err(task_err_to_registry_err)
    }

    async fn mark_rested(
        &self,
        id: &str,
        result: Option<String>,
        usage: Option<platform_api::task_registry::AgentRunUsage>,
    ) {
        // Dispatch to the inherent arm-rest path (no-op for unknown/terminal).
        TaskRegistry::mark_task_rested(self, id, result, usage, None, None, None).await;
    }

    async fn take_pending_task_notifications(
        &self,
    ) -> Result<Vec<platform_api::task_registry::TaskNotification>, TaskRegistryError> {
        // Dispatch to the inherent drain, which snapshots + marks-notified +
        // retains the terminal-not-notified tasks. Infallible at the registry
        // level (the lock is always acquirable), so the seam result is always
        // `Ok`.
        Ok(TaskRegistry::take_pending_task_notifications(self).await)
    }

    async fn park_foreground_agent(
        &self,
        agent_id: protocol::AgentId,
        outcome: platform_api::task_registry::AgentTerminalOutcome,
    ) -> bool {
        TaskRegistry::park_foreground_agent(self, agent_id, outcome).await
    }

    async fn can_wake_agent_for_task_notification(&self, agent_id: protocol::AgentId) -> bool {
        for state in TaskRegistry::list(self).await {
            match state {
                TaskState::LocalAgent(agent) if agent.agent_id == agent_id => {
                    return agent.is_parked
                }
                TaskState::InProcessTeammate(agent) if agent.agent_id == agent_id => {
                    return agent.is_idle
                }
                _ => {}
            }
        }
        true
    }

    async fn activate_agent_for_task_notification(&self, agent_id: protocol::AgentId) {
        for state in TaskRegistry::list(self).await {
            let resting_owner = match &state {
                TaskState::LocalAgent(agent) => agent.agent_id == agent_id && agent.is_parked,
                TaskState::InProcessTeammate(agent) => agent.agent_id == agent_id && agent.is_idle,
                _ => false,
            };
            if resting_owner {
                let _ = TaskRegistry::set_status(self, &state.base().id, TaskStatus::Running).await;
            }
        }
    }

    async fn publish_task_stopped(&self, task_id: &str) {
        TaskRegistry::publish_task_stopped(self, task_id).await;
    }

    fn subscribe_task_notifications(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        Some(TaskRegistry::subscribe_task_notifications(self))
    }

    async fn has_pending_task_notifications_for(
        &self,
        recipient: Option<protocol::AgentId>,
    ) -> bool {
        TaskRegistry::has_pending_task_notifications_for(self, recipient).await
    }

    async fn take_pending_task_notifications_for(
        &self,
        recipient: Option<protocol::AgentId>,
    ) -> Result<Vec<platform_api::task_registry::TaskNotification>, TaskRegistryError> {
        Ok(TaskRegistry::take_pending_task_notifications_for(self, recipient).await)
    }

    fn web_search_calls(&self) -> u32 {
        // Delegate to the inherent atomic-load method (`getWebSearchCalls(){return
        // n}`). Explicit `TaskRegistry::` path selects the inherent method over the
        // trait default, so the `WebSearch` tool observes the real session counter
        // through the seam rather than the null-registry `0` stub.
        TaskRegistry::web_search_calls(self)
    }

    fn increment_web_search_calls(&self) {
        // `incrementWebSearchCalls(){n++}` — the inherent atomic fetch-add.
        TaskRegistry::increment_web_search_calls(self);
    }

    fn reset_web_search_calls(&self) {
        // `resetWebSearchCalls(){n=0}` — the inherent atomic store.
        TaskRegistry::reset_web_search_calls(self);
    }
}

#[cfg(test)]
#[allow(
    clippy::cast_possible_truncation,
    clippy::map_unwrap_or,
    clippy::unwrap_used
)]
mod tests {
    use super::*;
    use crate::output_manager::TaskOutputManager;
    use async_trait::async_trait;
    use platform_api::filesystem::{FileContent, FileEvent, FileSystem, FlockGuard, FsError};
    use std::path::PathBuf;
    use std::sync::Arc;
    use tempfile::tempdir;
    use test_harness::mocks::MockRuntimeSpawner;

    use std::collections::HashMap;
    use tokio::sync::Mutex as TokioMutex;

    /// In-memory `FileSystem` that actually preserves writes — used by the
    /// M5-01 Task 6 `output_returns_real_content_after_spool_write` test.
    struct InMemoryFs {
        files: TokioMutex<HashMap<String, String>>,
    }

    impl InMemoryFs {
        fn new() -> Self {
            Self {
                files: TokioMutex::new(HashMap::new()),
            }
        }
    }

    #[async_trait]
    impl FileSystem for InMemoryFs {
        async fn read_file(
            &self,
            path: &str,
            offset: Option<u64>,
            limit: Option<u64>,
        ) -> Result<FileContent, FsError> {
            let map = self.files.lock().await;
            let content = map.get(path).cloned().unwrap_or_default();
            let off = offset.unwrap_or(0) as usize;
            let body: String = content.chars().skip(off).collect();
            let truncated = if let Some(lim) = limit {
                body.len() as u64 > lim
            } else {
                false
            };
            let trimmed = if let Some(lim) = limit {
                body.chars().take(lim as usize).collect::<String>()
            } else {
                body
            };
            let total_lines = content.lines().count() as u64;
            Ok(FileContent {
                content: trimmed,
                truncated,
                total_lines,
            })
        }
        async fn write_file(&self, path: &str, body: &str) -> Result<(), FsError> {
            self.files
                .lock()
                .await
                .insert(path.to_string(), body.to_string());
            Ok(())
        }
        fn is_within_workspace(&self, _: &str) -> bool {
            true
        }
        async fn watch(
            &self,
            _: &str,
        ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = FileEvent> + Send>>, FsError>
        {
            Err(FsError::Io("not supported".into()))
        }
        async fn append_file(&self, path: &str, body: &str) -> Result<(), FsError> {
            let mut map = self.files.lock().await;
            let entry = map.entry(path.to_string()).or_default();
            entry.push_str(body);
            Ok(())
        }
        async fn truncate(&self, path: &str, len: u64) -> Result<(), FsError> {
            let mut map = self.files.lock().await;
            if let Some(s) = map.get_mut(path) {
                s.truncate(len as usize);
            }
            Ok(())
        }
        async fn file_mtime(&self, _: &str) -> Result<std::time::SystemTime, FsError> {
            Ok(std::time::SystemTime::UNIX_EPOCH)
        }
        async fn file_size(&self, path: &str) -> Result<u64, FsError> {
            let map = self.files.lock().await;
            Ok(map.get(path).map(|s| s.len() as u64).unwrap_or(0))
        }
        async fn delete_file(&self, path: &str) -> Result<(), FsError> {
            self.files.lock().await.remove(path);
            Ok(())
        }
        async fn symlink(&self, _: &str, _: &str) -> Result<(), FsError> {
            Ok(())
        }
        async fn flock_exclusive(&self, _: &str) -> Result<Box<dyn FlockGuard>, FsError> {
            Err(FsError::Io("not supported".into()))
        }
        async fn fsync(&self, _: &str) -> Result<(), FsError> {
            Ok(())
        }
    }

    fn make_registry() -> (tempfile::TempDir, Arc<TaskRegistry>) {
        let dir = tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let runtime = Arc::new(MockRuntimeSpawner::default());
        let out_mgr = Arc::new(TaskOutputManager::new(
            PathBuf::from(dir.path()),
            fs.clone(),
        ));
        let registry = Arc::new(TaskRegistry::new(runtime, fs, out_mgr));
        (dir, registry)
    }

    #[test]
    fn web_search_counter_is_visible_through_the_handle_seam() {
        // Regression: the `TaskRegistryHandle` impl MUST delegate the session
        // WebSearch counter to the inherent atomic — not fall through to the
        // trait's `getWebSearchCalls(){return 0}` default. The `WebSearch` tool's
        // budget gate reads the count exclusively through this `&dyn` seam, so a
        // missing override would leave the counter permanently at 0 and never cap.
        let (_dir, registry) = make_registry();
        let handle: &dyn TaskRegistryHandle = registry.as_ref();
        assert_eq!(handle.web_search_calls(), 0);
        handle.increment_web_search_calls();
        handle.increment_web_search_calls();
        assert_eq!(handle.web_search_calls(), 2, "seam must observe increments");
        // Inherent and seam views share the same atomic.
        assert_eq!(registry.web_search_calls(), 2);
        handle.reset_web_search_calls();
        assert_eq!(handle.web_search_calls(), 0);
    }

    #[tokio::test]
    async fn monitor_event_is_drained_as_a_live_notification() {
        let (_dir, registry) = make_registry();
        let task_id = crate::id::generate_task_id(crate::id::TaskType::Monitor);
        let spool = registry.output_manager.allocate(&task_id).await.unwrap();
        registry
            .insert_state_for_test(TaskState::Monitor(crate::state::MonitorTaskState {
                base: crate::state::TaskStateBase {
                    id: task_id.clone(),
                    task_type: crate::id::TaskType::Monitor,
                    status: TaskStatus::Running,
                    description: "watch build".into(),
                    tool_use_id: Some("toolu_monitor".into()),
                    start_time: std::time::SystemTime::UNIX_EPOCH,
                    end_time: None,
                    total_paused_ms: 0,
                    output_file: spool.clone(),
                    evict_after: None,
                    output_offset: 0,
                    notified: false,
                    creator_teammate_name: None,
                    creator_team_name: None,
                    creator_agent_id: None,
                },
                command: "tail -f build.log".into(),
                exit_code: None,
                stdout_bytes: None,
            }))
            .await;

        let handle: &dyn TaskRegistryHandle = registry.as_ref();
        handle
            .notify_monitor_event(&task_id, "step <2> complete", false)
            .await;
        let events = handle.take_pending_task_notifications().await.unwrap();
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.task_id, task_id);
        assert_eq!(event.task_type, "monitor_ws");
        assert_eq!(event.status, "running");
        assert_eq!(event.result.as_deref(), Some("step <2> complete"));
        assert_eq!(event.tool_use_id.as_deref(), Some("toolu_monitor"));
        assert_eq!(event.output_path.as_deref(), spool.to_str());
        assert!(
            handle
                .take_pending_task_notifications()
                .await
                .unwrap()
                .is_empty(),
            "live event is consumed once"
        );
    }

    // ── agent-specific output helpers (T3) ───────────────────────────────

    /// TO-07 step 0: the shape the RUNNER actually produces. `SubagentResult`'s
    /// `content` is the runner's completion payload — an object carrying `text`
    /// and/or a `content` block array — and the `local_agent` spool holds that
    /// object pretty-printed. Requiring a bare array made this return `""` for
    /// every production agent, so `TaskOutputChunk.result` was always `None` and
    /// `TaskOutput` served the raw JSON transcript instead of the answer.
    #[test]
    fn extract_text_content_reads_the_runners_object_shape() {
        // `text` wins when present, matching the runner's own
        // `completed_result_text`.
        assert_eq!(
            extract_text_content(&serde_json::json!({
                "text": "the answer",
                "content": [{"type": "text", "text": "ignored"}]
            })),
            "the answer"
        );
        // Falls back to the block array under `content`.
        assert_eq!(
            extract_text_content(&serde_json::json!({
                "content": [
                    {"type": "text", "text": "first"},
                    {"type": "tool_use", "name": "Bash"},
                    {"type": "text", "text": "second"}
                ]
            })),
            "first\nsecond"
        );
        // An empty `text` is not an answer — fall through rather than return "".
        assert_eq!(
            extract_text_content(&serde_json::json!({
                "text": "",
                "content": [{"type": "text", "text": "real"}]
            })),
            "real"
        );
        // The max-turns payload carries neither, so it yields nothing.
        assert_eq!(
            extract_text_content(&serde_json::json!({
                "reason": "max_turns_exhausted", "max_turns": 12
            })),
            ""
        );
    }

    #[test]
    fn extract_text_content_joins_text_blocks_with_newline() {
        // Port of `extractTextContent(blocks, '\n')`: only `type:"text"` blocks,
        // joined by `\n`; non-text blocks (e.g. tool_use) are dropped.
        let content = serde_json::json!([
            { "type": "text", "text": "first" },
            { "type": "tool_use", "name": "Bash", "input": {} },
            { "type": "text", "text": "second" }
        ]);
        assert_eq!(extract_text_content(&content), "first\nsecond");
    }

    #[test]
    fn extract_text_content_non_array_is_empty() {
        // A non-array content (bare string / object) has no text blocks ⇒ "".
        assert_eq!(extract_text_content(&serde_json::json!("hello")), "");
        assert_eq!(extract_text_content(&serde_json::json!({ "x": 1 })), "");
    }

    #[test]
    fn agent_content_from_spool_strips_usage_footer_and_parses() {
        // The local_agent handler spools `to_string_pretty(content)` + a
        // `<usage>…</usage>` footer; recover the original content Value.
        let spool = "[\n  {\n    \"type\": \"text\",\n    \"text\": \"the answer\"\n  }\n]\n\
                     <usage><total_tokens>9</total_tokens></usage>\n";
        let content = agent_content_from_spool(spool).expect("parses");
        assert_eq!(extract_text_content(&content), "the answer");
    }

    #[test]
    fn agent_content_from_spool_unparseable_is_none() {
        // A `Failed` reason string (not JSON) ⇒ None, so the caller keeps the
        // raw output.
        assert!(agent_content_from_spool("model refused to continue").is_none());
        assert!(agent_content_from_spool("").is_none());
    }

    #[tokio::test]
    async fn output_local_agent_surfaces_clean_result_prompt_and_error() {
        // Drive a real local_agent task through the registry: insert a spawned
        // LocalAgent state, write its spool (the pretty-JSON transcript blob),
        // stamp an error, and verify `output()` returns the CLEAN extracted
        // text in `result` plus the agent's `prompt` + `error`.
        let (_d, registry) = make_registry();

        // Build a LocalAgent state directly and inject it (the registry's task
        // map is private, so use the spawn-state builder + the test setter).
        let chunk = {
            // Allocate a spool slot under a real LocalAgent id.
            let task_id = crate::id::generate_task_id(crate::id::TaskType::LocalAgent);
            let spool = registry.output_manager.allocate(&task_id).await.unwrap();
            let base = crate::state::TaskStateBase {
                id: task_id.clone(),
                task_type: crate::id::TaskType::LocalAgent,
                status: TaskStatus::Failed,
                description: "run the agent".into(),
                tool_use_id: None,
                start_time: std::time::SystemTime::UNIX_EPOCH,
                end_time: None,
                total_paused_ms: 0,
                output_file: spool.clone(),
                evict_after: None,
                output_offset: 0,
                notified: false,
                creator_teammate_name: None,
                creator_team_name: None,
                creator_agent_id: None,
            };
            let state = TaskState::LocalAgent(crate::state::LocalAgentTaskState {
                is_parked: false,
                is_observer: false,
                observed_agent_id: None,
                base,
                agent_id: protocol::AgentId::nil(),
                subagent_type: String::new(),
                prompt: "do the thing".into(),
                error: Some("model refused".into()),
                messages: vec![],
                pending_messages: vec![],
                is_backgrounded: true,
                outcome: Default::default(),
                forked_skill_name: None,
            });
            registry.insert_state_for_test(state).await;

            // Spool the raw pretty-JSON transcript + usage footer.
            let blob = "[\n  {\n    \"type\": \"text\",\n    \"text\": \"final answer\"\n  }\n]\n\
                        <usage><total_tokens>3</total_tokens></usage>\n";
            registry
                .output_manager
                .fs_for_test()
                .write_file(spool.to_str().unwrap(), blob)
                .await
                .unwrap();

            let h: &dyn TaskRegistryHandle = registry.as_ref();
            h.output(&task_id, None).await.unwrap()
        };

        assert_eq!(chunk.result.as_deref(), Some("final answer"));
        assert_eq!(chunk.prompt.as_deref(), Some("do the thing"));
        assert_eq!(chunk.error.as_deref(), Some("model refused"));
        assert!(chunk.done, "failed task is terminal");
    }

    #[tokio::test]
    async fn list_workflows_projects_running_paused_and_completed_and_skips_others() {
        let (_d, registry) = make_registry();

        let mk_wf =
            |id: &str, session: Option<&str>, status: TaskStatus, run: &str, ended: bool| {
                TaskState::LocalWorkflow(crate::state::LocalWorkflowTaskState {
                    base: crate::state::TaskStateBase {
                        id: id.into(),
                        task_type: crate::id::TaskType::LocalWorkflow,
                        status,
                        description: "review the diff".into(),
                        tool_use_id: None,
                        start_time: std::time::SystemTime::UNIX_EPOCH
                            + std::time::Duration::from_millis(1_000),
                        end_time: ended.then(|| {
                            std::time::SystemTime::UNIX_EPOCH
                                + std::time::Duration::from_millis(5_000)
                        }),
                        total_paused_ms: 0,
                        output_file: PathBuf::from(format!("/tmp/{id}.output")),
                        evict_after: None,
                        output_offset: 0,
                        notified: false,
                        creator_teammate_name: None,
                        creator_team_name: None,
                        creator_agent_id: None,
                    },
                    session_uuid: session.map(str::to_string),
                    workflow_id: format!("wf-{id}"),
                    script: format!("export const meta = {{ name: 'wf-{id}' }};"),
                    resume_from_run_id: None,
                    args: None,
                    run_id: Some(run.into()),
                    script_path: None,
                    transcript_dir: None,
                    current_step: 2,
                    outcome: Default::default(),
                    scope: None,
                })
            };
        registry
            .insert_state_for_test(mk_wf(
                "w0000run0",
                None,
                TaskStatus::Running,
                "wf_run",
                false,
            ))
            .await;
        registry
            .insert_state_for_test(mk_wf(
                "w0000done0",
                None,
                TaskStatus::Completed,
                "wf_done",
                true,
            ))
            .await;
        let mut paused = mk_wf("w000pause", None, TaskStatus::Paused, "wf_pause", false);
        if let TaskState::LocalWorkflow(workflow) = &mut paused {
            workflow.script.clear();
            workflow.script_path = Some("/workspace/build.js".into());
            workflow.args = Some(r#"{"app_id":"demo"}"#.into());
        }
        registry.insert_state_for_test(paused).await;
        // An unrelated task must NOT appear in the workflow projection.
        let base = crate::state::TaskStateBase {
            id: "b0000bash0".into(),
            task_type: crate::id::TaskType::LocalBash,
            status: TaskStatus::Running,
            description: "cargo build".into(),
            tool_use_id: None,
            start_time: std::time::SystemTime::UNIX_EPOCH,
            end_time: None,
            total_paused_ms: 0,
            output_file: PathBuf::from("/tmp/bash.output"),
            evict_after: None,
            output_offset: 0,
            notified: false,
            creator_teammate_name: None,
            creator_team_name: None,
            creator_agent_id: None,
        };
        registry
            .insert_state_for_test(TaskState::LocalBash(crate::state::LocalBashTaskState {
                is_adopted: false,
                caller: None,
                base,
                command: "cargo build".into(),
                pid: None,
                exit_code: None,
                cwd: None,
                is_backgrounded: None,
            }))
            .await;

        let h: &dyn TaskRegistryHandle = registry.as_ref();
        let mut wfs = h.list_workflows().await.unwrap();
        wfs.sort_by(|a, b| a.task_id.cmp(&b.task_id));

        assert_eq!(
            wfs.len(),
            3,
            "only workflow runs, including the adopted paused run"
        );
        let done = wfs.iter().find(|w| w.task_id == "w0000done0").unwrap();
        assert_eq!(done.name, "wf-w0000done0");
        assert_eq!(done.run_id.as_deref(), Some("wf_done"));
        assert_eq!(done.status, "completed");
        assert_eq!(done.current_step, 2);
        assert_eq!(done.started_at_ms, Some(1_000));
        assert_eq!(done.ended_at_ms, Some(5_000), "terminal run carries an end");
        assert_eq!(
            done.script.as_deref(),
            Some("export const meta = { name: 'wf-w0000done0' };")
        );
        let run = wfs.iter().find(|w| w.task_id == "w0000run0").unwrap();
        assert_eq!(run.status, "running");
        assert_eq!(run.ended_at_ms, None, "a running run has no end");
        assert_eq!(
            run.script.as_deref(),
            Some("export const meta = { name: 'wf-w0000run0' };")
        );
        let paused = wfs.iter().find(|w| w.task_id == "w000pause").unwrap();
        assert_eq!(paused.status, "paused");
        assert_eq!(
            paused.script, None,
            "adopted paused runs do not surface an empty inline script"
        );
        assert_eq!(paused.script_path.as_deref(), Some("/workspace/build.js"));
        assert_eq!(paused.args.as_deref(), Some(r#"{"app_id":"demo"}"#));
    }

    #[tokio::test]
    async fn list_and_list_workflows_filter_workflow_rows_to_active_session_only() {
        let (_d, registry) = make_registry();

        let mk_wf = |id: &str, session: &str, status: TaskStatus| {
            TaskState::LocalWorkflow(crate::state::LocalWorkflowTaskState {
                base: crate::state::TaskStateBase {
                    id: id.into(),
                    task_type: crate::id::TaskType::LocalWorkflow,
                    status,
                    description: "session workflow".into(),
                    tool_use_id: None,
                    start_time: std::time::SystemTime::UNIX_EPOCH,
                    end_time: None,
                    total_paused_ms: 0,
                    output_file: PathBuf::from(format!("/tmp/{id}.output")),
                    evict_after: None,
                    output_offset: 0,
                    notified: false,
                    creator_teammate_name: None,
                    creator_team_name: None,
                    creator_agent_id: None,
                },
                session_uuid: Some(session.to_string()),
                workflow_id: format!("wf-{id}"),
                script: String::new(),
                resume_from_run_id: None,
                args: None,
                run_id: Some(format!("wf-{id}")),
                script_path: None,
                transcript_dir: None,
                current_step: 0,
                outcome: Default::default(),
                scope: None,
            })
        };
        registry
            .insert_state_for_test(mk_wf("waaaaaaaa", "session-a", TaskStatus::Running))
            .await;
        registry
            .insert_state_for_test(mk_wf("wbbbbbbbb", "session-b", TaskStatus::Paused))
            .await;
        registry.set_workflow_session_filter(Some("session-a".to_string()));

        registry
            .insert_state_for_test(TaskState::LocalBash(crate::state::LocalBashTaskState {
                is_adopted: false,
                caller: None,
                base: crate::state::TaskStateBase {
                    id: "bvisible01".into(),
                    task_type: crate::id::TaskType::LocalBash,
                    status: TaskStatus::Running,
                    description: "still visible".into(),
                    tool_use_id: None,
                    start_time: std::time::SystemTime::UNIX_EPOCH,
                    end_time: None,
                    total_paused_ms: 0,
                    output_file: PathBuf::from("/tmp/bvisible01.output"),
                    evict_after: None,
                    output_offset: 0,
                    notified: false,
                    creator_teammate_name: None,
                    creator_team_name: None,
                    creator_agent_id: None,
                },
                command: "echo visible".into(),
                pid: None,
                exit_code: None,
                cwd: None,
                is_backgrounded: None,
            }))
            .await;

        let h: &dyn TaskRegistryHandle = registry.as_ref();
        let records = h.list(TaskListFilter::default()).await.unwrap();
        assert!(records.iter().any(|record| record.task_id == "waaaaaaaa"));
        assert!(records.iter().any(|record| record.task_id == "bvisible01"));
        assert!(
            records.iter().all(|record| record.task_id != "wbbbbbbbb"),
            "workflow rows from other sessions must be hidden: {records:?}"
        );

        let workflows = h.list_workflows().await.unwrap();
        assert_eq!(
            workflows.len(),
            1,
            "only the active session workflow remains"
        );
        assert_eq!(workflows[0].task_id, "waaaaaaaa");
    }

    #[tokio::test]
    async fn get_local_bash_record_carries_command() {
        // T10: a `local_bash` record surfaces its shell COMMAND (claude-code
        // `LocalShellTaskState.command`), distinct from its description, so
        // `TaskStop` can prefer it (`stopTask.ts:97`).
        let (_d, registry) = make_registry();
        let task_id = crate::id::generate_task_id(crate::id::TaskType::LocalBash);
        let spool = registry.output_manager.allocate(&task_id).await.unwrap();
        let base = crate::state::TaskStateBase {
            id: task_id.clone(),
            task_type: crate::id::TaskType::LocalBash,
            status: TaskStatus::Running,
            description: "build the workspace".into(),
            tool_use_id: None,
            start_time: std::time::SystemTime::UNIX_EPOCH,
            end_time: None,
            total_paused_ms: 0,
            output_file: spool,
            evict_after: None,
            output_offset: 0,
            notified: false,
            creator_teammate_name: None,
            creator_team_name: None,
            creator_agent_id: None,
        };
        let state = TaskState::LocalBash(crate::state::LocalBashTaskState {
            is_adopted: false,
            caller: None,
            base,
            command: "cargo build --release".into(),
            pid: None,
            exit_code: None,
            cwd: None,
            is_backgrounded: None,
        });
        registry.insert_state_for_test(state).await;

        let h: &dyn TaskRegistryHandle = registry.as_ref();
        let rec = h.get(&task_id).await.unwrap().expect("record present");
        assert_eq!(
            rec.command.as_deref(),
            Some("cargo build --release"),
            "local_bash record carries the shell command"
        );
        assert_eq!(rec.description, "build the workspace");
    }

    #[tokio::test]
    async fn get_local_agent_record_has_no_command() {
        // T10: non-bash task types report `command: None` so `TaskStop` falls back
        // to `description` (`stopTask.ts:97` `: task.description`).
        let (_d, registry) = make_registry();
        let task_id = crate::id::generate_task_id(crate::id::TaskType::LocalAgent);
        let spool = registry.output_manager.allocate(&task_id).await.unwrap();
        let base = crate::state::TaskStateBase {
            id: task_id.clone(),
            task_type: crate::id::TaskType::LocalAgent,
            status: TaskStatus::Running,
            description: "review the PR".into(),
            tool_use_id: None,
            start_time: std::time::SystemTime::UNIX_EPOCH,
            end_time: None,
            total_paused_ms: 0,
            output_file: spool,
            evict_after: None,
            output_offset: 0,
            notified: false,
            creator_teammate_name: None,
            creator_team_name: None,
            creator_agent_id: None,
        };
        let state = TaskState::LocalAgent(crate::state::LocalAgentTaskState {
            is_parked: false,
            is_observer: false,
            observed_agent_id: None,
            base,
            agent_id: protocol::AgentId::nil(),
            subagent_type: String::new(),
            prompt: "do it".into(),
            error: None,
            messages: vec![],
            pending_messages: vec![],
            is_backgrounded: true,
            outcome: Default::default(),
            forked_skill_name: None,
        });
        registry.insert_state_for_test(state).await;

        let h: &dyn TaskRegistryHandle = registry.as_ref();
        let rec = h.get(&task_id).await.unwrap().expect("record present");
        assert_eq!(rec.command, None, "non-bash record has no command");
    }

    /// F005 (reviewer round-1 blocking issue): a `local_fusion` task's
    /// progress-stage label reaches `TaskRecord.stage` through
    /// `state_to_record`, so `TaskRegistryHandle::get`/`list` — the DTO/list
    /// chain every client, TUI, Electron RuntimeCenter and notification
    /// surface reads — can actually observe it. Before this fix, `LocalFusion`
    /// hit the extras match's `_ => (None, ...)` arm and `TaskRecord` had no
    /// `stage` field at all, so `LocalFusionTaskState.stage` (set by
    /// `TaskStatusSink::set_fusion_stage`) had zero production readers.
    #[tokio::test]
    async fn get_local_fusion_record_carries_progress_stage() {
        let (_d, registry) = make_registry();
        let task_id = crate::id::generate_task_id(crate::id::TaskType::LocalFusion);
        let spool = registry.output_manager.allocate(&task_id).await.unwrap();
        let base = crate::state::TaskStateBase {
            id: task_id.clone(),
            task_type: crate::id::TaskType::LocalFusion,
            status: TaskStatus::Running,
            description: "compare two approaches".into(),
            tool_use_id: None,
            start_time: std::time::SystemTime::UNIX_EPOCH,
            end_time: None,
            total_paused_ms: 0,
            output_file: spool,
            evict_after: None,
            output_offset: 0,
            notified: false,
            creator_teammate_name: None,
            creator_team_name: None,
            creator_agent_id: None,
        };
        let state = TaskState::LocalFusion(crate::state::LocalFusionTaskState {
            base,
            conversation_id: "conv1".into(),
            prompt: "compare two approaches".into(),
            run_id: None,
            preset: "quality".into(),
            cross_provider: false,
            final_text: None,
            error: None,
            egress_profiles: Vec::new(),
            usage: None,
            stage: None,
            effective_timeout_ms: None,
            planned_panels: None,
            fusion_activation_deadline: None,
            publication_status: platform_api::FusionPublicationStatus::Pending,
            publication_error: None,
        });
        registry.insert_state_for_test(state).await;

        let h: &dyn TaskRegistryHandle = registry.as_ref();
        let before = h.get(&task_id).await.unwrap().expect("record present");
        assert_eq!(
            before.stage, None,
            "no progress event has landed yet on the fresh record"
        );

        // Drive the same producer chain the running orchestrator uses:
        // `TaskStatusSink::set_fusion_stage` → `RegistryStatusSink` →
        // `TaskRegistry::set_fusion_stage`.
        use crate::handlers::TaskStatusSink as _;
        let sink = crate::registry_status_sink::RegistryStatusSink::new();
        sink.bind(registry.clone());
        sink.set_fusion_stage(&task_id, "Running panels 2/3".to_string())
            .await;

        let after = h.get(&task_id).await.unwrap().expect("record present");
        assert_eq!(
            after.stage.as_deref(),
            Some("Running panels 2/3"),
            "the DTO/list record — not just LocalFusionTaskState — must carry the label"
        );
    }

    /// F010: a failed `/fusion` task's recorded reason must survive the
    /// public TaskRegistryHandle projection.  The registry stores the reason
    /// on LocalFusionTaskState and notifications already expose it, but the
    /// get/list DTO path used by task clients must expose the same value.
    #[tokio::test]
    async fn local_fusion_failure_reason_reaches_public_get_and_list_records() {
        let (_d, registry) = make_registry();
        let task_id = crate::id::generate_task_id(crate::id::TaskType::LocalFusion);
        let spool = registry.output_manager.allocate(&task_id).await.unwrap();
        let base = crate::state::TaskStateBase {
            id: task_id.clone(),
            task_type: crate::id::TaskType::LocalFusion,
            status: TaskStatus::Failed,
            description: "compare two approaches".into(),
            tool_use_id: None,
            start_time: std::time::SystemTime::UNIX_EPOCH,
            end_time: None,
            total_paused_ms: 0,
            output_file: spool,
            evict_after: None,
            output_offset: 0,
            notified: false,
            creator_teammate_name: None,
            creator_team_name: None,
            creator_agent_id: None,
        };
        registry
            .insert_state_for_test(TaskState::LocalFusion(crate::state::LocalFusionTaskState {
                base,
                conversation_id: "conv1".into(),
                prompt: "compare two approaches".into(),
                run_id: None,
                preset: "quality".into(),
                cross_provider: false,
                final_text: None,
                error: Some("too few eligible models".into()),
                egress_profiles: Vec::new(),
                usage: None,
                stage: None,
                effective_timeout_ms: None,
                planned_panels: None,
                fusion_activation_deadline: None,
                publication_status: platform_api::FusionPublicationStatus::Pending,
                publication_error: None,
            }))
            .await;

        let h: &dyn TaskRegistryHandle = registry.as_ref();
        let get = h.get(&task_id).await.unwrap().expect("record present");
        assert_eq!(get.status, "failed");
        assert_eq!(get.error.as_deref(), Some("too few eligible models"));

        let failed = h
            .list(TaskListFilter {
                status: Some("failed".into()),
            })
            .await
            .unwrap();
        let listed = failed
            .iter()
            .find(|record| record.task_id == task_id)
            .expect("failed Fusion record listed");
        assert_eq!(listed.error.as_deref(), Some("too few eligible models"));
    }

    #[tokio::test]
    async fn create_via_handle_returns_record_with_pending_status() {
        let (_d, registry) = make_registry();
        let h: &dyn TaskRegistryHandle = registry.as_ref();
        let rec = h
            .create(TaskCreateInput {
                task_type: "local_bash".into(),
                description: "hello".into(),
            })
            .await
            .unwrap();
        assert!(rec.task_id.starts_with('b'));
        assert_eq!(rec.status, "pending");
        assert_eq!(rec.task_type, "local_bash");
    }

    #[tokio::test]
    async fn list_filters_by_status() {
        let (_d, registry) = make_registry();
        let h: &dyn TaskRegistryHandle = registry.as_ref();
        h.create(TaskCreateInput {
            task_type: "local_bash".into(),
            description: "a".into(),
        })
        .await
        .unwrap();
        let pending = h
            .list(TaskListFilter {
                status: Some("pending".into()),
            })
            .await
            .unwrap();
        assert_eq!(pending.len(), 1);
        let running = h
            .list(TaskListFilter {
                status: Some("running".into()),
            })
            .await
            .unwrap();
        assert!(running.is_empty());
    }

    /// AG-1 — the id the `Agent` tool hands the model must work for BOTH
    /// `TaskOutput` and `TaskStop`. It resolved for `TaskOutput` (via
    /// `registry.get` → `canonical_or_raw`) and came back `NotFound` from
    /// `TaskStop`, because `resolve::resolve_stop_target` never consults the
    /// alias map. The model is explicitly told to use that id, so the two tools
    /// disagreeing about it is a contract break, not a nicety.
    #[tokio::test]
    async fn task_stop_resolves_the_agent_id_that_task_output_accepts() {
        let (_d, registry) = make_registry();
        let h: &dyn TaskRegistryHandle = registry.as_ref();
        let rec = h
            .create(TaskCreateInput {
                task_type: "local_agent".into(),
                description: "a".into(),
            })
            .await
            .unwrap();
        // What `aliases_for_spawn` registers for an async local agent: the bare
        // agent uuid the tool result publishes.
        let agent_uuid = format!("11111111-2222-3333-4444-{:012x}", 0xA6E17u64);
        registry
            .register_task_alias(&rec.task_id, agent_uuid.clone())
            .await
            .unwrap();

        // Precondition — TaskOutput's lookup already accepts it.
        assert!(
            h.get(&agent_uuid).await.unwrap().is_some(),
            "precondition: the alias resolves for the TaskOutput path"
        );

        match h.resolve_stop_target(&agent_uuid, &[]).await.unwrap() {
            platform_api::task_registry::TaskStopResolution::Found(found) => {
                assert_eq!(found.task_id, rec.task_id);
            }
            other => panic!("TaskStop must resolve the published agent id, got {other:?}"),
        }
    }

    /// The alias path must not shadow a real task id, and an id belonging to
    /// nothing must still be `NotFound` — otherwise the fix would turn every
    /// typo into a silent stop of some unrelated task.
    #[tokio::test]
    async fn a_real_task_id_still_wins_and_an_unknown_id_is_still_not_found() {
        let (_d, registry) = make_registry();
        let h: &dyn TaskRegistryHandle = registry.as_ref();
        let rec = h
            .create(TaskCreateInput {
                task_type: "local_agent".into(),
                description: "a".into(),
            })
            .await
            .unwrap();
        match h.resolve_stop_target(&rec.task_id, &[]).await.unwrap() {
            platform_api::task_registry::TaskStopResolution::Found(found) => {
                assert_eq!(found.task_id, rec.task_id);
            }
            other => panic!("a real task id must resolve, got {other:?}"),
        }
        assert!(
            matches!(
                h.resolve_stop_target("no-such-id-anywhere", &[])
                    .await
                    .unwrap(),
                platform_api::task_registry::TaskStopResolution::NotFound { .. }
            ),
            "an unknown id must stay NotFound"
        );
    }

    #[tokio::test]
    async fn set_status_via_handle_transitions() {
        let (_d, registry) = make_registry();
        let h: &dyn TaskRegistryHandle = registry.as_ref();
        let rec = h
            .create(TaskCreateInput {
                task_type: "local_bash".into(),
                description: "a".into(),
            })
            .await
            .unwrap();
        let updated = h.set_status(&rec.task_id, "running").await.unwrap();
        assert_eq!(updated.status, "running");
    }

    #[tokio::test]
    async fn unknown_task_type_is_invalid_input() {
        let (_d, registry) = make_registry();
        let h: &dyn TaskRegistryHandle = registry.as_ref();
        let err = h
            .create(TaskCreateInput {
                task_type: "bogus".into(),
                description: "x".into(),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, TaskRegistryError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn output_returns_empty_for_freshly_created_task_with_zero_byte_spool() {
        let (_d, registry) = make_registry();
        let h: &dyn TaskRegistryHandle = registry.as_ref();
        let rec = h
            .create(TaskCreateInput {
                task_type: "local_bash".into(),
                description: "a".into(),
            })
            .await
            .unwrap();
        let chunk = h.output(&rec.task_id, None).await.unwrap();
        assert_eq!(chunk.task_id, rec.task_id);
        assert_eq!(chunk.content, "");
        assert!(!chunk.truncated);
    }

    #[tokio::test]
    async fn output_returns_real_content_after_spool_write() {
        let (_d, registry) = make_registry();
        let h: &dyn TaskRegistryHandle = registry.as_ref();
        let rec = h
            .create(TaskCreateInput {
                task_type: "local_bash".into(),
                description: "echo hi".into(),
            })
            .await
            .unwrap();

        // Pull the spool path from the typed state, then write content
        // directly through the registry's filesystem (simulates a handler
        // producing output).
        let state = registry.get(&rec.task_id).await.unwrap();
        let path = state.base().output_file.clone();
        let path_str = path.to_str().unwrap().to_string();
        let fs = registry.output_manager.fs_for_test();
        fs.write_file(&path_str, "line1\nline2\nline3\n")
            .await
            .unwrap();

        let chunk = h.output(&rec.task_id, None).await.unwrap();
        assert_eq!(chunk.task_id, rec.task_id);
        assert_eq!(
            chunk.content, "line1\nline2\nline3\n",
            "spool content surfaces verbatim"
        );
        assert_eq!(chunk.total_lines, 3, "total_lines reflects the spool");
        assert!(!chunk.truncated, "no truncation on unlimited read");
    }

    #[tokio::test]
    async fn output_running_task_is_not_done_and_carries_no_exit_code() {
        let (_d, registry) = make_registry();
        let h: &dyn TaskRegistryHandle = registry.as_ref();
        let rec = h
            .create(TaskCreateInput {
                task_type: "local_bash".into(),
                description: "sleep".into(),
            })
            .await
            .unwrap();
        registry
            .force_bash_terminal_for_test(&rec.task_id, TaskStatus::Running, None)
            .await;

        let chunk = h.output(&rec.task_id, None).await.unwrap();
        assert_eq!(chunk.status.as_deref(), Some("running"));
        assert!(!chunk.done, "running task is not terminal");
        assert_eq!(chunk.exit_code, None, "no exit code while running");
    }

    #[tokio::test]
    async fn output_completed_task_is_done_with_exit_code_zero() {
        let (_d, registry) = make_registry();
        let h: &dyn TaskRegistryHandle = registry.as_ref();
        let rec = h
            .create(TaskCreateInput {
                task_type: "local_bash".into(),
                description: "echo".into(),
            })
            .await
            .unwrap();
        registry
            .force_bash_terminal_for_test(&rec.task_id, TaskStatus::Completed, Some(0))
            .await;

        let chunk = h.output(&rec.task_id, None).await.unwrap();
        assert_eq!(chunk.status.as_deref(), Some("completed"));
        assert!(chunk.done, "completed task is terminal");
        assert_eq!(chunk.exit_code, Some(0));
    }

    #[tokio::test]
    async fn output_failed_task_is_done_with_nonzero_exit_code() {
        let (_d, registry) = make_registry();
        let h: &dyn TaskRegistryHandle = registry.as_ref();
        let rec = h
            .create(TaskCreateInput {
                task_type: "local_bash".into(),
                description: "false".into(),
            })
            .await
            .unwrap();
        registry
            .force_bash_terminal_for_test(&rec.task_id, TaskStatus::Failed, Some(1))
            .await;

        let chunk = h.output(&rec.task_id, None).await.unwrap();
        assert_eq!(chunk.status.as_deref(), Some("failed"));
        assert!(chunk.done, "failed task is terminal");
        assert_eq!(chunk.exit_code, Some(1));
    }

    #[tokio::test]
    async fn output_threads_offset_into_output_manager() {
        let (_d, registry) = make_registry();
        let h: &dyn TaskRegistryHandle = registry.as_ref();
        let rec = h
            .create(TaskCreateInput {
                task_type: "local_bash".into(),
                description: "echo hi".into(),
            })
            .await
            .unwrap();

        let state = registry.get(&rec.task_id).await.unwrap();
        let path_str = state.base().output_file.to_str().unwrap().to_string();
        let fs = registry.output_manager.fs_for_test();
        fs.write_file(&path_str, "abcdef").await.unwrap();

        // offset=3 should drop the first 3 chars.
        let chunk = h.output(&rec.task_id, Some(3)).await.unwrap();
        assert_eq!(chunk.content, "def", "offset honoured");
    }
}
