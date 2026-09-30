//! Generic `Task` trait — implemented per [`TaskType`](crate::id::TaskType).

use async_trait::async_trait;
use lingxi_core::host::{
    FileSystem, FusionActivation, FusionPreparedSummary, RuntimeSpawner, SubagentInheritance,
    SubagentSpawnRequest,
};
use std::sync::Arc;
use thiserror::Error;

/// Socket input holds the transport injected by the tool's host.
#[derive(Clone)]
pub struct WebSocketMonitorInput {
    /// Source and common task metadata.
    pub registration: lingxi_core::host::task_registry::WebSocketMonitorRegistration,
    /// Native network transport.
    pub http: Arc<dyn lingxi_core::host::HttpTransport>,
}
impl std::fmt::Debug for WebSocketMonitorInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSocketMonitorInput")
            .field("registration", &self.registration)
            .finish_non_exhaustive()
    }
}

/// Generic interface implemented by per-type task handlers.
#[async_trait]
pub trait Task: Send + Sync {
    /// Human-readable handler name (for logs and metrics).
    fn name(&self) -> &str;
    /// Task type discriminant.
    fn task_type(&self) -> crate::id::TaskType;
    /// Spawn a new task instance.
    async fn spawn(&self, input: TaskSpawnInput, ctx: TaskContext)
        -> Result<TaskHandle, TaskError>;
    /// Kill a running task instance.
    async fn kill(&self, task_id: &str, ctx: TaskContext) -> Result<(), TaskError>;
    /// Wait for handler-owned workers that already won natural finalization.
    ///
    /// Ordinary [`Self::kill`] must return immediately when cancellation loses
    /// to a finalizing worker. Host shutdown calls this separate barrier only
    /// after task admission is closed and every cancellable task was stopped.
    async fn drain_shutdown(&self) -> Result<(), TaskError> {
        Ok(())
    }
    /// Whether the task's execution loop still has a cancellable worker.
    async fn has_live_worker(&self, _task_id: &str) -> bool {
        false
    }
    /// Real child agent identities whose processes belong to this task.
    async fn process_owner_ids(&self, _task_id: &str) -> Vec<lingxi_core::types::AgentId> {
        Vec::new()
    }

    fn forget_resume_recipe(&self, _task_id: &str) {}
    async fn register_resume_recipe(
        &self,
        _task_id: &str,
        _request: SubagentSpawnRequest,
        _inheritance: SubagentInheritance,
    ) -> Result<(), TaskError> {
        Err(TaskError::Unsupported)
    }
    async fn prepare_human_resume(
        &self,
        _task_id: &str,
        _agent_id: lingxi_core::types::AgentId,
        _epoch: u64,
        _ctx: TaskContext,
    ) -> Result<HumanResumePrepared, TaskError> {
        Err(TaskError::Unsupported)
    }

    /// Whether this task type supports inbound messages.
    fn supports_messages(&self) -> bool {
        false
    }
    /// Apply a trusted lead's plan review response at the teammate's idle boundary.
    async fn apply_plan_approval(
        &self,
        _task_id: &str,
        _response: lingxi_core::host::teammate_plan::PlanApprovalResponse,
        _ctx: TaskContext,
    ) -> Result<(), TaskError> {
        Err(TaskError::Unsupported)
    }

    /// Deliver a message to a running task. Default implementation rejects.
    async fn send_message(
        &self,
        _task_id: &str,
        _message: String,
        _ctx: TaskContext,
    ) -> Result<(), TaskError> {
        Err(TaskError::Unsupported)
    }
}

/// Spawn-time input — one variant per task type.
#[derive(Debug, Clone)]
pub enum TaskSpawnInput {
    /// Passive WebSocket monitor.
    MonitorWs(WebSocketMonitorInput),
    /// Spawn a local bash command.
    LocalBash {
        /// Bash command string.
        command: String,
        /// Optional time-out.
        timeout: Option<std::time::Duration>,
        /// Originating tool call, when the shell was launched from a tool.
        tool_use_id: Option<String>,
    },
    /// Spawn an in-process agent.
    LocalAgent {
        /// Agent target (per-instance identity UUID).
        agent_id: lingxi_core::types::AgentId,
        /// Resolved subagent-type name (TS `agentType`; the caller applies the
        /// `'general-purpose'` fallback when the `AgentDefinition` has none).
        subagent_type: String,
        /// Initial prompt.
        prompt: String,
        /// Whether to start backgrounded.
        is_backgrounded: bool,
        /// Originating `tool_use_id` of the spawning Agent tool call, stamped on
        /// the task so a backgrounded agent's `<task-notification>` carries the
        /// `<tool-use-id>` line (claude-code parity). `None` when not launched
        /// from a tool call.
        tool_use_id: Option<String>,
        /// DISPLAY name of the teammate / subagent that created this background
        /// task. Distinct from the TARGET agent name on `spawn_request.name`.
        creator_teammate_name: Option<String>,
        /// Team name of the teammate / subagent that created this background
        /// task. Distinct from the TARGET team on `spawn_request.team_name`.
        creator_team_name: Option<String>,
        /// Persistent agent id of the teammate / subagent that created this
        /// background task. Distinct from the TARGET child identity.
        creator_agent_id: Option<lingxi_core::types::AgentId>,
        /// Complete spawn request for a background Agent invocation. The
        /// duplicated state fields above remain the compact task-index surface;
        /// this preserves model/cwd/context/isolation/schema/depth overrides for
        /// the eventual runner. `None` keeps legacy direct task creation valid.
        spawn_request: Option<SubagentSpawnRequest>,
        /// The immediate parent's tool registry and budget handles. Background
        /// agent execution must inherit these exact Arcs, not fall back to the
        /// composition-root handles. `None` keeps legacy direct task creation
        /// valid.
        inheritance: Option<SubagentInheritance>,
    },
    /// Spawn a remote agent.
    RemoteAgent {
        /// Endpoint URL.
        endpoint: String,
        /// Initial prompt.
        prompt: String,
    },
    /// Spawn an in-process teammate.
    InProcessTeammate {
        /// Resolved Agent request and parent execution context.
        spawn_request: Option<lingxi_core::host::subagent_spawn::SubagentSpawnRequest>,
        /// Tool and budget handles inherited from the invoking session.
        inheritance: Option<lingxi_core::host::subagent_spawn::SubagentInheritance>,
        /// Agent target.
        agent_id: lingxi_core::types::AgentId,
        /// Display name (claude-code `TeammateContext.agentName`).
        name: String,
        /// Team name this teammate belongs to (claude-code
        /// `TeammateContext.teamName`). Empty when spawned standalone / without
        /// a coordinator team context. Threaded into the teammate's
        /// `SubagentContext.team_name` so its dispatched tools see the team
        /// identity (`getTeammateContext()?.teamName`).
        team_name: String,
        /// The teammate's initial task from the Agent prompt, seeded as its first user
        /// message (`SubagentContext::prompt_messages`) so it has a task to work
        /// on rather than only chatting. Empty ⇒ no initial message (the
        /// teammate parks awaiting the first injected message).
        description: String,
    },
    /// Spawn a local workflow.
    LocalWorkflow {
        /// Session that owns this workflow row. Desktop and mobile launchers
        /// pass the originating session so late agent/Fusion work keeps its
        /// budget and transcript identity; the desktop registry may still
        /// leave listing filters unscoped.
        session_uuid: Option<String>,
        /// Workflow identifier.
        workflow_id: String,
        /// The model-authored workflow script source (JavaScript) to execute.
        script: String,
        /// Resume a prior run (`wf_…`): journaled `agent()` results for the
        /// longest unchanged prefix of `agent()` calls are replayed instead of
        /// re-spawned.
        resume_from_run_id: Option<String>,
        /// The `args` global value (the Workflow tool's `args` input), as a JSON
        /// string. `None` ⇒ `undefined`.
        args: Option<String>,
        /// The run id (`wf_…`) to use for a FRESH run, minted by the launcher so
        /// it can be returned in the Workflow tool result (claude-code `runId`).
        /// `None` ⇒ the worker mints one. Ignored when `resume_from_run_id` is
        /// set (the resume id wins).
        run_id: Option<String>,
        // ── Telemetry fields (oracle §7 `tengu_workflow_launched` payload) ──
        /// How the workflow was invoked: `"scriptPath"` | `"named"` | `"inline"`.
        /// Derived from the original `WorkflowLaunchSpec` by the launcher.
        invocation_mode: Option<String>,
        /// The resolved workflow source category: `"built-in"`,
        /// `"projectSettings"`, `"userSettings"`, `"plugin"`,
        /// `"scriptPath"`, or `"inline"`.
        workflow_source: Option<String>,
        /// Whether the resolved script is byte-identical to a bundled built-in.
        /// This is intentionally separate from `workflow_source`: a named
        /// built-in with an explicit script override still has source
        /// `"built-in"`, but is not verbatim for telemetry redaction.
        script_is_verbatim_builtin: Option<bool>,
        /// Launch-pinned transcript directory for every child this workflow
        /// spawns. `None` keeps the spawner's session-derived default.
        transcript_subdir: Option<std::path::PathBuf>,
        /// `true` when launched from a subagent context (`t.agentId != null`).
        launched_from_subagent: bool,
        /// Originating Workflow tool-use id for `<tool-use-id>` in the terminal
        /// task notification.
        tool_use_id: Option<String>,
        /// Creator display/team/id ownership for rest-notification deferral.
        creator_teammate_name: Option<String>,
        /// Team containing the creator, when it belongs to one.
        creator_team_name: Option<String>,
        /// Persistent identity of the creator agent, when available.
        creator_agent_id: Option<lingxi_core::types::AgentId>,
        /// Host-minted Local App authority for this run: which app this
        /// workflow may touch, and why (design §18 Phase -1 step 8 / §8.1).
        ///
        /// [`crate::scope::LocalAppWorkflowTaskScope`] has no public
        /// constructor that takes a name, no `Default` and no `Deserialize`
        /// (see that module's docs), so the only way a `Some` reaches this
        /// field is a Host that called a purpose constructor for an app id it
        /// resolved itself. `state_for_spawn` copies it onto the task row,
        /// where the workspace-lease gate
        /// (`crate::handlers::local_workflow::requires_workspace_lease`) and
        /// the App delete guard
        /// (`crate::registry::TaskRegistry::find_nonterminal_local_app_workflows`)
        /// read it.
        ///
        /// `None` for every workflow that is not a Local App workflow, and
        /// for any Local App launch the Host could not fully validate. `None`
        /// is authority-free by design: neither guard has any fallback to
        /// `workflow_id` or `args`, because both are caller-supplied and a
        /// custom workflow reusing a real workflow's name is indistinguishable
        /// from the real one at this layer.
        scope: Option<crate::scope::LocalAppWorkflowTaskScope>,
    },
    /// Spawn an MCP monitor.
    MonitorMcp {
        /// Server name to monitor.
        server_name: String,
        /// Resources to watch.
        watch: Vec<String>,
    },
    /// Spawn a shell stdout event monitor.
    Monitor {
        /// Shell command to execute.
        command: String,
        /// The sandbox-wrapped form to actually spawn, when the tool's
        /// `shouldUseSandbox` decision said to confine it. `None` ⇒ spawn
        /// `command` unwrapped. `command` stays the raw text so `/tasks` and
        /// the notifications keep showing what the model asked for.
        spawn_command: Option<String>,
        /// Optional deadline; `None` is session-persistent.
        timeout: Option<std::time::Duration>,
        /// Working directory inherited from the tool invocation.
        cwd: Option<std::path::PathBuf>,
        /// Originating assistant tool-use id.
        tool_use_id: Option<String>,
        /// Creator display/team/id ownership for rest-notification deferral.
        creator_teammate_name: Option<String>,
        /// Team containing the creator, when it belongs to one.
        creator_team_name: Option<String>,
        /// Persistent identity of the creator agent, when available.
        creator_agent_id: Option<lingxi_core::types::AgentId>,
    },
    /// Spawn a backgrounded MCP tool call (claude-code 2.1.212 `mcp_task`).
    /// Created when a single `tools/call` exceeds `getMcpAutoBackgroundMs` and
    /// is detached from the turn (`callMcpToolWithAutoBackground`/`NZu`).
    McpTask {
        /// MCP server name (`serverName`).
        server_name: String,
        /// MCP tool name (`toolName`).
        tool_name: String,
        /// Originating assistant `tool_use_id`, if any (`toolUseId`).
        tool_use_id: Option<String>,
        /// Creator display/team/id ownership for rest-notification deferral.
        creator_teammate_name: Option<String>,
        /// Team containing the creator, when it belongs to one.
        creator_team_name: Option<String>,
        /// Persistent identity of the creator agent, when available.
        creator_agent_id: Option<lingxi_core::types::AgentId>,
    },
    /// Environment scan; its owning command drives execution.
    AutoModeScan,
    /// Spawn a dream loop.
    Dream {
        /// Initial prompt.
        prompt: String,
        /// Optional cap on iterations.
        max_iterations: Option<u32>,
    },
    /// Spawn a Fusion deliberation (`/fusion`).
    LocalFusion {
        /// Validated Fusion request. Origin is [`lingxi_core::host::FusionOrigin::Slash`].
        request: lingxi_core::host::FusionRequest,
        /// Parent conversation id for the completion sink.
        conversation_id: String,
    },
}

/// Per-task execution context provided by the engine.
#[derive(Clone)]
pub struct TaskContext {
    /// File-system trait object.
    pub fs: Arc<dyn FileSystem>,
    /// Runtime spawner trait object.
    pub runtime: Arc<dyn RuntimeSpawner>,
}

pub struct HumanResumePrepared {
    pub handle: TaskHandle,
    pub ready: tokio::sync::oneshot::Receiver<Result<(), TaskError>>,
}

/// Handle returned by [`Task::spawn`].
pub struct TaskHandle {
    /// Generated task ID.
    pub task_id: String,
    /// Optional cleanup hook to run on task termination.
    pub cleanup: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Prepared Fusion identity/duration/panel summary copied into the task
    /// row before `TaskCreated`. `None` for every non-Fusion task.
    pub(crate) fusion_prepared_summary: Option<FusionPreparedSummary>,
    /// One-shot worker activation owned by the registry handoff. Dropping an
    /// unactivated handle cancels handlers whose callback owns a readiness
    /// sender, so a cancelled registry spawn cannot launch partial work.
    activation: Option<Box<dyn FnOnce() + Send + 'static>>,
    /// Fusion-specific activation callback. The registry captures the
    /// monotonic activation instant before any post-publication scheduling
    /// wait, then passes that exact instant through this callback.
    fusion_activation: Option<Box<dyn FnOnce(FusionActivation) + Send + 'static>>,
}

impl TaskHandle {
    /// Construct an immediately-runnable handle with no activation barrier.
    pub fn new(task_id: impl Into<String>, cleanup: Option<Arc<dyn Fn() + Send + Sync>>) -> Self {
        Self {
            task_id: task_id.into(),
            cleanup,
            fusion_prepared_summary: None,
            activation: None,
            fusion_activation: None,
        }
    }

    /// Attach the immutable summary captured during Fusion preparation.
    #[must_use]
    pub fn with_fusion_prepared_summary(mut self, summary: FusionPreparedSummary) -> Self {
        self.fusion_prepared_summary = Some(summary);
        self
    }

    /// Attach a one-shot activation invoked only after the registry has fully
    /// installed state, routing, cleanup, aliases, and creation hooks.
    #[must_use]
    pub fn with_activation<F>(mut self, activation: F) -> Self
    where
        F: FnOnce() + Send + 'static,
    {
        self.activation = Some(Box::new(activation));
        self
    }

    /// Attach a Fusion activation callback that receives the exact monotonic
    /// instant captured by the registry after TaskCreated hooks complete.
    #[must_use]
    pub fn with_fusion_activation<F>(mut self, activation: F) -> Self
    where
        F: FnOnce(FusionActivation) + Send + 'static,
    {
        self.fusion_activation = Some(Box::new(activation));
        self
    }

    /// Release the prepared worker exactly once. Handles without a barrier are
    /// already runnable, so this is a no-op for legacy handlers.
    pub fn activate(&mut self) {
        self.activate_at(FusionActivation::now());
    }

    /// Release the prepared worker using a caller-captured Fusion instant.
    /// Legacy activation callbacks remain source-compatible and simply ignore
    /// the supplied timestamp.
    pub fn activate_at(&mut self, activation: FusionActivation) {
        if let Some(activation_callback) = self.fusion_activation.take() {
            activation_callback(activation);
        } else if let Some(activation_callback) = self.activation.take() {
            activation_callback();
        }
    }
}

/// Errors produced by [`Task`] operations.
#[derive(Debug, Clone, Error)]
pub enum TaskError {
    /// Handler does not exist for the requested task type.
    #[error("unknown task type")]
    UnknownType,
    /// No task is registered with the given ID.
    #[error("task not found: {0}")]
    NotFound(String),
    /// Task already reached terminal status — cannot accept messages.
    #[error("terminated task — cannot accept messages")]
    TerminatedTask,
    /// Operation unsupported by the underlying handler.
    #[error("unsupported operation")]
    Unsupported,
    /// I/O failure.
    #[error("io: {0}")]
    Io(String),
    /// Internal handler error.
    #[error("internal: {0}")]
    Internal(String),
}
