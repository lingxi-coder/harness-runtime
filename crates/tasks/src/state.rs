//! Polymorphic task state — one variant per [`TaskType`](crate::id::TaskType).

use crate::id::TaskType;
use lingxi_core::types::AgentId;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::SystemTime;

/// Lifecycle state of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    /// Created but not yet started.
    Pending,
    /// Currently executing.
    Running,
    /// Checkpointed and waiting for an explicit resume.
    Paused,
    /// Finished successfully.
    Completed,
    /// Finished with an error.
    Failed,
    /// Killed by the user or the runtime.
    Killed,
}

impl TaskStatus {
    /// Whether the status represents a terminal (non-recoverable) state.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Killed)
    }
}

/// Fields shared by every task type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskStateBase {
    /// Task ID (e.g. `b3f9zk2x`).
    pub id: String,
    /// Task type discriminant.
    pub task_type: TaskType,
    /// Current status.
    pub status: TaskStatus,
    /// Human-readable description (shown in UI).
    pub description: String,
    /// Originating `tool_use_id` if launched from a tool call.
    pub tool_use_id: Option<String>,
    /// Wall-clock start time.
    pub start_time: SystemTime,
    /// Wall-clock end time, once terminal.
    pub end_time: Option<SystemTime>,
    /// Cumulative paused duration in milliseconds.
    pub total_paused_ms: u64,
    /// Path to the spool file accumulating stdout/stderr.
    pub output_file: PathBuf,
    /// Wall-clock deadline after which a NOTIFIED terminal row may be evicted
    /// from the registry — claude-code `evictAfter`, stamped as
    /// `Date.now() + 30_000` on the terminal transition
    /// (`ret`, `src_160988549.js` @2041938).
    ///
    /// `None` on a live row, and also on a resting `local_agent` that still owns
    /// live background children: the oracle declines to set a deadline there
    /// (`if(t.park&&keepaliveReasons.size>0)return`) so the parent outlives the
    /// children that still report to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evict_after: Option<SystemTime>,
    /// Last byte offset surfaced to the caller (for incremental reads).
    pub output_offset: u64,
    /// Whether the user has been notified of completion.
    pub notified: bool,
    /// Display name of the teammate / subagent that CREATED this task, if any.
    /// Additive + defaulted so older serialized rows remain valid.
    #[serde(default)]
    pub creator_teammate_name: Option<String>,
    /// Team name of the teammate / subagent that CREATED this task, if any.
    /// Additive + defaulted so older serialized rows remain valid.
    #[serde(default)]
    pub creator_team_name: Option<String>,
    /// Persistent agent id of the teammate / subagent that CREATED this task.
    /// Kept on the shared base so every background task type can participate
    /// in unnamed-parent rest deferral. Additive/defaulted for old rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creator_agent_id: Option<AgentId>,
}

/// Tagged union of per-type task states.
///
/// # `Serialize` but deliberately NOT `Deserialize`
///
/// This enum owns [`LocalWorkflowTaskState`], whose `scope` field is the
/// managed-app delete/lease authority and is `#[serde(skip)]`. A `Deserialize`
/// impl on this enum would therefore hand every future read-back path a row
/// whose `scope` is silently `None` -- no compile error, no test failure, and
/// a delete guard that quietly stops guarding. Dropping the derive turns that
/// silent default into a hard compile error at the read site
/// (`the trait bound `TaskState: Deserialize<'_>` is not satisfied`), which is
/// the only form of "you must handle scope here" that a future author cannot
/// walk past. `taskstate_scope_readback_tripwire::taskstate_must_not_implement_deserialize`
/// pins the absence, so re-adding the derive goes red naming this reason.
///
/// The write direction is untouched: serializing a task row cannot create
/// authority, and `#[serde(skip)]` keeps `scope` out of the bytes. When a real
/// persistence read seam is built, restore the read direction AT that seam and
/// re-mint `scope` there from state the Host re-resolves on load -- exactly
/// what [`crate::registry::TaskRegistry::register_adopted_workflow_with_scope`]
/// already does at the restart-adoption seam.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "task_type", rename_all = "snake_case")]
pub enum TaskState {
    /// Local bash command.
    LocalBash(LocalBashTaskState),
    /// Local agent.
    LocalAgent(LocalAgentTaskState),
    /// Remote agent.
    RemoteAgent(RemoteAgentTaskState),
    /// In-process teammate.
    InProcessTeammate(InProcessTeammateTaskState),
    /// Local workflow.
    LocalWorkflow(LocalWorkflowTaskState),
    /// MCP monitor.
    MonitorMcp(MonitorMcpTaskState),
    /// Shell stdout event monitor.
    Monitor(MonitorTaskState),
    /// Backgrounded MCP tool call (`mcp_task`).
    McpTask(McpTaskState),
    /// Dream loop.
    Dream(DreamTaskState),
    /// Environment scan owned by /auto-mode-setup.
    AutoModeScan(AutoModeScanTaskState),
    /// Fusion multi-model deliberation.
    LocalFusion(LocalFusionTaskState),
}

impl TaskState {
    pub(crate) fn handback(&self) -> Option<&lingxi_core::host::handback::HandbackState> {
        match self {
            Self::LocalAgent(agent) => agent.handback.as_ref(),
            Self::InProcessTeammate(agent) => agent.handback.as_ref(),
            _ => None,
        }
    }

    pub(crate) fn handback_slot(
        &mut self,
    ) -> Option<&mut Option<lingxi_core::host::handback::HandbackState>> {
        match self {
            Self::LocalAgent(agent) => Some(&mut agent.handback),
            Self::InProcessTeammate(agent) => Some(&mut agent.handback),
            _ => None,
        }
    }

    pub(crate) fn archive_handback(
        &mut self,
        previous: lingxi_core::host::handback::HandbackState,
    ) {
        if previous.report.is_none() && previous.receipt.is_none() {
            return;
        }
        match self {
            Self::LocalAgent(agent) => agent.handback_history.push(previous),
            Self::InProcessTeammate(agent) => agent.handback_history.push(previous),
            _ => {}
        }
    }

    pub(crate) fn handback_history(&self) -> &[lingxi_core::host::handback::HandbackState] {
        match self {
            Self::LocalAgent(agent) => &agent.handback_history,
            Self::InProcessTeammate(agent) => &agent.handback_history,
            _ => &[],
        }
    }

    /// Whether a completed local-agent turn retains a resumable runner.
    #[must_use]
    pub fn is_parked(&self) -> bool {
        matches!(self, Self::LocalAgent(agent) if agent.is_parked)
    }

    /// Terminal task status does not imply the persistent runner has exited.
    #[must_use]
    pub fn is_terminated(&self) -> bool {
        self.base().status.is_terminal() && !self.is_parked()
    }

    /// Borrow the common base fields regardless of variant.
    #[must_use]
    pub fn base(&self) -> &TaskStateBase {
        match self {
            Self::LocalBash(s) => &s.base,
            Self::LocalAgent(s) => &s.base,
            Self::RemoteAgent(s) => &s.base,
            Self::InProcessTeammate(s) => &s.base,
            Self::LocalWorkflow(s) => &s.base,
            Self::MonitorMcp(s) => &s.base,
            Self::Monitor(s) => &s.base,
            Self::McpTask(s) => &s.base,
            Self::Dream(s) => &s.base,
            Self::AutoModeScan(s) => &s.base,
            Self::LocalFusion(s) => &s.base,
        }
    }

    /// Mutably borrow the common base fields regardless of variant.
    #[must_use]
    pub fn base_mut(&mut self) -> &mut TaskStateBase {
        match self {
            Self::LocalBash(s) => &mut s.base,
            Self::LocalAgent(s) => &mut s.base,
            Self::RemoteAgent(s) => &mut s.base,
            Self::InProcessTeammate(s) => &mut s.base,
            Self::LocalWorkflow(s) => &mut s.base,
            Self::MonitorMcp(s) => &mut s.base,
            Self::Monitor(s) => &mut s.base,
            Self::McpTask(s) => &mut s.base,
            Self::Dream(s) => &mut s.base,
            Self::AutoModeScan(s) => &mut s.base,
            Self::LocalFusion(s) => &mut s.base,
        }
    }
}

/// State specific to a local bash task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalBashTaskState {
    /// This host adopted a supervised shell launched by an earlier host.
    #[serde(default)]
    pub is_adopted: bool,
    /// Origin of the shell invocation (oracle Bft/U6t): turn, agent or inner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller: Option<String>,
    /// Shared base fields.
    #[serde(flatten)]
    pub base: TaskStateBase,
    /// Bash command string.
    pub command: String,
    /// OS process ID once running.
    pub pid: Option<u32>,
    /// Exit code once terminated.
    pub exit_code: Option<i32>,
    /// Why an automatic lifetime guard stopped this shell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_cause: Option<String>,
    /// Directory the command was launched in (claude-code stores `cwd: Q()` on
    /// the `local_bash` record, 2.1.263 `Xne`). `#[serde(default)]` so records
    /// written before the field existed still parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Whether this shell is running in the background (claude-code
    /// `isBackgrounded`: `Xne` registers `true`, the 2 s foreground arming
    /// `U6t` registers `false`). Only a backgrounded shell is a task the model
    /// can address, so this is the field the `Stop` hook's `background_tasks`
    /// filter and `/tasks` read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_backgrounded: Option<bool>,
}

/// State specific to an in-process agent task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalAgentTaskState {
    /// Trusted plugin source captured at admission, independent of any
    /// rewritten spawn JSON.
    #[serde(default)]
    pub agent_spawn_provenance: lingxi_core::host::subagent_spawn::AgentSpawnProvenance,
    /// Direct Native list lifecycle facts reported by the runner/task registry.
    /// Missing fields remain unknown and are never derived from parked/status.
    #[serde(skip)]
    pub agent_list_lifecycle: lingxi_core::host::task_registry::AgentListLocalLifecycleFacts,
    /// Spawn-time description used by the agent-list identity projection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawned_description: Option<String>,
    /// Dynamic report state belongs to the current run, independently of the
    /// frozen launch request and immutable creator ancestry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handback: Option<lingxi_core::host::handback::HandbackState>,
    /// Prior admitted reports remain available after the next run resets its
    /// allowance. These snapshots contain sanitized text only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub handback_history: Vec<lingxi_core::host::handback::HandbackState>,
    /// The turn has completed while the persistent runner remains resumable.
    /// Separate from task status so parked agents are not counted as active work.
    #[serde(default)]
    pub is_parked: bool,
    /// An independent activity observer; never a user-facing completion.
    #[serde(default)]
    pub is_observer: bool,
    /// Activity source for a sidecar; separate from task ownership so ending the
    /// observed agent does not cascade-stop its independent observer.
    #[serde(default)]
    pub observed_agent_id: Option<AgentId>,
    /// Shared base fields.
    #[serde(flatten)]
    pub base: TaskStateBase,
    /// Target agent.
    pub agent_id: AgentId,
    /// Resolved subagent type label (one of the registered agent types, e.g.
    /// `general-purpose`). Independent sibling of [`Self::agent_id`] (which is a
    /// per-instance identity UUID). Mirrors claude-code `LocalAgentTaskState`'s
    /// `agentType` — surfaced verbatim as the `Stop` / `SubagentStop` hook
    /// `background_tasks[].agent_type` field (claude-code `Lic`'s `n.agentType`).
    /// `#[serde(default)]` so older on-disk task rows (pre-field) still parse.
    #[serde(default)]
    pub subagent_type: String,
    /// Initial prompt.
    pub prompt: String,
    /// Error message if the agent failed.
    pub error: Option<String>,
    /// Accumulated conversation messages.
    pub messages: Vec<lingxi_core::types::ConversationMessage>,
    /// Inbound messages queued for delivery.
    pub pending_messages: Vec<String>,
    /// Whether the agent is currently backgrounded.
    pub is_backgrounded: bool,
    /// The SKILL this agent IS, when a `context: fork` skill launched it
    /// (claude `forkedSkillName`). Threaded from
    /// `SubagentSpawnRequest::forked_skill_name` at spawn. Keys the
    /// live-duplicate guard (one live fork per skill) and is the task-record
    /// half of the identity a resume corroborates against the on-disk scoping
    /// record (`session::forked_skill`). `#[serde(default)]` so pre-field rows
    /// still parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_skill_name: Option<String>,
    /// What the run reported when it terminated — final text, usage, and the
    /// kept-worktree coordinates — plus `killed_by` once a stop names its
    /// initiator. Populated by
    /// [`TaskRegistryHandle::set_agent_outcome`](lingxi_core::host::task_registry::TaskRegistryHandle::set_agent_outcome)
    /// / `kill_with_reason` and read by the notification drain, which before
    /// this always rendered a `local_agent` completion with no `<result>`,
    /// `<usage>` or `<worktree>` and every stop as the bare `was stopped`.
    ///
    /// `#[serde(default)]` so pre-field on-disk task rows still parse.
    #[serde(default)]
    pub outcome: AgentOutcomeState,
}

/// [`LocalAgentTaskState::outcome`] — the terminal notification payload plus
/// the stop initiator.
///
/// [`lingxi_core::host::task_registry::AgentTerminalOutcome`] is the WRITE shape (what a
/// terminating run reports); this is the stored shape, which additionally holds
/// `killed_by` because that arrives from the kill path rather than from the
/// run.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentOutcomeState {
    /// Concrete model and string effort selected for the current runner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,

    /// Final text response → the notification's `<result>` section.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// Run usage → the `<usage>` section.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<lingxi_core::host::task_registry::AgentRunUsage>,
    /// Who stopped the task (`"parent"` / `"user"`) → the killed-summary verb.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub killed_by: Option<String>,
    /// Kept isolation worktree path → gates and fills `<worktree>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    /// Kept isolation worktree branch → `<worktreeBranch>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_branch: Option<String>,
    /// Turn budget the run exhausted → the turn-limit `completed` summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns_reached: Option<u64>,
    /// Launch metadata stamped by the spawning tool, so the KILL path (which
    /// knows `killed_by` but not the launch context) can report it.
    #[serde(default)]
    pub agent_depth: Option<u32>,
    /// See [`Self::agent_depth`].
    #[serde(default)]
    pub is_built_in: Option<bool>,
}

impl AgentOutcomeState {
    /// Start a fresh turn-set while preserving immutable launch/display data.
    pub fn reset_run(&mut self) {
        self.result = None;
        self.usage = None;
        self.killed_by = None;
        self.max_turns_reached = None;
    }
    /// Merge a terminating run's report in. A `Some` field overwrites; a `None`
    /// leaves the stored value alone, so a later partial report (e.g. a kill
    /// that only carries a worktree) never erases an earlier result.
    pub fn merge(&mut self, incoming: lingxi_core::host::task_registry::AgentTerminalOutcome) {
        let lingxi_core::host::task_registry::AgentTerminalOutcome {
            handback: _,
            result,
            usage,
            error: _,
            worktree_path,
            worktree_branch,
            max_turns_reached,
            agent_depth,
            is_built_in,
        } = incoming;
        if result.is_some() {
            self.result = result;
        }
        if usage.is_some() {
            self.usage = usage;
        }
        if worktree_path.is_some() {
            self.worktree_path = worktree_path;
        }
        if worktree_branch.is_some() {
            self.worktree_branch = worktree_branch;
        }
        if max_turns_reached.is_some() {
            self.max_turns_reached = max_turns_reached;
        }
        if agent_depth.is_some() {
            self.agent_depth = agent_depth;
        }
        if is_built_in.is_some() {
            self.is_built_in = is_built_in;
        }
    }
}

/// State specific to a remote agent task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteAgentTaskState {
    /// Shared base fields.
    #[serde(flatten)]
    pub base: TaskStateBase,
    /// Remote session identifier.
    pub remote_session_id: String,
    /// Endpoint URL.
    pub remote_endpoint: String,
}

/// State specific to an in-process teammate task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InProcessTeammateTaskState {
    /// Concrete model route captured from the final admitted SubagentContext.
    /// Host-only; never persist a provider route as task wire data.
    #[serde(skip)]
    pub child_model: Option<String>,
    /// Explicitly resolved profile for `child_model`, when unambiguous.
    /// Host-only and omitted from serialized state.
    #[serde(skip)]
    pub child_model_profile: Option<String>,
    /// Trusted plugin source captured at admission, independent of any
    /// rewritten spawn JSON.
    #[serde(default)]
    pub agent_spawn_provenance: lingxi_core::host::subagent_spawn::AgentSpawnProvenance,
    /// Spawn-time agent type and description, when the caller supplied them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawned_agent_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawned_description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handback: Option<lingxi_core::host::handback::HandbackState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub handback_history: Vec<lingxi_core::host::handback::HandbackState>,
    /// The persistent runner has finished its turn-set and is awaiting work.
    #[serde(default)]
    pub is_idle: bool,
    /// Waiting for the lead to review a submitted plan; independent of idle.
    #[serde(default)]
    pub awaiting_plan_approval: bool,
    /// Shared base fields.
    #[serde(flatten)]
    pub base: TaskStateBase,
    /// Teammate agent ID.
    pub agent_id: AgentId,
    /// Inbound messages queued for delivery.
    pub pending_messages: Vec<String>,
}

/// State specific to a local workflow task.
///
/// `Serialize` only -- see [`TaskState`]'s doc comment and [`Self::scope`].
/// The missing `Deserialize` is the tripwire, not an oversight.
#[derive(Debug, Clone, Serialize)]
pub struct LocalWorkflowTaskState {
    /// Shared base fields.
    #[serde(flatten)]
    pub base: TaskStateBase,
    /// Session that owns this workflow row. Launchers persist it for stable
    /// late-work budget/transcript ownership; the desktop registry may keep
    /// TaskList and `/workflows` unfiltered while mobile scopes them live.
    #[serde(default)]
    pub session_uuid: Option<String>,
    /// Workflow identifier.
    pub workflow_id: String,
    /// The model-authored workflow script source (carried for resume).
    #[serde(default)]
    pub script: String,
    /// Prior run id to resume journaled `agent()` results from, if any.
    #[serde(default)]
    pub resume_from_run_id: Option<String>,
    /// The `args` global value (JSON string), if any.
    #[serde(default)]
    pub args: Option<String>,
    /// The EFFECTIVE run id (`wf_…`) this workflow executes under — the
    /// launcher-minted id for a fresh run, or the resumed id. Stored so the
    /// resume gate (claude-code validateInput errorCode 3) can detect a
    /// `resumeFromRunId` that names a still-running workflow.
    #[serde(default)]
    pub run_id: Option<String>,
    /// Persisted script path used to explicitly resume an adopted workflow.
    #[serde(default)]
    pub script_path: Option<String>,
    /// Directory containing this run's append-only `journal.jsonl`.
    #[serde(default)]
    pub transcript_dir: Option<PathBuf>,
    /// Index of the currently-executing step.
    pub current_step: usize,
    /// Terminal result/failure/usage payload for workflow notifications.
    #[serde(default)]
    pub outcome: lingxi_core::host::task_registry::WorkflowTerminalOutcome,
    /// Typed managed-app workflow authority for this run (design §18 Phase -1
    /// step 8 / §8.1) -- which app this task may touch, and why. Read by the
    /// workspace-lease and App-delete guards INSTEAD of `workflow_id`/`args`;
    /// see [`crate::scope`]'s module docs for the whole design.
    ///
    /// Copied verbatim from
    /// [`crate::task_trait::TaskSpawnInput::LocalWorkflow`]'s `scope` field by
    /// `state_for_spawn`; see that field's doc comment for what a `Some`
    /// proves and who is allowed to mint one. Nothing in this crate derives
    /// it from `workflow_id` or `args`.
    ///
    /// `None` when no scope was minted for this task -- every workflow that
    /// is not a managed-app workflow, and any managed-app launch the Host could
    /// not fully validate. Both guards treat `None` as "no authority", not as
    /// "assume the worst": a `None` row never takes the workspace lease and
    /// never blocks an App's delete. The alternative -- granting authority to
    /// an unscoped row -- is exactly the vulnerability this type exists to
    /// close (a forged custom workflow reusing a real build workflow's name
    /// is ALSO unscoped, and is indistinguishable from a legitimate one at
    /// this layer), so denying by default is the only choice that does not
    /// reopen it. See
    /// `a_custom_workflow_with_the_same_name_gets_no_lease_and_does_not_block_delete`
    /// and `a_spawned_workflows_scope_is_what_blocks_its_apps_delete` in
    /// `registry_test.rs`.
    ///
    /// `#[serde(skip)]`, not `#[serde(default)]`: [`crate::scope::ManagedWorkflowScope`]
    /// deliberately implements `Serialize` and NOT `Deserialize` (see its
    /// module docs' `serde surface` section) -- so this field cannot be
    /// read back from persisted bytes at all today, by construction, not by
    /// omission. `#[serde(skip)]` (rather than `#[serde(skip_deserializing)]`,
    /// which would still serialize it out) matches the FIRST of the two
    /// acceptable shapes that module documents: nothing in this workspace
    /// currently deserializes `TaskState` (this is a new seam, not an
    /// existing one), so there is no reader for persisted scope bytes to
    /// serve yet, and shipping a scope's bytes to disk before any reader
    /// exists to validate provenance would be speculative. When a real
    /// persistence seam is built, the Host should re-mint the scope from the
    /// binding it resolves on load, not read it back from JSON --
    /// [`crate::registry::TaskRegistry::register_adopted_workflow_with_scope`]
    /// is the existing precedent for exactly that (it re-mints `scope` at the
    /// restart-ADOPTION seam, the one persistence-adjacent read-back path
    /// this crate has today, which does not go through `Deserialize` at all).
    ///
    /// # Why that is a MECHANISM here, not just advice
    ///
    /// `#[serde(skip)]` only requires `Default` on the field's type, which
    /// `Option<_>` always has -- so a `Deserialize` impl on [`TaskState`]
    /// would compile clean and read every row back with `scope: None`,
    /// silently reverting the delete guard to the pre-scope hole. So
    /// [`TaskState`] and [`LocalWorkflowTaskState`] do not implement
    /// `Deserialize` AT ALL: a future read-back path is a COMPILE ERROR at
    /// its own call site, not a silent `None`. `taskstate_scope_readback_tripwire`
    /// (this module, below) pins that absence so re-adding the derive goes
    /// red naming this field and the guard it protects.
    #[serde(skip)]
    pub scope: Option<crate::scope::ManagedWorkflowScope>,
}

/// State specific to an MCP monitor task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorMcpTaskState {
    /// Shared base fields.
    #[serde(flatten)]
    pub base: TaskStateBase,
    /// Server name being monitored.
    pub server_name: String,
    /// Resource URIs watched.
    pub watch_resources: Vec<String>,
}

/// State specific to a shell stdout event monitor (`monitor_ws`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorTaskState {
    /// Shared base fields.
    #[serde(flatten)]
    pub base: TaskStateBase,
    /// Shell command being monitored.
    pub command: String,
    /// Exit code once the command terminates.
    pub exit_code: Option<i32>,
    /// Bytes the script wrote to STDOUT over the monitor's life (claude-code
    /// `taskOutput.pipedStdoutBytes`). `Some(0)` is what selects the
    /// "ended without producing output" completion summary, so the distinction
    /// from `None` — "never measured", e.g. an MCP monitor that has no stdout
    /// at all — is load-bearing and must not collapse to a defaulting `0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_bytes: Option<u64>,
}

/// State specific to a backgrounded MCP tool call (claude-code `mcp_task`,
/// minted by `callMcpToolWithAutoBackground`/`NZu`). Distinct from
/// [`MonitorMcpTaskState`], which watches a whole server; this tracks one
/// detached `tools/call`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTaskState {
    /// Full-result persistence receipt for the terminal notification.
    #[serde(default)]
    pub saved_hint: Option<String>,

    /// Shared base fields.
    #[serde(flatten)]
    pub base: TaskStateBase,
    /// MCP server name (`serverName`).
    pub server_name: String,
    /// MCP tool name (`toolName`).
    pub tool_name: String,
    /// Coarse MCP task status (`mcpStatus`): `"working"` | `"input_required"`
    /// | `"completed"` | `"cancelled"` | `"failed"`. Defaults to `"working"`.
    pub mcp_status: String,
    /// Latest human-readable status line (`statusMessage`), if any.
    pub status_message: Option<String>,
    /// The settled call's result text (`resultText`), kept so the terminal
    /// notification can inline it in `<result>` the way claude-code's `F` does.
    /// The spool copy is still written — that is what `TaskOutput` reads — but
    /// a notification cannot go and read a file, so the text has to be here too.
    /// `None` until the call settles, and on a call that failed or was
    /// cancelled (those render from `status_message` instead).
    pub result_text: Option<String>,
}

/// State specific to a dream task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DreamTaskState {
    /// Shared base fields.
    #[serde(flatten)]
    pub base: TaskStateBase,
    /// Number of iterations performed so far.
    pub iteration_count: u32,
    /// Optional cap on iterations.
    pub max_iterations: Option<u32>,
}

/// State specific to a Fusion deliberation task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalFusionTaskState {
    /// Shared base fields.
    #[serde(flatten)]
    pub base: TaskStateBase,
    /// Parent conversation id for the completion sink.
    pub conversation_id: String,
    /// Task prompt.
    pub prompt: String,
    /// `fu_…` run id once the orchestrator mints it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// `quality` or `fast`.
    pub preset: String,
    /// Whether panels may leave the parent provider.
    pub cross_provider: bool,
    /// Sanitized final text for TaskNotification `<result>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_text: Option<String>,
    /// Failure reason for a `failed` run — folded into the `<error>` section
    /// and the failed-summary text. `None` for a run that never failed (or
    /// hasn't reached a terminal status yet).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Provider profiles that received prompt data on this run → the
    /// `<egress-profiles>` notification line. Never includes model names.
    #[serde(default)]
    pub egress_profiles: Vec<String>,
    /// Aggregate run usage summary for the `<usage>` notification section.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<lingxi_core::host::task_registry::AgentRunUsage>,
    /// Current progress-stage label (F005), e.g. "Running panels 2/3" — the
    /// SAME text `FusionStage::label()` produces for the Agent-tool path, so
    /// the `/fusion` task DTO's progress reads identically. Additive; `None`
    /// until the first `FusionProgress` event lands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    /// Effective end-to-end timeout captured for this run before task
    /// publication. The print-mode waiter uses this snapshot rather than
    /// reloading live settings, so a later config edit cannot shorten or
    /// lengthen an already-running Fusion task's wait budget. `None` is kept
    /// for legacy/external task producers that do not expose the seam.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_timeout_ms: Option<u64>,
    /// Exact panel count resolved before this task was published, when the
    /// executor exposed one during preparation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planned_panels: Option<u8>,
    /// Monotonic deadline captured at registry activation. This is deliberately
    /// in-memory only: persisted rows use `base.start_time` as a legacy
    /// wall-clock fallback because a monotonic instant has no process-stable
    /// representation.
    #[serde(skip)]
    pub fusion_activation_deadline: Option<tokio::time::Instant>,
    /// Truthful parent-session publication state, independent from the
    /// computational terminal status. New runs begin `Pending` and only a
    /// sink receipt can move this to a terminal state.
    #[serde(default)]
    pub publication_status: lingxi_core::host::FusionPublicationStatus,
    /// Sanitized publication/outbox failure detail, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication_error: Option<String>,
}

/// Tripwire for the residual documented on
/// [`LocalWorkflowTaskState::scope`].
///
/// # The residual
///
/// `scope` is `#[serde(skip)]`. `#[serde(skip)]` only requires `Default` on
/// the field's type -- which `Option<_>` always has -- so a `Deserialize` impl
/// on [`TaskState`] would compile happily and hand every read-back row
/// `scope: None`. No compile error, no failing test, and the managed-app delete
/// guard (`crate::registry::TaskRegistry::find_nonterminal_managed_workflows`)
/// silently stops guarding: exactly the gap
/// `crate::registry::TaskRegistry::register_adopted_workflow_with_scope`
/// closes at the restart-adoption seam, reopened by one derive.
///
/// # The mechanism
///
/// [`TaskState`] and [`LocalWorkflowTaskState`] do not implement
/// `Deserialize` at all. A future persistence seam that tries to read a task
/// row back therefore does not silently get `None` -- it does not COMPILE
/// (`the trait bound `TaskState: Deserialize<'_>` is not satisfied`), which
/// forces the author to the doc comments above and to re-mint `scope` at the
/// new seam. A textual scan for `from_str::<TaskState>` and friends was
/// considered and rejected: the canonical Rust spelling of a read-back is
/// `let row: TaskState = serde_json::from_str(&bytes)?;`, which names no
/// turbofish and would walk straight past any such grep.
///
/// The test below pins the ABSENCE of the impl, because absence is the thing
/// that can be undone by one word. It uses the same autoref-specialization
/// probe [`crate::scope`]'s `scope_type_does_not_implement_deserialize` uses,
/// for the same reason: once the impl is gone, a `serde_json::from_str::<T>`
/// assertion cannot even be written, so it could never be the regression test.
#[cfg(test)]
mod taskstate_scope_readback_tripwire {
    use super::*;

    /// `implements_deserialize!(T)` is `true` iff `T: DeserializeOwned`,
    /// WITHOUT requiring that bound at the call site. The specialized impl
    /// sits on `&Probe<T>` behind the bound and the fallback on `Probe<T>`;
    /// the call site passes `&&Probe<T>` so method resolution stops at the
    /// first deref step that has a candidate. (Mirrors `crate::scope`'s
    /// `de_probe` -- see that module for the full explanation.)
    mod de_probe {
        use serde::de::DeserializeOwned;
        use std::marker::PhantomData;

        pub struct Probe<T>(pub PhantomData<T>);

        pub trait ProbeFallback {
            fn implements_deserialize(&self) -> bool {
                false
            }
        }
        impl<T> ProbeFallback for Probe<T> {}

        pub trait ProbeSpecialized {
            fn implements_deserialize(&self) -> bool {
                true
            }
        }
        impl<T: DeserializeOwned> ProbeSpecialized for &Probe<T> {}
    }

    macro_rules! implements_deserialize {
        ($t:ty) => {{
            #[allow(unused_imports)]
            use de_probe::{ProbeFallback as _, ProbeSpecialized as _};
            (&&de_probe::Probe::<$t>(::std::marker::PhantomData)).implements_deserialize()
        }};
    }

    /// THE TRIPWIRE. Re-derive `Deserialize` on [`TaskState`] or
    /// [`LocalWorkflowTaskState`] and this test goes red naming the field and
    /// the guard that quietly stops guarding.
    ///
    /// The controls are load-bearing, not decoration: [`TaskStateBase`] and
    /// [`LocalBashTaskState`] DO keep `Deserialize` (nothing about them is
    /// authority), so a `false` for the two types above is a measurement and
    /// not a probe that never fires.
    #[test]
    fn taskstate_must_not_implement_deserialize() {
        assert!(
            implements_deserialize!(TaskStateBase),
            "probe control: TaskStateBase keeps Deserialize, so the probe can answer true"
        );
        assert!(
            implements_deserialize!(LocalBashTaskState),
            "probe control: a task-state struct with no scope field keeps Deserialize"
        );

        assert!(
            !implements_deserialize!(LocalWorkflowTaskState),
            "LocalWorkflowTaskState must NOT implement Deserialize while its \
             `scope` field is #[serde(skip)]: a derive is one word, it compiles \
             clean, and every row it reads back gets `scope: None` -- silently \
             reverting the delete guard \
             (registry::TaskRegistry::find_nonterminal_managed_workflows) to \
             the pre-scope hole that \
             register_adopted_workflow_with_scope closes. Before adding a \
             read-back path, re-mint `scope` AT that seam from state the Host \
             re-resolves on load; see LocalWorkflowTaskState::scope's doc \
             comment and this module's docs."
        );
        assert!(
            !implements_deserialize!(TaskState),
            "TaskState must NOT implement Deserialize: it owns \
             LocalWorkflowTaskState, so deserializing the enum is a read-back \
             path for the #[serde(skip)] `scope` field just as much as \
             deserializing the struct directly. See this module's docs."
        );
    }

    /// The write direction is deliberately UNCHANGED by the tripwire: a task
    /// row still serializes, and `scope` is still absent from the bytes. If
    /// this ever fails, the tripwire was implemented by breaking persistence
    /// rather than by removing the read direction.
    #[test]
    fn taskstate_still_serializes_and_still_omits_scope() {
        let state = TaskState::LocalWorkflow(LocalWorkflowTaskState {
            base: TaskStateBase {
                id: "w12345678".to_string(),
                task_type: TaskType::LocalWorkflow,
                status: TaskStatus::Paused,
                description: "fixture".to_string(),
                tool_use_id: None,
                start_time: SystemTime::UNIX_EPOCH,
                end_time: None,
                total_paused_ms: 0,
                output_file: PathBuf::from("/dev/null"),
                evict_after: None,
                output_offset: 0,
                notified: false,
                creator_teammate_name: None,
                creator_team_name: None,
                creator_agent_id: None,
            },
            session_uuid: None,
            workflow_id: "fixture-workflow".to_string(),
            script: String::new(),
            resume_from_run_id: None,
            args: None,
            run_id: None,
            script_path: None,
            transcript_dir: None,
            current_step: 0,
            outcome: Default::default(),
            scope: crate::scope::ManagedWorkflowScope::for_build("some-app").ok(),
        });
        let json = serde_json::to_string(&state).expect("a task row still serializes");
        assert!(
            !json.contains("scope"),
            "`scope` must stay #[serde(skip)] -- it is never written to disk. Got: {json}"
        );
        assert!(
            json.contains("\"workflow_id\":\"fixture-workflow\""),
            "the rest of the row must still serialize normally. Got: {json}"
        );
    }
}

/// A cancellable environment scan; it never produces a conversation completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoModeScanTaskState {
    /// Shared task identity and lifecycle.
    #[serde(flatten)]
    pub base: TaskStateBase,
}
