//! `TaskRegistryHandle` — narrow trait abstracting `TaskRegistry` CRUD so the
//! six `Task*` tools in `lingxi-tools` can dispatch into the production
//! registry without taking a cyclic dep on `lingxi-tasks`.
//!
//! Concrete impl lives in `lingxi-tasks`. Tests inject an in-memory mock.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Input to [`TaskRegistryHandle::create`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskCreateInput {
    /// Wire string for the task type — one of TEN: the nine claude-code
    /// variants (`local_bash`, `local_agent`, `remote_agent`,
    /// `in_process_teammate`, `local_workflow`, `monitor_mcp`, `monitor_ws`,
    /// `mcp_task`, `dream`) plus LingXi's own `local_fusion`. The oracle's
    /// tenth, `auto_mode_scan` (prefix `e`), tracks environment reconnaissance.
    pub task_type: String,
    /// Human-readable description shown in UI listings.
    pub description: String,
}

/// Registration input for a backgrounded MCP tool call (claude-code 2.1.212
/// `callMcpToolWithAutoBackground`'s `NZu` register-input builder). Minted when
/// a single `tools/call` exceeds `getMcpAutoBackgroundMs` and is detached from
/// the turn.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpTaskRegistration {
    /// MCP server name (`serverName`).
    pub server_name: String,
    /// MCP tool name (`toolName`).
    pub tool_name: String,
    /// Originating assistant `tool_use_id`, if any (`toolUseId`).
    pub tool_use_id: Option<String>,
    /// Creator ownership used to defer a resting parent's notification.
    pub creator_teammate_name: Option<String>,
    /// Team containing the creator, when it belongs to one.
    pub creator_team_name: Option<String>,
    /// Persistent identity of the creator agent, when available.
    pub creator_agent_id: Option<crate::types::AgentId>,
}

/// Launch input for a background stdout event monitor.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MonitorRegistration {
    /// Shell command whose stdout lines become events.
    pub command: String,
    /// Human-readable description repeated in notifications.
    pub description: String,
    /// Effective timeout in milliseconds. `0` means session-persistent.
    pub timeout_ms: u64,
    /// Whether the monitor lives until TaskStop/session teardown.
    pub persistent: bool,
    /// Invocation working directory.
    pub cwd: Option<String>,
    /// The command as it should actually be SPAWNED, when that differs from
    /// [`Self::command`] — i.e. the sandbox-wrapped form.
    ///
    /// claude-code passes `shouldUseSandbox: jS({command})` to the shared shell
    /// entry point (`src_168769646.js` @9633) and lets it wrap; the port's
    /// Monitor has its own spawn path, so the decision is made at the tool and
    /// the wrapped string travels here. [`Self::command`] stays the raw text so
    /// `/tasks` and the notifications keep showing what the model asked for.
    /// `None` ⇒ spawn `command` unwrapped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_command: Option<String>,
    /// Originating assistant tool-use id, when available.
    pub tool_use_id: Option<String>,
    /// Creator ownership used to defer a resting parent's notification.
    pub creator_teammate_name: Option<String>,
    /// Team containing the creator, when it belongs to one.
    pub creator_team_name: Option<String>,
    /// Persistent identity of the creator agent, when available.
    pub creator_agent_id: Option<crate::types::AgentId>,
}

/// Passive WebSocket event monitor registration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebSocketMonitorRegistration {
    /// Public ASCII ws/wss endpoint.
    pub url: String,
    /// Optional ordered RFC 6455 subprotocols.
    pub protocols: Vec<String>,
    /// Common task metadata; command/cwd fields are unused for sockets.
    pub task: MonitorRegistration,
}

/// Filter for [`TaskRegistryHandle::list`].
/// A background shell command the caller is about to spawn.
///
/// claude-code registers a `local_bash` record for the SAME identity its shell
/// spawn already minted (`Xne`, 2.1.263 `src_160988549.js` @4281167), so the
/// `backgroundTaskId` handed to the model resolves in `TaskOutput`, `TaskStop`,
/// `TaskList` and `/tasks`, and its completion produces a
/// `<task-notification>`. The port mints the identity through the registry
/// (which owns the id space and the output directory) and then hands it to the
/// process runner.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackgroundBashRegistration {
    /// The command as the model wrote it; stored verbatim on the record.
    pub command: String,
    /// Human-readable description used in the completion summary
    /// (`Background command "{description}" completed (exit code N)`).
    pub description: String,
    /// Originating tool-use id, surfaced as the notification's `<tool-use-id>`.
    pub tool_use_id: Option<String>,
    /// Working directory the command was launched in.
    pub cwd: Option<String>,
    /// The agent that launched it, when a subagent did. claude-code stores this
    /// as the record's `agentId` and uses it to decide who a completion belongs
    /// to; a `None` here means the main session owns the task.
    pub creator_agent_id: Option<crate::types::AgentId>,
}

/// The identity the registry minted for a background shell command.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackgroundBashHandle {
    /// Registry task id (`b` + 8 base-36 characters).
    pub task_id: String,
    /// Absolute path of the task's output file, already created.
    pub output_path: String,
}

/// Terminates a task whose process the registry does not own.
#[async_trait]
pub trait TaskKiller: Send + Sync {
    /// Kill the underlying OS process. Best-effort.
    async fn kill(&self);
}

/// The rosters claude-code appends to a TaskStop / TaskOutput "no task found"
/// message so the model can see what it COULD have addressed.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskNotFoundRosters {
    /// `bjn` — running `in_process_teammate` rows, by their addressable
    /// identity (`name@team` where one exists).
    pub running_teammates: Vec<String>,
    /// `JFe` — running backgrounded `local_agent` rows that are NOT in the
    /// agent-name registry, rendered `{id} ({description})` or a bare id.
    pub background_agents: Vec<String>,
}

/// Registry identity and output path for an allocated foreground agent.
#[derive(Debug, Clone)]
pub struct ForegroundAgentHandle {
    pub task_id: String,
    pub output_path: String,
}

/// Metadata for a foreground agent allocated by the Agent tool.
#[derive(Debug, Clone)]
pub struct ForegroundAgentRegistration {
    pub agent_id: crate::types::AgentId,
    pub agent_type: String,
    pub prompt: String,
    pub description: String,
    pub tool_use_id: Option<String>,
    pub creator_agent_id: Option<crate::types::AgentId>,
    pub creator_teammate_name: Option<String>,
    pub creator_team_name: Option<String>,
}

/// Delivers an intentional message to a retained externally driven agent.
#[async_trait]
pub trait TaskMessageReceiver: Send + Sync {
    async fn send(&self, message: String) -> Result<(), TaskRegistryError>;
}

/// Moves a still-running FOREGROUND task to the background on request.
///
/// The port of claude-code `I_t`'s first act, `t.background(e)` on the live
/// `shellCommand`. The registry holds one of these per armed foreground row so
/// Ctrl+B, background-all and the SDK `background_tasks` request can reach a
/// child the registry did not spawn.
/// Why foreground work is being detached from its owning turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TaskBackgroundReason {
    /// Explicit Ctrl+B/background-all request.
    User = 1,
    /// Preserve work while the foreground turn is cancelled.
    TurnAbort = 2,
    /// Allow an arriving message to reach the model without interrupting work.
    DeliverMessage = 3,
}

#[async_trait]
pub trait TaskBackgrounder: Send + Sync {
    /// Ask the in-flight command to detach. Best-effort and idempotent: the
    /// runner takes the same path a timeout would.
    async fn background(&self);
    /// Preserve reason-aware result text; old backgrounders remain compatible.
    async fn background_with_reason(&self, _reason: TaskBackgroundReason) {
        self.background().await;
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskListFilter {
    /// Optional status filter — one of the 5 byte-locked status strings.
    pub status: Option<String>,
}

/// Patch shape for [`TaskRegistryHandle::update`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskUpdatePatch {
    /// New status, if changed.
    pub status: Option<String>,
}

/// TaskStop lookup preserves ambiguity rather than choosing a namesake.
#[derive(Debug, Clone)]
pub enum TaskStopResolution {
    Found(TaskRecord),
    Ambiguous(String),
    NotFound { suggestion: Option<String> },
}

/// One task as surfaced to the tool layer.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskRecord {
    /// Running shell inherited from an acknowledged host handoff.
    #[serde(default)]
    pub is_adopted: bool,
    /// Shell invocation origin, retained when a foreground shell backgrounds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller: Option<String>,
    /// Whether a completed agent satisfies the task-dialog Cj predicate.
    #[serde(default)]
    pub completed_agent_visible: bool,
    /// Completion has been delivered or consumed.
    #[serde(default)]
    pub notified: bool,
    /// Concrete agent model and optional string effort for task presentation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,

    /// An internal observer sidecar, excluded from user-facing task notices.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_observer: bool,
    /// Teammate address (`name@team`) used by TaskStop name resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub teammate_agent_id: Option<String>,
    /// Teammate display name, independent of the task description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub teammate_name: Option<String>,
    /// Shell specialization; command event monitors use `monitor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// A completed local agent whose persistent runner is still resumable.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_parked: bool,
    /// Authoritative persistent-teammate idle state, used by goal deferral.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_idle: bool,
    /// A persistent teammate is waiting for the leader's plan decision.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub awaiting_plan_approval: bool,
    /// 9-char `[bartwmdksfe][0-9a-z]{8}` task id.
    pub task_id: String,
    /// Task type wire string.
    pub task_type: String,
    /// Status wire string.
    pub status: String,
    /// Human-readable description.
    pub description: String,
    /// Wall-clock start as Unix epoch milliseconds, when known. This feeds
    /// the `subagentStatusLine.tasks[].startTime` payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    /// The shell command, for `local_bash` tasks only (claude-code
    /// `LocalShellTaskState.command`). `None` for every other task type. The
    /// `TaskStop` tool surfaces this in preference to `description` for
    /// `local_bash`, mirroring claude-code `stopTask.ts:97`
    /// (`isLocalShellTask(task) ? task.command : task.description`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// `local_agent` only: the agent's type label, surfaced as the
    /// `background_tasks[].agent_type` field of a `Stop` / `SubagentStop` hook
    /// payload (claude-code `Lic`'s `r.agent_type = n.agentType`). `None` for
    /// every other task type. Additive default `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// claude-code `task.agentId` — the agent that OWNS this background task,
    /// and the sole non-main-session caller `TaskStop` will let stop it
    /// (`sut(callerAgentId, task.agentId)`, `src_160988549.js` @3386470).
    ///
    /// `local_bash`: the SPAWNING agent. `local_agent`: the agent's OWN id —
    /// deliberately NOT its creator, which is the oracle's separate
    /// `ownerAgentId` that `sut` does not read. Every other task type carries no
    /// `agentId` upstream, so this stays `None` and only the main session may
    /// stop them.
    ///
    /// Only `local_agent` runner identities are lowered as client task-row
    /// `agent_id`; shell creator identities remain internal. Additive default `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_agent_id: Option<String>,
    /// Who stopped this task, when something did — `"user"` or `"parent"`.
    ///
    /// `local_agent` only, and the port's stand-in for claude-code's separate
    /// `stoppedByUser` boolean: a USER stop means the work was deliberately
    /// cancelled and must not be silently resumed, where a model/parent stop
    /// carries no such intent. `None` on a task nothing stopped.
    ///
    /// The two are not the same axis upstream — `stoppedByUser` is set by a
    /// helper (`LV`) that a budget-halt or teardown stop-all also calls, with
    /// `source: "system"`. This port has no such path, so the kill reason is a
    /// faithful witness today; if one is ever added it must set the flag
    /// independently of the reason.
    ///
    /// The gate this feeds is claude-code `z2t`
    /// (`src_160988549.js` @2030017): `local_agent` only, refuse when
    /// `stoppedByUser`, and a `userInitiated` resume bypasses it outright. Its
    /// two companions are deliberately NOT ported, because nothing in this port
    /// can produce their input:
    ///
    /// * `userStopCount` / `userInitiatedAt` (`V2t` / `K2t`) stamp each queued
    ///   message with the stop epoch it was enqueued in, so a user stop
    ///   invalidates older user messages. Persistent observer/notification
    ///   queues now exist here, but carry only automatic/model-originated work.
    /// * the `userInitiated` bypass needs a USER-typed message aimed at an
    ///   agent. The port has no trusted human agent-address input surface;
    ///   `SendMessage`, mailbox delivery and observer digests cannot assert it.
    ///   Upstream DOES produce this flag: `src_180597926.js` @1393158 calls
    ///   resume with `userInitiated:true`; @1393281 queues a typed message when
    ///   the resume lock is busy. Porting that input surface requires the epoch
    ///   guard too, not simply granting existing model messages a bypass.
    ///
    /// Additive default `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub killed_by: Option<String>,
    /// `monitor_mcp` / `mcp_task` only: the MCP server name, surfaced as the
    /// `background_tasks[].server` field (claude-code `Lic`'s `r.server`).
    /// `None` for every other task type. Additive default `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// `monitor_mcp` / `mcp_task` only: the MCP tool name, surfaced as the
    /// `background_tasks[].tool` field (claude-code `Lic`'s `r.tool`). The port
    /// `MonitorMcpTaskState` carries no per-tool name (it watches resources, not
    /// a single tool), so this stays `None` for `monitor_mcp`; the `mcp_task`
    /// type (`McpTaskState`) DOES carry a single `tool_name` and populates it.
    ///
    /// The oracle's `monitor_mcp` record DOES carry one — its projection is
    /// `o.server=r.server, o.tool=r.tool`, the same pair as `mcp_task`. Nothing
    /// creates such a record in 2.1.263 either, so neither side has a value to
    /// disagree about; a future producer here would have to fill both.
    /// Additive default `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// `local_workflow` only: the workflow name, surfaced as the
    /// `background_tasks[].name` field (claude-code `Lic`'s
    /// `r.name = n.workflowName`). The port carries a `workflow_id` rather than a
    /// separate display name, so the id is used. `None` for every other task
    /// type. Additive default `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `local_agent` only: the skill this agent IS, when a `context: fork`
    /// skill launched it (claude `forkedSkillName`). Keys the live-duplicate
    /// guard — a skill already running as a live fork does not fork again — and
    /// is the task-record half of the identity a resume corroborates against
    /// the on-disk scoping record. Additive default `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_skill_name: Option<String>,
    /// `local_agent` only: whether the agent is currently backgrounded, used by
    /// the `Stop` / `SubagentStop` `background_tasks` filter (claude-code `wA`:
    /// drop a task when `"isBackgrounded" in e && e.isBackgrounded === false`).
    /// Only `local_agent` tasks carry an `isBackgrounded` field in claude-code,
    /// so this is `None` for every other task type (and such tasks are never
    /// dropped by that filter clause). Additive default `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_backgrounded: Option<bool>,
    /// Terminal failure reason for a task whose `status` is `failed`, when the
    /// producing handler reported one. `local_workflow` fills it from
    /// [`WorkflowTerminalOutcome::error`] and `local_agent` from the agent's
    /// recorded error; every other task type leaves it `None`. Appended field
    /// (default `None`) — the clients render it next to `description` so a
    /// failed background task is not a bare id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// `local_fusion` only (F005): the run's current progress-stage label —
    /// e.g. "Running panels 2/3" — the SAME text `FusionStage::label()`
    /// produces for the Agent-tool path's `subagent_activity` forwarding, so
    /// a `/fusion` task's DTO/list entry can render identical progress.
    /// `None` for every other task type, and for a `local_fusion` task before
    /// its first `FusionProgress` event lands. Additive default `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
}

/// A `local_workflow` run projected for the interactive `/workflows` picker
/// ("Browse running and completed workflows"). Richer than [`TaskRecord`]: it
/// surfaces the `wf_…` run id, the launcher-minted current phase step, and the
/// start/end wall-clock (epoch millis) so the picker can show elapsed time.
/// These come from the concrete [`crate::host::task_registry::TaskRegistryHandle`]
/// impl's access to the full task state; the trait's default `list_workflows`
/// fills only what the reduced [`TaskRecord`] carries.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowRecord {
    /// 9-char task id (`w…`) — the handle key for `output`/`kill`.
    pub task_id: String,
    /// The effective `wf_…` run id, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Display name — the workflow's `meta.name` / `workflow_id`.
    pub name: String,
    /// Status wire string (`pending`/`running`/`paused`/`completed`/`failed`/`killed`).
    pub status: String,
    /// The workflow's launch description (script summary), when present.
    #[serde(default)]
    pub description: String,
    /// Index of the currently-executing phase step (0-based).
    #[serde(default)]
    pub current_step: usize,
    /// Wall-clock start (epoch millis), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    /// Wall-clock end (epoch millis) for a terminal run, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<u64>,
    /// Stored script source for a saveable dynamic workflow run. Claude keeps
    /// the resolved script on every live workflow task, regardless of whether
    /// it was launched inline, by name, or by path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// Script path needed by an explicitly resumed paused workflow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_path: Option<String>,
    /// Serialized workflow args needed by an explicitly resumed paused workflow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<String>,
    /// Distinct workflow agents known for this run. Terminal runs surface the
    /// runtime's aggregate count; live runs may leave this `0` until the TUI
    /// enriches from the spool/progress feed.
    #[serde(default)]
    pub agent_count: u64,
    /// Aggregate workflow tokens when the runtime has them. Live runs may leave
    /// this `0` until progress updates land.
    #[serde(default)]
    pub total_tokens: u64,
}

/// Everything a terminating `local_agent` run reports beyond its status — the
/// payload claude-code passes to `enqueueAgentNotification` (`yNt`) in the SAME
/// call that carries the terminal status.
///
/// The binary builds this at the moment of termination: `finalMessage` (the
/// agent's final text), the `{totalTokens,toolUses,durationMs}` usage object,
/// the `error` string on the failed path, and the worktree result spread
/// (`...await getWorktreeResult()`) whose two keys gate and fill the optional
/// `<worktree>` section. Modeling it as one struct keeps the port's write
/// ATOMIC with respect to the notification drain: a terminal status published
/// before its payload would let a drain fire the byte-shape of a result-less
/// completion.
///
/// Every field is optional and a `None` never clears an already-stored value
/// (see [`TaskRegistryHandle::set_agent_outcome`]), so a partial report — e.g.
/// a killed run with a worktree but no result — is safe.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentTerminalOutcome {
    /// The agent's final text response → the `<result>` section (claude
    /// `finalMessage`, `wc(content,"\n")` — text blocks joined with `\n`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// Run usage → the `<usage>` section.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<AgentRunUsage>,
    /// Failure reason → folded into the `failed` summary (claude `error ||
    /// 'Unknown error'`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The KEPT isolation worktree's path → gates and fills `<worktree>`. A
    /// worktree the terminal judgment auto-removed reports `None` (claude's
    /// `getWorktreeResult` resolves to `{}`), so the section is omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    /// The kept worktree's branch → `<worktreeBranch>` inside that section.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_branch: Option<String>,
    /// The agent's turn budget, set ONLY when the run ended by exhausting it
    /// (claude-code `enqueueAgentNotification`'s `maxTurnsReached`). Selects the
    /// turn-limit variant of the `completed` summary; `None` ⇒ the plain
    /// "finished" verb. Additive default `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns_reached: Option<u64>,
    /// Spawn depth of this agent, stamped at launch by the tool that has it.
    /// Carried so the kill path — which knows `killed_by` but not the launch
    /// metadata — can report `tengu_agent_tool_terminated`'s `agent_depth`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_depth: Option<u32>,
    /// Whether this agent type is built in, stamped at launch for the same
    /// reason as [`Self::agent_depth`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_built_in: Option<bool>,
}

/// Agent-run usage for a `local_agent` task-notification's optional `<usage>`
/// section — mirrors claude-code's `enqueueAgentNotification` usage object
/// (`{ totalTokens, toolUses, durationMs }`, rendered as
/// `<subagent_tokens>/<tool_uses>/<duration_ms>`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentRunUsage {
    /// `totalTokens` → `<subagent_tokens>`.
    pub subagent_tokens: u64,
    /// `totalToolUseCount` → `<tool_uses>`.
    pub tool_uses: u64,
    /// `totalDurationMs` → `<duration_ms>`.
    pub duration_ms: u64,
}

/// Everything a terminating `local_workflow` run reports beyond its status.
/// Claude keeps the script result, non-fatal item failures, and aggregate usage
/// as separate notification fields; keeping them together here makes the
/// status-sink update atomic with the terminal transition.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowTerminalOutcome {
    /// The script's serialized return value, if it returned one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// Non-fatal `parallel()` / `pipeline()` diagnostics.
    #[serde(default)]
    pub failures: Vec<String>,
    /// Fatal script/engine error, if the workflow failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Number of real `agent()` calls, including cached calls.
    #[serde(default)]
    pub agent_count: u64,
    /// Aggregate terminal subagent tokens.
    #[serde(default)]
    pub total_tokens: u64,
    /// Aggregate terminal subagent tool calls.
    #[serde(default)]
    pub total_tool_calls: u64,
    /// End-to-end workflow duration.
    #[serde(default)]
    pub duration_ms: u64,
    /// Terminal workflow-agent counts used by Claude's enriched `<usage>`.
    #[serde(default)]
    pub agents_done: u64,
    /// Terminal workflow agents that failed.
    #[serde(default)]
    pub agents_error: u64,
    /// Terminal workflow agents that were skipped.
    #[serde(default)]
    pub agents_skipped: u64,
    /// Successful workflow agents whose result was structurally empty.
    #[serde(default)]
    pub agents_empty_result: u64,
    /// Whether the terminal counts above came from real workflow progress
    /// instrumentation instead of a default zero-fill.
    #[serde(default)]
    pub progress_counts_available: bool,
}

/// A terminal task that has not yet been surfaced to the model, snapshotted at
/// drain time for the `<task-notification>` renderer (claude-code's per-task-type
/// `enqueue*Notification`, e.g. `enqueueShellNotification` /
/// `enqueueAgentNotification`). Each field maps to a tag the renderer emits;
/// fields that a given task type does not carry stay `None` and the renderer
/// omits the corresponding clause/tag.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskNotification {
    /// Recipient agent; None denotes the main session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient_agent_id: Option<crate::types::AgentId>,
    /// 9-char task id → `<task-id>`.
    pub task_id: String,
    /// Task type wire string (one of the ten — see [`TaskRecord::task_type`]).
    /// Selects the
    /// per-type notification format (bash / agent / monitor / generic).
    pub task_type: String,
    /// Terminal status wire string — one of `completed` / `failed` / `killed`
    /// → `<status>` and the human-readable summary verb.
    pub status: String,
    /// Human-readable description → interpolated into `<summary>`.
    pub description: String,
    /// Originating `tool_use_id`, if the task was launched from a tool call →
    /// the optional `<tool-use-id>` line. `None` ⇒ the line is omitted.
    pub tool_use_id: Option<String>,
    /// Absolute on-disk spool path → `<output-file>`. `None` ⇒ the renderer
    /// falls back to the bare `<task_id>.output` filename.
    pub output_path: Option<String>,
    /// Process exit code for `local_bash` / `monitor_ws` tasks, folded into the
    /// summary (e.g. `(exit code 1)`). `None` ⇒ the exit clause is omitted.
    pub exit_code: Option<i32>,
    /// `monitor_ws` only: bytes the script wrote to STDOUT over its life
    /// (claude-code `pipedStdoutBytes`). `Some(0)` selects the
    /// "ended without producing output" completion summary; `None` means the
    /// count was never taken — an `mcp_task`/`monitor_mcp` row has no stdout at
    /// all — and must NOT be read as zero. Additive default `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_bytes: Option<u64>,
    /// Failure reason for a `local_agent` task, folded into the `failed`
    /// summary (`Agent "…" came to rest with an error: {error}`). `None` falls
    /// back to `Unknown error` (claude-code `error || 'Unknown error'`).
    pub error: Option<String>,
    /// `local_agent`: final text response; `monitor_ws` while running: batched
    /// stdout event. Both render in an escaped `<result>` section.
    pub result: Option<String>,
    /// `local_agent` only: run usage → the optional `<usage>` section.
    /// `None` ⇒ omitted (claude-code's `i ? <usage>… : ''`).
    pub usage: Option<AgentRunUsage>,
    /// `local_agent` only: the stop reason for a `killed` task — selects the
    /// killed-summary verb (claude-code `enqueueAgentNotification`'s `killedBy`
    /// param). `Some("parent")` → `was stopped by Claude` (a parent-agent /
    /// `TaskStop`-initiated stop), `Some("user")` → `was stopped by user`,
    /// `None`/other → the generic `was stopped` (the binary's
    /// `n==="parent"?…:n==="user"?…:"was stopped"` fallback for an undefined
    /// `killedBy`). Additive default `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub killed_by: Option<String>,
    /// `local_agent` only: the isolation worktree's absolute path → gates AND
    /// fills the optional `<worktree><worktreePath>…</worktreePath>…</worktree>`
    /// section (claude-code `enqueueAgentNotification`'s `worktreePath`; tags
    /// `pZo="worktree"` / `fZo="worktreePath"`). `None` ⇒ the whole worktree
    /// section is omitted (the binary's `c ? … : ''`). Additive default `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    /// `local_agent` only: the isolation worktree's branch name → the optional
    /// `<worktreeBranch>…</worktreeBranch>` tag rendered INSIDE the worktree
    /// section (claude-code `worktreeBranch`; tag `mZo="worktreeBranch"`,
    /// independently gated on `u`). `None` while `worktree_path` is `Some` ⇒ the
    /// branch tag is omitted but the section still renders. Additive default
    /// `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_branch: Option<String>,
    /// `monitor_ws` while running: this event is a harness housekeeping line
    /// (suppression notice, timeout marker) rather than script output —
    /// claude-code `GM`'s `isHousekeeping`. Suppresses the per-event
    /// push-notification hint. Additive default `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub monitor_housekeeping: bool,
    /// `mcp_task` only: the fields claude-code's `F` needs on top of the
    /// registry id. `None` for every other task type, which is what keeps the
    /// generic arm the fallback. Additive default `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<McpTaskNotificationMeta>,
    /// `local_agent` only: the turn budget the run exhausted (claude-code
    /// `enqueueAgentNotification`'s `maxTurnsReached`). `Some(n)` on a
    /// `completed` task replaces the "finished" verb with
    /// `stopped at its {n}-turn limit (partial result; SendMessage to task-id
    /// to continue)`. Additive default `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns_reached: Option<u64>,
    /// `local_workflow`: non-fatal per-item diagnostics rendered in a separate
    /// `<failures>` section, never appended to `<result>`.
    #[serde(default)]
    pub workflow_failures: Vec<String>,
    /// `local_workflow`: aggregate workflow usage fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_agent_count: Option<u64>,
    /// `local_workflow`: aggregate terminal subagent token count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_total_tokens: Option<u64>,
    /// `local_workflow`: aggregate terminal subagent tool-call count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_total_tool_calls: Option<u64>,
    /// `local_workflow`: end-to-end workflow duration in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_duration_ms: Option<u64>,
    /// `local_workflow`: session-owned persisted script path and run identity,
    /// used to render recovery/diagnostics instructions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_script_path: Option<String>,
    /// `local_workflow`: stable run identifier used by resume instructions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_run_id: Option<String>,
    /// `local_workflow`: serialized workflow arguments used by rerun instructions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_args: Option<String>,
    /// `local_workflow`: directory containing per-agent transcript journals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_transcript_dir: Option<String>,
    /// Terminal workflow-agent counters included inside `<usage>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_agents_done: Option<u64>,
    /// `local_workflow`: number of failed terminal agent calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_agents_error: Option<u64>,
    /// `local_workflow`: number of skipped terminal agent calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_agents_skipped: Option<u64>,
    /// `local_workflow`: number of terminal agent calls with empty results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_agents_empty_result: Option<u64>,
    /// `local_fusion`: provider profiles that received prompt data on this
    /// run (parent's profile plus any cross-provider panels) → the optional
    /// `<egress-profiles>` line. Never includes model names, only profile
    /// ids. Empty ⇒ the section is omitted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub egress_profiles: Vec<String>,
}

/// One chunk of a task's accumulated stdout/stderr spool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskOutputChunk {
    /// Harness turn-limit note, rendered before the task output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_head: Option<String>,
    /// 9-char task id.
    pub task_id: String,
    /// Spooled content for this chunk.
    pub content: String,
    /// Total line count of the spool.
    pub total_lines: u64,
    /// `true` when the surfaced content was truncated by a limit.
    pub truncated: bool,
    /// Task status wire string at the chunk point (one of the 5 byte-locked
    /// status strings), if the registry could resolve it. Mirrors the TS
    /// `task.status` carried by `TaskOutputTool`'s `TaskOutput`.
    pub status: Option<String>,
    /// Process exit code at the chunk point, if terminal and applicable.
    /// Mirrors the TS `exitCode` (`bashTask.result?.code ?? null`).
    pub exit_code: Option<i32>,
    /// `true` when the task has reached a terminal status (completed / failed /
    /// killed). Lets the tool compute `block`/`retrieval_status` without a
    /// second registry round-trip.
    pub done: bool,
    /// Agent-task error message, if any. Mirrors the TS `TaskOutput.error`
    /// (`agentTask.error`); only populated for `local_agent` tasks. Surfaced by
    /// `TaskOutputTool` as a trailing `<error>…</error>` element.
    pub error: Option<String>,
    /// Agent-task initial prompt. Mirrors the TS `TaskOutput.prompt`
    /// (`agentTask.prompt`); only populated for `local_agent` tasks.
    pub prompt: Option<String>,
    /// Clean final answer extracted from the agent's last assistant message
    /// (the `text` content blocks joined by `\n`). Mirrors the TS
    /// `cleanResult = extractTextContent(agentTask.result.content, '\n')`; only
    /// populated for `local_agent` tasks. `TaskOutputTool` prefers this over the
    /// raw on-disk transcript for the model-facing `<output>`.
    pub result: Option<String>,
    /// Absolute on-disk path of the task's spool file, when the registry can
    /// resolve it. Mirrors the path `getTaskOutputPath(taskId)` returns in
    /// claude-code (`<projectTempDir>/<sessionId>/tasks/<taskId>.output`).
    /// `TaskOutputTool` uses it for the `[Truncated. Full output: <path>]`
    /// header (`outputFormatting.ts:31-34`), so the model sees the real absolute
    /// path it can read. `None` ⟶ the tool falls back to the bare
    /// `<taskId>.output` filename.
    pub output_path: Option<String>,
    /// `mcp_task` only: the fields claude-code's `getTaskOutputData` folds into
    /// a SYNTHETIC metadata block instead of returning the spool
    /// (`src_160988549.js` @3690698). `None` for every other type. Additive
    /// default `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<McpTaskOutputMeta>,
}

/// The `mcp_task` half of [`TaskNotification`] — see its `mcp` field.
///
/// Claude-code passes these to `F` (`src_184372091.js` @12876) at the enqueue
/// site (@16139) alongside the registry id, which is the only thing the other
/// task types need.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpTaskNotificationMeta {
    /// Receipt explaining where the complete result was saved, or why it was not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_hint: Option<String>,

    /// `serverName` — the left half of the `(server/tool)` clause.
    pub server_name: String,
    /// `toolName` — the right half.
    pub tool_name: String,
    /// `mcpStatus`, NOT the registry status. `F` interpolates this into both
    /// the summary's trailing word and the `<status>` tag, so an `mcp_task` is
    /// the one notification whose `<status>` is not the registry's own wire
    /// string: a server-cancelled call is `cancelled` here while the registry
    /// row says `failed`.
    pub mcp_status: String,
    /// `statusMessage` — the detail line a failed or cancelled call carries.
    /// `None` selects the "cancelled by the server" wording.
    pub status_message: Option<String>,
}

/// The `mcp_task` half of [`TaskOutputChunk`] — see its `mcp` field.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpTaskOutputMeta {
    /// `serverName` → the `server: …` line.
    pub server_name: String,
    /// `toolName` → the `tool: …` line.
    pub tool_name: String,
    /// `mcpStatus` → the `status: …` line (or `server status when stopped: …`
    /// once the task was killed). Underscores render as spaces.
    pub mcp_status: String,
    /// `statusMessage` → the optional `status message: …` line.
    pub status_message: Option<String>,
    /// `(endTime ?? now) - startTime` → the `elapsed: …` line. Computed at the
    /// registry, which owns both timestamps.
    pub elapsed_ms: u64,
}

/// The model-facing refusal for a message aimed at a USER-stopped agent
/// (AGT-07), byte-locked to claude-code's `uM` message in the resume queue
/// (`src_180597926.js` @407402):
/// `` `Agent ${Ke} was stopped by the user and won't be resumed.` `` — where
/// `Ke` is the TASK id (the registry key `z2t`/`K2t` are handed), not the
/// agent id.
///
/// Lives here because two crates need the SAME wording — the registry's own
/// `send_message` gate and `SendMessage`'s synchronous pre-flight — and the
/// model must not get two different accounts of one fact.
///
/// The oracle's sibling arm — `` `Agent ${Ke} is ${jt} and cannot take queued
/// messages.` `` for a non-running/completed status — is deliberately NOT
/// ported: that case already reaches the pump as
/// [`TeamSpawnError::Terminated`](crate::host::team_spawn::TeamSpawnError::Terminated)
/// through the spawned-id lookup, and it is not what AGT-07 names.
#[must_use]
pub fn stopped_by_user_message(task_id: &str) -> String {
    format!("Agent {task_id} was stopped by the user and won't be resumed.")
}

/// Failure modes for [`TaskRegistryHandle`] operations.
#[derive(Debug, Error)]
pub enum TaskRegistryError {
    /// No task with that id exists.
    #[error("Task: not found: {0}")]
    NotFound(String),
    /// The input is malformed (unknown `task_type`, malformed id, bad status).
    #[error("Task: invalid input: {0}")]
    InvalidInput(String),
    /// Any other internal failure.
    #[error("Task: internal error: {0}")]
    Internal(String),
}

/// CRUD surface used by the 6 `Task*` tools.
#[async_trait]
pub trait TaskRegistryHandle: Send + Sync {
    /// Trusted host input only. Model tools must keep using their guarded message path.
    async fn send_human_task_message(
        &self,
        _task_id: &str,
        _message: &str,
    ) -> Result<(), TaskRegistryError> {
        Err(TaskRegistryError::Internal(
            "human task messages unavailable".into(),
        ))
    }
    /// Runtime boundary drain; the registry checks current stop epochs.
    async fn take_human_task_messages_for(&self, _agent_id: crate::types::AgentId) -> Vec<String> {
        Vec::new()
    }
    /// Last startup check for a trusted stopped-agent restoration.
    async fn begin_human_task_resume(
        &self,
        _task_id: &str,
        _epoch: u64,
    ) -> Result<(), TaskRegistryError> {
        Err(TaskRegistryError::Internal(
            "human task resume unavailable".into(),
        ))
    }
    /// Preserve a foreground launch's exact configuration for explicit human resume.
    async fn register_agent_resume_recipe(
        &self,
        _task_id: &str,
        _request: crate::host::SubagentSpawnRequest,
        _inheritance: crate::host::SubagentInheritance,
    ) -> Result<(), TaskRegistryError> {
        Ok(())
    }

    async fn export_shell_handoff(
        &self,
    ) -> Result<Vec<crate::host::shell_handoff::ShellTaskHandoff>, TaskRegistryError> {
        Ok(Vec::new())
    }
    async fn prepare_shell_handoff(
        &self,
        records: &[crate::host::shell_handoff::ShellTaskHandoff],
    ) -> Result<(), TaskRegistryError> {
        if records.is_empty() {
            Ok(())
        } else {
            Err(TaskRegistryError::Internal(
                "shell adoption unsupported".into(),
            ))
        }
    }
    async fn commit_shell_handoff(
        &self,
        _ids: &[String],
    ) -> Result<Vec<String>, TaskRegistryError> {
        Ok(Vec::new())
    }
    async fn adopt_shell_handoff(
        &self,
        records: &[crate::host::shell_handoff::ShellTaskHandoff],
    ) -> Result<(), TaskRegistryError> {
        if records.is_empty() {
            Ok(())
        } else {
            Err(TaskRegistryError::Internal(
                "shell adoption unsupported".into(),
            ))
        }
    }
    async fn rollback_shell_handoff(
        &self,
        records: &[crate::host::shell_handoff::ShellTaskHandoff],
    ) -> Result<(), TaskRegistryError> {
        self.adopt_shell_handoff(records).await
    }

    /// Add actual permission prompt wait time to an attributed task.
    fn add_permission_paused_ms(&self, _agent_id: crate::types::AgentId, _milliseconds: u64) {}

    /// Bind an allocated output to the trusted spawner's transcript path.
    async fn link_agent_output(
        &self,
        _id: &str,
        _target: &std::path::Path,
    ) -> Result<(), TaskRegistryError> {
        Err(TaskRegistryError::Internal(
            "transcript output links are not wired".into(),
        ))
    }

    /// Feed an observed agent's activity to its independent persistent sidecar.
    /// Running observers queue digests and resume only after their current turn.
    async fn observe_agent_activity(
        &self,
        _request: crate::host::SubagentSpawnRequest,
        _inheritance: crate::host::SubagentInheritance,
        _observed_agent_id: crate::types::AgentId,
        _digest: String,
        _seed: Option<crate::host::observer_pairing::ObserverPairingSeed>,
    ) -> Result<(), TaskRegistryError> {
        Err(TaskRegistryError::InvalidInput(
            "observer tasks are unavailable".into(),
        ))
    }
    /// Session output root used by file search and output display.
    async fn task_output_directory(&self) -> Option<String> {
        None
    }

    /// Create a new task, returning the freshly generated record.
    async fn create(&self, input: TaskCreateInput) -> Result<TaskRecord, TaskRegistryError>;

    /// Look up a task by id.
    async fn get(&self, id: &str) -> Result<Option<TaskRecord>, TaskRegistryError>;

    /// Agent identities whose live OS process groups belong to this task loop.
    async fn process_owners_for_task(&self, id: &str) -> Vec<String> {
        match self.get(id).await {
            Ok(Some(record)) if record.task_type == "local_agent" => {
                record.owner_agent_id.into_iter().collect()
            }
            _ => Vec::new(),
        }
    }

    /// A terminal agent/workflow may still have an execution loop to stop.
    async fn has_live_task_loop(&self, _id: &str) -> bool {
        false
    }

    /// Resolve task ids and agent names for TaskStop.
    async fn resolve_stop_target(
        &self,
        requested: &str,
        _named_agents: &[(String, String)],
    ) -> Result<TaskStopResolution, TaskRegistryError> {
        Ok(match self.get(requested).await? {
            Some(record) => TaskStopResolution::Found(record),
            None => TaskStopResolution::NotFound { suggestion: None },
        })
    }

    /// List tasks, optionally filtered.
    async fn list(&self, filter: TaskListFilter) -> Result<Vec<TaskRecord>, TaskRegistryError>;

    /// List `local_workflow` runs (running, adopted-paused, and completed) for the
    /// `/workflows` picker. The default impl derives them from [`Self::list`],
    /// filling only the reduced [`TaskRecord`] fields; the concrete
    /// `TaskRegistry` impl overrides this to add the `wf_…` run id, phase step,
    /// and start/end timestamps from the full task state. Defaulted so existing
    /// mock handles compile unchanged (frozen-trait idiom).
    async fn list_workflows(&self) -> Result<Vec<WorkflowRecord>, TaskRegistryError> {
        let records = self.list(TaskListFilter::default()).await?;
        Ok(records
            .into_iter()
            .filter(|r| r.task_type == "local_workflow")
            .map(|r| WorkflowRecord {
                task_id: r.task_id,
                run_id: None,
                name: r.name.unwrap_or_else(|| r.description.clone()),
                status: r.status,
                description: r.description,
                current_step: 0,
                started_at_ms: None,
                ended_at_ms: None,
                script: None,
                script_path: None,
                args: None,
                agent_count: 0,
                total_tokens: 0,
            })
            .collect())
    }

    /// Apply a patch (currently: status transition).
    async fn update(
        &self,
        id: &str,
        patch: TaskUpdatePatch,
    ) -> Result<TaskRecord, TaskRegistryError>;

    /// Force a specific status string (covers `TaskUpdate` for variants whose
    /// status field is exposed in the M1 registry surface). Wired directly to
    /// the concrete `TaskRegistry::set_status` method.
    async fn set_status(&self, id: &str, status: &str) -> Result<TaskRecord, TaskRegistryError>;

    /// Kill the task (cancels any background handle, marks status `killed`).
    async fn kill(&self, id: &str) -> Result<TaskRecord, TaskRegistryError>;

    /// True only while an approved teammate departure still needs cleanup.
    async fn has_pending_teammate_departure(&self, _id: &str) -> bool {
        false
    }

    /// Kill the task, recording WHO stopped it so a `local_agent`'s killed
    /// notification renders the right verb.
    ///
    /// Claude-code's stop entry point (`H$e`) takes `killedBy` from its caller
    /// and stores it on the task; the renderer then picks
    /// `"parent"` → `was stopped by Claude`, `"user"` → `was stopped by user`,
    /// anything else → the bare `was stopped`. The two production callers are
    /// byte-visible in 2.1.220: the `TaskStop` TOOL passes `killedBy:"parent"`
    /// (a parent agent stopping a child), and the UI/control-channel `stopTask`
    /// passes `source:"user"` and inherits `H$e`'s `killedBy = "user"`
    /// destructuring default.
    ///
    /// Defaulted to plain [`kill`](Self::kill) — a host that does not
    /// distinguish the two stores no reason, which is exactly the binary's
    /// `undefined killedBy` case.
    async fn kill_with_reason(
        &self,
        id: &str,
        _killed_by: &str,
    ) -> Result<TaskRecord, TaskRegistryError> {
        self.kill(id).await
    }

    /// Record a terminating `local_agent`'s notification payload
    /// ([`AgentTerminalOutcome`]).
    ///
    /// MUST be called BEFORE the terminal `set_status`: the drain
    /// ([`take_pending_task_notifications`](Self::take_pending_task_notifications))
    /// is gated on terminal-and-not-notified, so publishing the status first
    /// opens a window where the notification renders without its `<result>` /
    /// `<usage>` / `<worktree>` sections. Claude-code has no such window — it
    /// passes status and payload to `enqueueAgentNotification` in one call.
    ///
    /// Merge semantics: a `Some` field overwrites, a `None` field leaves the
    /// stored value alone. Default no-op so existing mock handles compile
    /// unchanged.
    async fn set_agent_outcome(&self, _id: &str, _outcome: AgentTerminalOutcome) {}

    /// Record a terminating `local_workflow`'s result, failures, and usage
    /// before its terminal status is published. Defaulted for existing hosts
    /// and mocks that do not expose workflow notifications.
    async fn set_workflow_outcome(&self, _id: &str, _outcome: WorkflowTerminalOutcome) {}

    /// Spawn a real background monitor and return its registry task id.
    /// Start a passive WebSocket monitor using the host's vetted transport.
    async fn spawn_websocket_monitor(
        &self,
        _registration: WebSocketMonitorRegistration,
        _http: std::sync::Arc<dyn crate::host::http::HttpTransport>,
    ) -> Result<String, TaskRegistryError> {
        Err(TaskRegistryError::InvalidInput(
            "WebSocket monitoring is unavailable on this host".into(),
        ))
    }

    /// Hosts without a task runtime fail closed rather than minting a fake id.
    async fn spawn_monitor(&self, reg: MonitorRegistration) -> Result<String, TaskRegistryError> {
        let _ = reg;
        Err(TaskRegistryError::Internal(
            "monitor registration unwired".into(),
        ))
    }

    /// Enqueue one live stdout event for the next task-notification drain.
    ///
    /// `housekeeping` marks a line the harness produced ABOUT the monitor (the
    /// suppression notice, the timeout marker) rather than one the watched
    /// script wrote — claude-code's `GM(…, {isHousekeeping:!0})`. It suppresses
    /// the per-event push-notification hint, which only makes sense for real
    /// output.
    async fn notify_monitor_event(&self, _id: &str, _event: &str, _housekeeping: bool) {}

    /// Stop the background agent work that Claude Code tears down when the
    /// session reaches `--max-budget-usd`.
    ///
    /// Claude Code 2.1.217's `rcr` budget cleanup selects running
    /// `local_agent` tasks unless `isBackgrounded === false`, plus running
    /// `local_workflow` tasks. It deliberately leaves shell, MCP, remote-agent,
    /// and foreground-agent tasks alone. Keep the selection here at the shared
    /// registry boundary so the print and stream-json turn drivers cannot drift
    /// from one another.
    ///
    /// `local_fusion` has no claude-code analogue but is added here
    /// unconditionally (LingXi-specific, finding G003(b)): before this, a
    /// running Fusion run was invisible to `_ => false` and kept spending
    /// past `--max-budget-usd` — the one runtime kill switch for background
    /// spend never reached it.
    ///
    /// `before_stop` is called exactly once, after at least one matching task is
    /// found and before any cancellation begins. The print driver uses that
    /// seam to preserve Claude's observable ordering: write the budget notice,
    /// then tear down background work.
    ///
    /// Returns the number of matching tasks for which a stop was attempted.
    /// Individual stop races are best-effort: one task finishing between the
    /// list and kill calls must not prevent the remaining agents from stopping.
    async fn stop_background_agents_for_budget(
        &self,
        before_stop: &(dyn Fn() + Send + Sync),
    ) -> Result<usize, TaskRegistryError> {
        let running = self
            .list(TaskListFilter {
                status: Some("running".to_string()),
            })
            .await?;
        let ids: Vec<String> = running
            .into_iter()
            .filter(|task| match task.task_type.as_str() {
                "local_agent" => task.is_backgrounded != Some(false),
                "local_workflow" | "local_fusion" => true,
                _ => false,
            })
            .map(|task| task.task_id)
            .collect();
        if !ids.is_empty() {
            before_stop();
        }
        for id in &ids {
            let _ = self.kill(id).await;
        }
        Ok(ids.len())
    }

    /// Register a backgrounded MCP tool call and return the minted `k…` task id
    /// (claude-code 2.1.212 `callMcpToolWithAutoBackground`'s `i.register(g)`,
    /// where `g = NZu({serverName, toolName, toolUseId, abortController})`). The
    /// task is inserted `running` with `mcpStatus:"working"`; `cancel` is fired
    /// when the task is later killed (`TaskStop`), aborting the still-running
    /// in-flight call — the port equivalent of the state's `abortController` +
    /// the poll loop's `cancelTask` on `status==="killed"`.
    ///
    /// Default impl returns [`TaskRegistryError::Internal`] so a host that has
    /// not wired a real registry never auto-backgrounds — the [`crate`] MCP tool
    /// falls back to the direct await. Frozen-trait defaulted-method idiom so
    /// existing mock handles compile unchanged.
    async fn register_mcp_task(
        &self,
        reg: McpTaskRegistration,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<String, TaskRegistryError> {
        let _ = (reg, cancel);
        Err(TaskRegistryError::Internal(
            "mcp_task registration unwired".into(),
        ))
    }

    /// Settle a backgrounded MCP tool call once the detached `tools/call`
    /// resolves (claude-code `callMcpToolWithAutoBackground`'s inner `E`
    /// callback + `p.then(…)`): write `result_text` into the task spool so the
    /// drained `<task-notification>`'s `output-file` carries the real result,
    /// then mark the task terminal — `completed` on success, `failed` otherwise
    /// (`{...O, status:S, mcpStatus:S, endTime, notified:true}`). A task already
    /// terminal (e.g. killed via `TaskStop`) is left untouched (the binary's
    /// `if(O.notified) return O` guard). Default no-op so existing mock handles
    /// compile unchanged.
    ///
    /// Returns `Ok(true)` when this call won the terminal transition and
    /// `Ok(false)` when the task was already terminal (the no-op path) — callers
    /// gate the `mcp_auto_background` outcome counter on this so a killed /
    /// already-settled task never re-emits it. Default returns `Ok(false)`
    /// (the no-op mock never settles anything).
    async fn settle_mcp_task(
        &self,
        id: &str,
        result_text: &str,
        failed: bool,
    ) -> Result<bool, TaskRegistryError> {
        let _ = (id, result_text, failed);
        Ok(false)
    }

    /// Settle with the receipt from persisting a truncated MCP result. The hint
    /// and terminal status must become visible atomically.
    async fn settle_mcp_task_with_hint(
        &self,
        id: &str,
        text: &str,
        failed: bool,
        _saved_hint: Option<&str>,
    ) -> Result<bool, TaskRegistryError> {
        self.settle_mcp_task(id, text, failed).await
    }

    /// Read the task's spool starting at `offset` (or from 0 if `None`).
    /// Mint the task identity for a shell command: a task id and an
    /// already-created output file for the process runner to append to.
    ///
    /// claude-code mints exactly one such identity per shell command, in its
    /// single spawn (`vV`), so the id the model may later be handed, the id the
    /// registry records and the file the child writes to are the same identity.
    /// Defaults to an explicit error so a host without a registry keeps the
    /// previous runner-owned behaviour instead of silently losing the task.
    ///
    /// # Errors
    /// Returns [`TaskRegistryError`] when no registry is wired, or when the
    /// output file cannot be allocated.
    async fn allocate_bash_output(&self) -> Result<BackgroundBashHandle, TaskRegistryError> {
        Err(TaskRegistryError::Internal(
            "background bash allocation unwired".into(),
        ))
    }

    /// Register a `local_bash` record for an identity already minted by
    /// [`Self::allocate_bash_output`], because the command is being
    /// backgrounded (explicitly, or after exceeding its timeout).
    ///
    /// Registration is deliberately separate from allocation: claude-code mints
    /// the identity for every shell command but only creates a task record when
    /// the command is actually backgrounded (`Xne`) or has been running long
    /// enough to be armed for it (`U6t`). Registering every foreground command
    /// would put a completed row in `TaskList` and a `<task-notification>` in
    /// the transcript for every shell call.
    ///
    /// # Errors
    /// Returns [`TaskRegistryError`] when no registry is wired.
    async fn register_background_bash(
        &self,
        task_id: &str,
        registration: BackgroundBashRegistration,
    ) -> Result<(), TaskRegistryError> {
        let _ = (task_id, registration);
        Err(TaskRegistryError::Internal(
            "background bash registration unwired".into(),
        ))
    }

    /// Delete an allocated output file that was never needed, because the
    /// command completed in the foreground and its output was returned inline
    /// (claude-code `deleteOutputFile` under `outputFileRedundant`).
    async fn discard_bash_output(&self, task_id: &str) {
        let _ = task_id;
    }

    /// Register a still-running FOREGROUND shell so it is addressable
    /// (claude-code `U6t`, called from the Bash poll loop once the command has
    /// been running for `cnr` = 2000 ms).
    ///
    /// The row is `status:"running"` with `isBackgrounded:false`. It exists so
    /// `/tasks`, Ctrl+B and background-all can see a long-running foreground
    /// command; it is NOT a background task and must be withdrawn by
    /// [`Self::unregister_foreground_bash`] when the command finishes in the
    /// foreground, or the model would be told a command "completed" that it was
    /// never told had started.
    ///
    /// `auto_background_armed` records whether the deadline would background
    /// this command rather than kill it, which the row carries for the UI.
    ///
    /// # Errors
    /// Returns [`TaskRegistryError`] when no registry is wired.
    async fn register_foreground_bash(
        &self,
        task_id: &str,
        registration: BackgroundBashRegistration,
        auto_background_armed: bool,
    ) -> Result<(), TaskRegistryError> {
        let _ = (task_id, registration, auto_background_armed);
        Err(TaskRegistryError::Internal(
            "foreground bash registration unwired".into(),
        ))
    }

    /// Bind messaging for a foreground runner the registry did not spawn.
    async fn bind_agent_message_receiver(
        &self,
        _id: &str,
        _receiver: std::sync::Arc<dyn TaskMessageReceiver>,
    ) -> Result<(), TaskRegistryError> {
        Err(TaskRegistryError::Internal(
            "external agent messaging unwired".into(),
        ))
    }

    async fn set_agent_display(&self, _id: &str, _model: String, _effort: Option<String>) {}

    /// Register an allocated foreground agent before it begins model work.
    async fn register_foreground_agent(
        &self,
        _registration: ForegroundAgentRegistration,
    ) -> Result<ForegroundAgentHandle, TaskRegistryError> {
        Err(TaskRegistryError::Internal(
            "foreground agent registration unwired".into(),
        ))
    }

    /// Remove a finished foreground agent; preserve rows already backgrounded.
    async fn unregister_foreground_agent(&self, _task_id: &str) {}

    /// Withdraw an armed foreground row because the command finished in the
    /// foreground (claude-code `W6t`: `if(!bp(o)||o.isBackgrounded||o.notified)
    /// return; r.remove(e)`).
    ///
    /// A row that was backgrounded in the meantime is left alone — it is a real
    /// background task now and owns its own completion notification.
    async fn unregister_foreground_bash(&self, task_id: &str) {
        let _ = task_id;
    }

    /// Attach the handle that moves an armed foreground shell to the background
    /// on demand.
    ///
    /// # Errors
    /// Returns [`TaskRegistryError`] when the task is unknown or no registry is
    /// wired.
    async fn bind_background_requester(
        &self,
        task_id: &str,
        requester: std::sync::Arc<dyn TaskBackgrounder>,
    ) -> Result<(), TaskRegistryError> {
        let _ = (task_id, requester);
        Err(TaskRegistryError::Internal(
            "background requester binding unwired".into(),
        ))
    }

    /// Move one task to the background (claude-code `Wer` for `local_bash`,
    /// `s9` for everything else). Returns whether anything moved.
    async fn background_task(&self, task_id: &str) -> bool {
        let _ = task_id;
        false
    }

    /// Collect the rosters a "no task found" message names (claude-code `bjn`
    /// and `JFe`).
    ///
    /// `caller_agent_id` is excluded from the background-agent roster (`JFe`'s
    /// `p.id!==r`), and `named_agent_ids` — the values of the agent-name
    /// registry — are excluded too, because those are reported separately as
    /// "Running named agents".
    async fn not_found_rosters(
        &self,
        caller_agent_id: Option<&str>,
        named_agent_ids: &[String],
    ) -> TaskNotFoundRosters {
        let _ = (caller_agent_id, named_agent_ids);
        TaskNotFoundRosters::default()
    }

    /// Move the task owning `tool_use_id` to the background (claude-code
    /// `Ode`). Returns whether anything moved.
    async fn background_task_for_tool_use(&self, tool_use_id: &str) -> bool {
        let _ = tool_use_id;
        false
    }

    /// Move every backgroundable task to the background (claude-code `zM`) and
    /// report how many moved.
    async fn background_all_tasks(&self) -> usize {
        0
    }

    /// Detach work with the trigger that caused the transition.
    async fn background_all_tasks_with_reason(&self, _reason: TaskBackgroundReason) -> usize {
        self.background_all_tasks().await
    }

    /// Whether anything is currently backgroundable (claude-code `H_t`), i.e.
    /// whether a Ctrl+B hint should be offered at all.
    async fn has_backgroundable_tasks(&self) -> bool {
        false
    }

    /// Attach the killer that terminates a background shell's OS process, so
    /// `TaskStop` and session teardown can reach a child the registry did not
    /// spawn itself.
    ///
    /// # Errors
    /// Returns [`TaskRegistryError`] when the id is unknown.
    async fn bind_background_killer(
        &self,
        id: &str,
        killer: std::sync::Arc<dyn TaskKiller>,
    ) -> Result<(), TaskRegistryError> {
        let _ = (id, killer);
        Ok(())
    }

    /// Bind the actual OS identity together with the stop capability.
    async fn bind_background_process(
        &self,
        id: &str,
        _pid: u32,
        killer: std::sync::Arc<dyn TaskKiller>,
    ) -> Result<(), TaskRegistryError> {
        self.bind_background_killer(id, killer).await
    }

    /// Kill the background shells a finishing agent started, returning how many
    /// were still running.
    ///
    /// The Bash tool tells a synchronous subagent that a backgrounded command
    /// "is terminated when you give your final response"; this is what makes
    /// that true (claude-code sweeps them in its agent-run cleanup). Defaults to
    /// a no-op so a host without a registry behaves as before.
    async fn kill_background_shells_for_agent(&self, agent_id: crate::types::AgentId) -> usize {
        let _ = agent_id;
        0
    }

    /// Settle a background shell task once its child has been reaped.
    ///
    /// `killed` wins over the exit code; otherwise exit code `0` completes the
    /// task and anything else (including an unknown code) fails it — the port
    /// of claude-code `Fpt` (`interrupted` → killed, `code === 0` → completed,
    /// else failed).
    ///
    /// # Errors
    /// Returns [`TaskRegistryError`] when the id is unknown.
    /// Append captured shell output through the registry's bounded writer.
    async fn append_bash_output(
        &self,
        _task_id: &str,
        _content: &str,
    ) -> Result<(), TaskRegistryError> {
        Err(TaskRegistryError::Internal(
            "background output writer unwired".into(),
        ))
    }

    /// Wait for an existing shell-output drain; do not restart a failed drain.
    /// Finalize a completed output copy, returning its pre-truncation size.
    async fn finalize_persisted_output(
        &self,
        _id: &str,
        _max_bytes: u64,
    ) -> Result<Option<u64>, TaskRegistryError> {
        Ok(None)
    }

    async fn flush_bash_output(&self, _task_id: &str) -> Result<(), TaskRegistryError> {
        Ok(())
    }

    /// Session-scoped SDK lifecycle events, emitted at each task mutation.
    fn subscribe_task_lifecycle(
        &self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>> {
        None
    }

    /// Main-host state used to protect active sessions from pressure reaping.
    fn update_shell_session_activity(
        &self,
        _interactive: bool,
        _busy: bool,
        _user_interaction: bool,
    ) {
    }

    /// Emit an advisory prompt-stall notice without consuming completion.
    async fn notify_bash_stall(&self, _task_id: &str, _tail: &str) {}

    /// Atomically claim an eligible shell stop; unwired hosts decline.
    async fn claim_bash_memory_pressure_stop(&self, _task_id: &str) -> bool {
        false
    }

    /// The native supervisor is the sole terminal-output writer for this ID.
    async fn mark_shell_supervised(&self, _id: &str) {}

    async fn settle_background_bash(
        &self,
        id: &str,
        exit_code: Option<i32>,
        killed: bool,
    ) -> Result<(), TaskRegistryError> {
        let _ = (id, exit_code, killed);
        Ok(())
    }

    async fn output(
        &self,
        id: &str,
        offset: Option<u64>,
    ) -> Result<TaskOutputChunk, TaskRegistryError>;

    /// Mark a task as having had its terminal output consumed by a reader,
    /// suppressing a later duplicate `<task-notification>`. Mirrors claude-code
    /// `TaskOutputTool`'s `updateTaskState(task_id, t => ({ ...t, notified: true
    /// }))` in both the non-blocking and blocking terminal branches. The task is
    /// retained after notification so callers can still list/read completed
    /// background work until an explicit cleanup/delete path removes it. A
    /// `None`/unknown id is a no-op for callers that cannot guarantee the task
    /// still exists; the default impl is a no-op so existing mock handles compile
    /// unchanged.
    async fn mark_notified(&self, _id: &str) -> Result<(), TaskRegistryError> {
        Ok(())
    }

    /// Record a `local_bash` task's child exit code once the process exits
    /// (M8 cc2.1.198 "Task panels: no stuck Running"): the registry's
    /// `output()` projection derives `exit_code`/`done` from it. Defaulted
    /// no-op so existing mock handles compile unchanged (the frozen-trait
    /// defaulted-method idiom).
    async fn set_exit_code(&self, _id: &str, _exit_code: i32) -> Result<(), TaskRegistryError> {
        Ok(())
    }

    /// Arm a one-shot "came to rest" notification for a PERSISTENT, still-alive
    /// task — the read side of which is surfaced (without eviction) by
    /// [`take_pending_task_notifications`]. Called via the task status sink's
    /// `notify_rest` each time a backgrounded agent parks after a turn-set.
    /// `result` is the agent's final-text response and `usage` its run usage —
    /// both surfaced as the optional `<result>` / `<usage>` notification sections
    /// (the binary `enqueueAgentNotification` always passes them when a result
    /// exists). Default no-op so existing mock handles compile unchanged.
    async fn mark_rested(&self, _id: &str, _result: Option<String>, _usage: Option<AgentRunUsage>) {
    }

    /// Drain the terminal tasks that have NOT yet been surfaced to the model,
    /// marking each `notified` so a given completion is reported exactly once.
    /// Returns a snapshot of each drained task for the
    /// `<task-notification>` renderer, in registry-iteration order.
    ///
    /// 1:1 with claude-code's per-task-type completion path: a task that reaches
    /// a terminal status enqueues exactly one `<task-notification>` and is then
    /// `notified` (guarded by the same `notified` flag's check-and-set), so this
    /// drain is the turn-boundary equivalent of those per-type
    /// `enqueue*Notification` callbacks. A task ALREADY `notified` (e.g. by the
    /// `TaskOutput`/`TaskStop` tool consuming its output) is skipped — no
    /// duplicate. The default impl returns empty so existing mock handles compile
    /// unchanged and builds with no registry stay byte-identical (no reminder).
    async fn take_pending_task_notifications(
        &self,
    ) -> Result<Vec<TaskNotification>, TaskRegistryError> {
        Ok(Vec::new())
    }

    /// Keep a foreground runner alive after Ctrl+B or while its owned tasks
    /// still owe work/results. A true result means the caller must park rather
    /// than publish its final Completed event and deallocate the runner.
    async fn park_foreground_agent(
        &self,
        _agent_id: crate::types::AgentId,
        _outcome: AgentTerminalOutcome,
    ) -> bool {
        false
    }

    /// Whether the handler has acknowledged the runner's completed turn-set.
    /// A taskless/mock runner has no asynchronous lifecycle acknowledgement.
    async fn can_wake_agent_for_task_notification(&self, _agent_id: crate::types::AgentId) -> bool {
        true
    }

    /// Reactivate a parked owner before a notification starts its next turn.
    async fn activate_agent_for_task_notification(&self, agent_id: crate::types::AgentId) {
        if let Ok(rows) = self.list(TaskListFilter::default()).await {
            let id = agent_id.to_string();
            for row in rows {
                if row.is_parked && row.owner_agent_id.as_deref() == Some(id.as_str()) {
                    let _ = self.set_status(&row.task_id, "running").await;
                }
            }
        }
    }

    /// Finish a monitor quietly while publishing its once-only SDK stopped event.
    async fn publish_task_stopped(&self, _task_id: &str) {}

    /// Revision subscription for pending notifications; subscribe before checking pending.
    fn subscribe_task_notifications(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        None
    }

    /// Non-consuming recipient-scoped readiness check.
    async fn has_pending_task_notifications_for(
        &self,
        _recipient: Option<crate::types::AgentId>,
    ) -> bool {
        false
    }

    /// Drain only this recipient. Legacy mocks remain main-session-only.
    async fn take_pending_task_notifications_for(
        &self,
        recipient: Option<crate::types::AgentId>,
    ) -> Result<Vec<TaskNotification>, TaskRegistryError> {
        if recipient.is_none() {
            self.take_pending_task_notifications().await
        } else {
            Ok(Vec::new())
        }
    }

    /// Total number of subagents spawned so far this session (claude 2.1.212
    /// `taskRegistry.getTotalAgentSpawns()`). The `Agent` tool reads this before
    /// every spawn and rejects the launch once it reaches
    /// `CLAUDE_CODE_MAX_SUBAGENTS_PER_SESSION` (default 200). Default impl returns
    /// `0` so existing mock handles compile unchanged and stay uncapped
    /// (frozen-trait defaulted-method idiom); the concrete `TaskRegistry`
    /// overrides this and the reservation methods below with a real per-session
    /// atomic counter.
    ///
    /// [`increment_total_agent_spawns`]: Self::increment_total_agent_spawns
    fn get_total_agent_spawns(&self) -> u64 {
        0
    }

    /// Increment the per-session subagent-spawn counter (claude 2.1.212
    /// `taskRegistry.incrementTotalAgentSpawns()`), called by the `Agent` tool
    /// once a spawn clears the cap gate. Default impl is a no-op so existing mock
    /// handles compile unchanged.
    fn increment_total_agent_spawns(&self) {}

    /// Atomically reserve one per-session subagent-spawn slot, returning the new
    /// count on success or the already-reached count on failure. The default
    /// preserves legacy mock behavior; the production registry overrides it
    /// with a compare/update loop so parallel `Agent` calls cannot race past the
    /// session cap.
    fn try_reserve_total_agent_spawn(&self, cap: u64) -> Result<u64, u64> {
        let current = self.get_total_agent_spawns();
        if current >= cap {
            Err(current)
        } else {
            self.increment_total_agent_spawns();
            Ok(current.saturating_add(1))
        }
    }

    /// Release a reservation when the runtime rejects the launch before a
    /// subagent slot is allocated (for example, a concurrent pool-cap race).
    /// Default no-op keeps legacy stateless handles source-compatible.
    fn release_total_agent_spawn_reservation(&self) {}

    /// Reserve `n` lifetime spawn slots at once (Fusion panels). Rolls back
    /// on failure so a partial hold cannot leak. Default loops the single-slot
    /// reserve so mocks keep working.
    fn try_reserve_total_agent_spawns(&self, n: u64, cap: u64) -> Result<u64, u64> {
        if n == 0 {
            return Ok(self.get_total_agent_spawns());
        }
        let mut last = 0_u64;
        for i in 0..n {
            match self.try_reserve_total_agent_spawn(cap) {
                Ok(count) => last = count,
                Err(count) => {
                    self.release_total_agent_spawn_reservations(i);
                    return Err(count);
                }
            }
        }
        Ok(last)
    }

    /// Release `n` previously reserved lifetime spawn slots.
    fn release_total_agent_spawn_reservations(&self, n: u64) {
        for _ in 0..n {
            self.release_total_agent_spawn_reservation();
        }
    }

    /// Session-wide count of WebSearch calls executed so far — 1:1 with
    /// claude-code's `taskRegistry.getWebSearchCalls(){return n}` (parity
    /// 2.1.212). The `WebSearch` tool reads this before every search and compares
    /// it against `CLAUDE_CODE_MAX_WEB_SEARCHES_PER_SESSION` (default 200) to gate
    /// the session budget. Default `0` so existing mock handles compile unchanged
    /// and a null / no-registry context never caps — byte-identical to the
    /// binary's stub `getWebSearchCalls(){return 0}`.
    fn web_search_calls(&self) -> u32 {
        0
    }

    /// Increment the session WebSearch counter — 1:1 with
    /// `taskRegistry.incrementWebSearchCalls(){n++}`. Called once for every
    /// non-capped `WebSearch` invocation. Default no-op so mock handles and
    /// registry-less contexts are unaffected.
    fn increment_web_search_calls(&self) {}

    /// Reset the session WebSearch counter to zero — 1:1 with
    /// `taskRegistry.resetWebSearchCalls(){n=0}`. Default no-op.
    fn reset_web_search_calls(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// AGT-07. Byte-locked to claude-code `uM`
    /// (`src_180597926.js` @407402): `` `Agent ${Ke} was stopped by the user
    /// and won't be resumed.` ``. Two crates render this one fact, so the bytes
    /// live here and are pinned here.
    #[test]
    fn stopped_by_user_message_matches_the_oracle_bytes() {
        assert_eq!(
            super::stopped_by_user_message("a1b2c3d4e"),
            "Agent a1b2c3d4e was stopped by the user and won't be resumed."
        );
    }

    #[test]
    fn trait_is_object_safe() {
        let _: Option<Arc<dyn TaskRegistryHandle>> = None;
    }

    /// Fixed roster of running tasks, one of each `task_type` the ceiling
    /// teardown must decide about, plus the two it must leave alone.
    struct FixedRoster {
        tasks: Vec<TaskRecord>,
        killed: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl TaskRegistryHandle for FixedRoster {
        async fn create(&self, _input: TaskCreateInput) -> Result<TaskRecord, TaskRegistryError> {
            unimplemented!("not exercised by this test")
        }
        async fn get(&self, _id: &str) -> Result<Option<TaskRecord>, TaskRegistryError> {
            unimplemented!("not exercised by this test")
        }
        async fn list(
            &self,
            _filter: TaskListFilter,
        ) -> Result<Vec<TaskRecord>, TaskRegistryError> {
            Ok(self.tasks.clone())
        }
        async fn update(
            &self,
            _id: &str,
            _patch: TaskUpdatePatch,
        ) -> Result<TaskRecord, TaskRegistryError> {
            unimplemented!("not exercised by this test")
        }
        async fn set_status(
            &self,
            _id: &str,
            _status: &str,
        ) -> Result<TaskRecord, TaskRegistryError> {
            unimplemented!("not exercised by this test")
        }
        async fn kill(&self, id: &str) -> Result<TaskRecord, TaskRegistryError> {
            self.killed.lock().unwrap().push(id.to_string());
            self.tasks
                .iter()
                .find(|t| t.task_id == id)
                .cloned()
                .ok_or_else(|| TaskRegistryError::NotFound(id.to_string()))
        }
        async fn output(
            &self,
            _id: &str,
            _offset: Option<u64>,
        ) -> Result<TaskOutputChunk, TaskRegistryError> {
            unimplemented!("not exercised by this test")
        }
    }

    fn running(task_id: &str, task_type: &str) -> TaskRecord {
        TaskRecord {
            task_id: task_id.into(),
            task_type: task_type.into(),
            status: "running".into(),
            description: "d".into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn ceiling_teardown_stops_local_fusion_alongside_agent_and_workflow() {
        // Finding G003(b): before `local_fusion` was added to this match, the
        // `_ => false` arm made a running Fusion run invisible to
        // `--max-budget-usd`'s ceiling teardown — it kept spending past the
        // cap while `local_agent`/`local_workflow` tasks correctly stopped.
        let registry = FixedRoster {
            tasks: vec![
                running("a1", "local_agent"),
                running("w1", "local_workflow"),
                running("f1", "local_fusion"),
                running("b1", "local_bash"),
                running("m1", "mcp_task"),
            ],
            killed: Mutex::new(Vec::new()),
        };
        let before_stop_calls = AtomicUsize::new(0);
        let count = registry
            .stop_background_agents_for_budget(&|| {
                before_stop_calls.fetch_add(1, Ordering::SeqCst);
            })
            .await
            .unwrap();
        assert_eq!(count, 3, "local_agent + local_workflow + local_fusion");
        assert_eq!(before_stop_calls.load(Ordering::SeqCst), 1);
        let mut killed = registry.killed.lock().unwrap().clone();
        killed.sort();
        assert_eq!(
            killed,
            vec!["a1".to_string(), "f1".to_string(), "w1".to_string()],
            "local_bash and mcp_task must be left alone; local_fusion must be stopped"
        );
    }
}
