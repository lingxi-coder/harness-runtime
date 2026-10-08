//! Per-spawn runtime context for a subagent.
//!
//! [`SubagentContext`] is the bundle of data the host hands to a
//! [`crate::pool::StateMachinePool`] slot at allocation time. It carries
//! identity, prompt material, tool exposure, optional worktree handle, and
//! the bridges into the various cross-cutting subsystems (memory, MCP,
//! transcripts, content replacement). See spec §10.3.

use crate::definition::AgentDefinition;
use crate::display::AgentDisplay;
use lingxi_core::host::WorktreeHandle;
use lingxi_core::types::{AgentId, ConversationMessage, McpConnectionId, SessionId};
use memory::snapshot::AgentMemorySnapshot;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tool_api::content_replacement::ContentReplacementState;

/// All the state required to drive one subagent run.
///
/// Cloning is cheap: heavy state (replacement, memory snapshot) is wrapped
/// in [`Arc`] so callers can hand the context to background tasks without
/// re-allocating.
#[derive(Clone)]
pub struct SubagentContext {
    /// Stable identifier for this spawn — every event carries this id.
    pub agent_id: AgentId,
    /// Shared registry used for recipient-scoped notification folding.
    pub task_registry: Option<Arc<dyn lingxi_core::host::task_registry::TaskRegistryHandle>>,
    /// Parent agent id, when this agent was dispatched by another agent.
    pub parent_agent_id: Option<AgentId>,
    /// Host-trusted caller/origin snapshot inherited by nested Agent launches.
    pub agent_spawn_provenance: lingxi_core::host::subagent_spawn::AgentSpawnProvenance,
    /// DISPLAY NAME of this agent when it is an in-process teammate in a swarm
    /// (claude-code `TeammateContext.agentName`, surfaced via `getAgentName()`).
    /// For a teammate this is its human name (e.g. `"researcher"`), NOT the
    /// `agent:<uuid>` form of [`Self::agent_id`]. `None` for one-shot subagents
    /// and the main thread. The runner threads this into every dispatched
    /// tool's [`lingxi_core::host::tool_invoker::SubagentInvocationContext`] so the
    /// swarm-only `TaskUpdate` side-effects (auto-owner / owner-change mailbox
    /// notification) key on the NAME (matching `getAgentStatuses`).
    pub agent_name: Option<String>,
    /// TEAM NAME this teammate belongs to (claude-code
    /// `TeammateContext.teamName`, surfaced via `getTeamName()`). Threaded into
    /// every dispatched tool's [`lingxi_core::host::tool_invoker::SubagentInvocationContext`]
    /// so `getTaskListId()` resolves the teammate to the leader's on-disk task
    /// directory. `None` for one-shot subagents / standalone sessions.
    pub team_name: Option<String>,
    /// Static definition that gave rise to this spawn.
    pub agent_definition: AgentDefinition,
    /// Initial conversation messages seeded into the agent's state machine.
    pub prompt_messages: Vec<ConversationMessage>,
    /// For fork-mode runs, the byte-exact context to splice from the parent.
    pub fork_context_messages: Option<Vec<ConversationMessage>>,
    /// Tool names the agent is allowed to call. Computed by
    /// [`crate::tool_resolver::AgentToolResolver`].
    ///
    /// Also the dispatch-time enforcement key: [`crate::runner::run_subagent`]
    /// refuses any `tool_use` whose name is not in a NON-EMPTY list (before
    /// invoking the inherited tool invoker). EMPTY = no restriction (the guard
    /// is skipped) — NOT "no tools allowed"; an empty resolver result therefore
    /// means unrestricted, so a caller wiring this from a real policy must
    /// produce the full resolved set, never an empty list, for a locked-down
    /// agent. The production spawner populates this from
    /// [`crate::tool_resolver::AgentToolResolver`] (the same resolved set it
    /// serializes into [`Self::tool_schemas`]), so the advertised set and the
    /// allow-list stay in lock-step. The policy comes from the spawn-resolved
    /// [`crate::definition::AgentDefinition`] (built-in or user/project), so a
    /// read-only agent (e.g. `Explore`/`Plan`, `AgentToolPolicy::Except` of the
    /// write tools) genuinely narrows this list; a `general-purpose` agent
    /// (`All`) keeps every registered tool name.
    pub allowed_tools: Vec<String>,
    /// Optional worktree the agent runs inside.
    pub worktree_handle: Option<WorktreeHandle>,
    /// Per-agent working directory the agent's tools operate in — the worktree
    /// path (`isolation:"worktree"`) or an explicit `cwd` override. Threaded by
    /// the runner into every dispatched tool's
    /// [`lingxi_core::host::tool_invoker::SubagentInvocationContext::cwd`] →
    /// `ToolUseContext.cwd`, so the agent's filesystem + shell tools run there
    /// instead of the shared session workspace (claude-code's per-agent
    /// `agentWorktree`/cwd). `None` for a non-isolated agent (tools use the shared
    /// workspace, byte-identical to before).
    pub cwd: Option<std::path::PathBuf>,
    /// `true` when the agent runs asynchronously (e.g. background scan).
    pub is_async: bool,
    /// `true` for a long-lived, message-driven teammate: after a turn-set ends
    /// with a terminal stop the runner parks awaiting the next inbound
    /// [`lingxi_core::Event::UserMessage`] instead of returning. Terminates only on
    /// [`lingxi_core::Event::UserExit`] / [`lingxi_core::Event::UserInterrupt`] or when
    /// the inbound event channel closes. Distinct from [`Self::is_async`],
    /// which only governs background-vs-foreground scheduling.
    pub persistent: bool,
    /// Whether the agent may surface permission prompts to the user.
    pub can_show_permission_prompts: bool,
    /// Owning session mode for prompt/provider gates inside this independently
    /// spawned runner. `None` preserves the legacy process-global fallback.
    pub session_interactive: Option<bool>,
    /// Trusted originating session propagated across nested Agent spawns.
    /// Unlike the hook session id, this is the budget/accounting owner for
    /// descendants and is never inferred by a receiver.
    pub origin_session_id: Option<SessionId>,
    /// MCP connections the agent should attach to.
    pub mcp_clients: Vec<McpConnectionId>,
    /// Directory under which this agent writes its transcript.
    pub transcript_subdir: PathBuf,
    /// Filesystem used to APPEND this agent's conversation to
    /// `<transcript_subdir>/agent-<id>.jsonl`.
    ///
    /// `transcript_subdir` alone only ever named a path: the `SubagentStop`
    /// hook reported `agent_transcript_path` while nothing wrote the file, so
    /// the payload pointed at something that did not exist. Wiring this makes
    /// the transcript real, and is the prerequisite for reconstructing a
    /// background agent's conversation outside the process that ran it.
    ///
    /// `None` ⇒ nothing is persisted (tests / minimal builds), byte-identical
    /// to the previous behaviour.
    #[allow(clippy::struct_field_names)]
    pub transcript_fs: Option<Arc<dyn lingxi_core::host::FileSystem>>,
    /// A conversation recovered from this agent's persisted transcript, used to
    /// RESTORE it in a later process.
    ///
    /// When present it REPLACES the normal seeding — fork context, prompt, and
    /// the `SubagentStart` / frontmatter-skills preload — rather than prefixing
    /// it. All three are already inside the recovered history (they were
    /// persisted the first time round), so re-running them would re-inject
    /// context the agent has already seen and re-fire start hooks for a run
    /// that started in another process.
    ///
    /// `None` ⇒ a fresh spawn, seeded as before.
    pub resumed_history: Option<Vec<ConversationMessage>>,
    /// Pre-rendered system prompt (post template + frontmatter expansion).
    pub rendered_system_prompt: Option<Arc<str>>,
    /// Fixed engine-owned mobile runtime reminder for a fresh child. Rendered
    /// after this spawn's tool policy is resolved, so shell details are exposed
    /// only when `Shell` is in the child's actual advertised tool set. The
    /// runner persists it with the initial history; resumed agents therefore
    /// recover it from `resumed_history` and do not inject it again.
    pub mobile_runtime_environment_reminder: Option<Arc<str>>,
    /// Per-spawn guest workspace reminder, kept separate from the stable mobile
    /// runtime prefix because explicit cwd/worktree isolation can vary.
    pub mobile_runtime_workspace_reminder: Option<Arc<str>>,
    /// Resolved user context and per-agent lazy instruction cursor.
    pub instruction_context: lingxi_core::host::instructions::InstructionContext,
    /// Explicit fork userContext overrides bypass omitInstructions selection.
    pub instruction_context_is_override: bool,
    pub instruction_provider:
        Option<Arc<dyn lingxi_core::host::instructions::InstructionContextProvider>>,
    /// Shared content-replacement state (e.g. file mention expansion). Wrapped
    /// in a `Mutex` so the tool layer can mutate it across awaits.
    pub content_replacement_state: Option<Arc<Mutex<ContentReplacementState>>>,
    /// Snapshot of the memory tier visible to this agent type.
    pub agent_memory: Option<AgentMemorySnapshot>,
    /// UI display configuration (color, icon).
    pub display: AgentDisplay,
    /// Provider profile name used to route this subagent's model round-trips to
    /// a specific provider (the dual-LLM candidate's resolved profile). Threaded
    /// from [`lingxi_core::host::subagent_spawn::SubagentSpawnRequest::model_profile`] by the
    /// spawner; the runner passes it as the `profile` arg of the api client's
    /// typed streaming request. `None` selects the host's current default
    /// provider route.
    pub model_profile: Option<String>,
    /// Live configured routes used to resolve model changes returned by child
    /// tools against this agent's current model and provider profile.
    pub model_resolution_context_provider:
        Option<Arc<dyn crate::model_resolution::ModelResolutionContextProvider>>,
    /// Managed model restriction snapshot used to validate a provider-admitted
    /// server fallback before the child accepts its response. This carries no
    /// provider catalog; `None` means the host supplied no restriction.
    pub server_fallback_model_enforcement: Option<llm_runtime::model::allowlist::ModelEnforcement>,
    /// The refusal-fallback CHAIN this run may walk when the model refuses.
    ///
    /// claude-code runs subagents through the same query generator as the main
    /// thread, so they inherit its cascade; here the two loops are separate, so
    /// the chain is handed down explicitly. Empty (the default) means a refusal
    /// ends the run, which is what this port did for every subagent before.
    pub refusal_fallback_chain: Vec<String>,
    /// Model API for the multi-turn runner. `None` is an unconfigured host
    /// and fails before instruction discovery or startup writes.
    pub api_client: Option<Arc<dyn crate::api::SubagentApiClient>>,
    /// Tool dispatch seam inherited from the parent via
    /// [`lingxi_core::host::subagent_spawn::SubagentInheritance`]. `None` means the agent
    /// cannot dispatch tools — a `tool_use` in that state surfaces a failure.
    pub tool_invoker: Option<Arc<dyn lingxi_core::host::ToolInvoker>>,
    /// Per-agent passive LSP diagnostics cursor. Each spawned agent receives
    /// an independent source so consuming a builder reminder cannot suppress
    /// the same diagnostic for the main conversation or another agent.
    ///
    /// The runner polls this immediately after a `Write`/`Edit` tool result;
    /// implementations may wait briefly for the matching LSP publication.
    pub new_diagnostics_source: Option<Arc<dyn lingxi_core::host::NewDiagnosticsSource>>,
    /// Wire tool definitions (`{name, description, input_schema}`) advertised to
    /// the model on every round-trip of the multi-turn loop — the streaming
    /// analog of the orchestrator's own `tools` array. Built by the spawner via
    /// [`tool_api::wire::tools_to_wire`]. Empty means the subagent calls the
    /// model with no tools (so it cannot emit `tool_use`).
    ///
    /// LIVE in production, resolved PER-SPAWN: the spawner holds the live tool
    /// registry (filled after it is built — breaking the construction cycle where
    /// the spawner sits inside the `BuiltinToolContext` that builds the registry)
    /// and, at each spawn, runs [`crate::tool_resolver::AgentToolResolver`] over
    /// the registry's `available_tools` per the child's
    /// [`crate::definition::AgentToolPolicy`], serializing the result here AND
    /// recording the resolved names into [`Self::allowed_tools`]. So the
    /// advertised set and the dispatch allow-list narrow TOGETHER, and
    /// resolution reflects the registry's state at spawn time rather than a
    /// one-time serialized snapshot taken at boot. (The shared `Arc<ToolRegistry>`
    /// is immutable once handed to the spawner, so this picks up boot-time
    /// registry state, not live post-boot mutation.) The production
    /// [`Self::tool_invoker`] (`RegistryToolInvoker`) does not enforce policy
    /// itself (`find_by_name` + `tool.call`), so the runner's allow-list check on
    /// `allowed_tools` is the dispatch-time guard. The spawn path now resolves a
    /// REAL [`crate::definition::AgentDefinition`] per `subagent_type` (the 6
    /// built-ins from [`crate::builtins`], overridden by the file catalog), so
    /// the policy is per-agent: a read-only agent narrows this advertised set
    /// AND `allowed_tools` together, while `general-purpose` keeps the full set.
    pub tool_schemas: Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    /// Structured-output schema (JSON Schema string) forwarded from
    /// [`lingxi_core::host::subagent_spawn::SubagentSpawnRequest::schema`]. When `Some`, the
    /// runner injects a forced `StructuredOutput` tool and returns the model's
    /// tool input as the result. `None` ⇒ free-form text output.
    pub schema: Option<String>,
    /// Per-turn `tool_choice` policy applied while [`Self::schema`] is `Some`,
    /// forwarded from
    /// [`lingxi_core::host::subagent_spawn::SubagentSpawnRequest::structured_output_mode`].
    /// Ignored when [`Self::schema`] is `None`.
    pub structured_output_mode: lingxi_core::host::subagent_spawn::StructuredOutputMode,
    /// Maximum opt-in malformed StructuredOutput retries across this run (0..=2).
    pub structured_output_parse_retries: u32,
    /// Inherited budget enforcer (from `SubagentInheritance::budget`). When
    /// `Some`, the multi-turn loop consults it once per turn and stops with a
    /// budget-exhausted terminal when the cumulative cost is over the limit.
    /// `None` disables budget enforcement (legacy/test contexts).
    pub budget: Option<Arc<dyn lingxi_core::host::budget::BudgetEnforcerHandle>>,
    /// Hook executor the runner fires `SubagentStart` through to collect the
    /// hooks' `additionalContexts` and inject them into the child's initial
    /// messages (claude `runAgent.ts:530-555`), and to register/clear the
    /// agent's frontmatter hooks scoped to this `agent_id` (Stop→SubagentStop,
    /// `runAgent.ts:557-575`). The SAME `Arc<HookExecutorImpl>` the orchestrator
    /// fires its other hooks through (engine-desktop fills it post-construction
    /// via the spawner's set-once cell). `None` (tests / minimal builds) ⇒ the
    /// runner skips SubagentStart firing + frontmatter-hook registration, keeping
    /// the child's history byte-identical to legacy.
    pub hook_executor: Option<Arc<hooks::HookExecutorImpl>>,
    /// Whether this producer has a consumer for session/plugin terminal hooks.
    pub agent_spawn_token: Option<lingxi_core::host::agent_statistics::AgentSpawnToken>,
    pub stop_hook_scope: lingxi_core::host::subagent_spawn::SubagentStopScope,
    /// Session epoch captured at admission. Snapshot publication cannot bind a
    /// retained child to a later restored run or session generation.
    pub subagent_stop_firer:
        Option<Arc<dyn lingxi_core::host::subagent_spawn::SubagentStopHookFirer>>,
    /// Managed `strictPluginOnlyCustomization:["hooks"]` decision captured by
    /// the composition root. When true, user/project definitions may not
    /// register command-capable frontmatter hooks.
    pub strict_plugin_only_hooks: bool,
    /// Skill loader the runner uses to preload the agent definition's
    /// frontmatter `skills:` into the child's initial messages (claude
    /// `runAgent.ts:577-646`). A leaf-trait seam (see [`lingxi_core::host::skill_loader`])
    /// so the agent crate avoids a cycle into the command/skill registry. `None`
    /// ⇒ no skill preloading (byte-identical legacy).
    pub skill_loader: Option<Arc<dyn lingxi_core::host::skill_loader::SkillLoader>>,
    /// Session id stamped on the `HookContext` the runner builds for the
    /// SubagentStart fire + frontmatter-hook registration (the orchestrator's
    /// session). Only consulted when [`Self::hook_executor`] is `Some`.
    pub hook_session_id: SessionId,
    /// Engine cwd stamped on that `HookContext`. Only consulted when
    /// [`Self::hook_executor`] is `Some`.
    pub hook_cwd: PathBuf,
    /// This agent's recursion depth (claude `agentContext.depth`): the main
    /// thread is 0, a subagent is its spawning parent's depth + 1 (set by the
    /// spawner from [`lingxi_core::host::subagent_spawn::SubagentSpawnRequest::depth`]). The
    /// runner threads it into every dispatched tool's
    /// [`lingxi_core::host::tool_invoker::SubagentInvocationContext::depth`] →
    /// `ToolUseContext.depth`, and the spawner passes it to
    /// [`crate::tool_resolver::AgentToolResolver`] to gate the `Agent` tool at
    /// `depth < LINGXI_MAX_SUBAGENT_SPAWN_DEPTH` (default 1).
    pub depth: u32,
    /// Observer declaration whose companion watches this agent and, when
    /// enabled, propagates to recursive children.
    pub observer: Option<lingxi_core::host::subagent_spawn::ObserverSpec>,
    /// The child's EFFECTIVE permission-context mode as a WIRE string
    /// (`"plan"`/`"acceptEdits"`/…), computed by [`crate::handle`] from the Agent
    /// tool `mode` clamped against the parent's live mode (claude-code 2.1.207
    /// `wKe`/`ve`) or the agent definition's own permission mode. `Some` ⇒ the
    /// runner threads it into every dispatched tool's
    /// [`lingxi_core::host::tool_invoker::SubagentInvocationContext::mode_override`] so the
    /// child's tool-dispatch permission checks run under this mode — e.g. a
    /// `mode:"plan"` child gates mutations (`Edit`/`Write`/`Bash`) while reads stay
    /// frictionless. `None` ⇒ the child inherits the shared gate's live/boot mode
    /// (byte-identical to pre-2.1.207). The fork path never sets it.
    pub permission_mode_override: Option<String>,
    /// Command-deny rules FROZEN at fork time (claude `freezeCommandDenies`),
    /// carried from [`lingxi_core::host::subagent_spawn::SubagentSpawnRequest`] so the
    /// runner can replay them on every dispatched tool call.
    ///
    /// See [`lingxi_core::host::tool_invoker::SubagentInvocationContext::frozen_command_denies`]
    /// for why they exist and why a frozen deny beats a live allow. Empty for
    /// every non-fork spawn, which keeps the dispatch path unchanged.
    pub frozen_command_denies: Vec<String>,
    /// Optional per-turn output token cap (Fusion panel ceilings).
    pub max_output_tokens_per_turn: Option<u32>,
    /// Optional per-turn input payload cap in bytes (oldest messages dropped).
    pub max_input_bytes_per_turn: Option<u64>,
    /// Optional COGS query-source label forwarded to the API client.
    pub query_source_label: Option<String>,
    /// Caller correlation id, forwarded from
    /// [`lingxi_core::host::subagent_spawn::SubagentSpawnRequest::correlation_id`]
    /// (Fusion's `{run_id}:p{index}` stamp, `fusion::panel::spawn_request`).
    /// `None` for spawns that don't set one. Threaded onto the transcript's
    /// metadata (via [`crate::transcript::AgentTranscriptWriter`]) so a
    /// panel's transcript file can be matched back to its run and panel
    /// index (G011).
    pub correlation_id: Option<String>,
    /// Typed registered run context, retained across rounds and never serialized.
    ///
    /// Deliberately LAST, for the same reason as
    /// [`lingxi_core::host::subagent_spawn::SubagentSpawnRequest::model_attempt`]:
    /// `main` grows this struct from the front.
    pub model_attempt: Option<lingxi_core::host::ModelAttemptContext>,
    /// Private reporting contract and runtime-minted run capability.
    pub handback: Option<Arc<crate::handback::HandbackRuntime>>,
    /// One-shot cold-restore publication barrier, independent of tool opt-in.
    pub handback_restore_start: Option<lingxi_core::host::handback::HandbackRestoreParticipant>,
}

/// Held outside the runner future so cancellation before its first poll still
/// releases a published cold-restore slot. Arrival is idempotent.
pub(crate) struct HandbackRestoreStartupGuard(
    pub Option<lingxi_core::host::handback::HandbackRestoreParticipant>,
);

impl Drop for HandbackRestoreStartupGuard {
    fn drop(&mut self) {
        if let Some(participant) = &self.0 {
            participant.release_failed();
        }
    }
}
