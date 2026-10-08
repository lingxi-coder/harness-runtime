//! Hook event taxonomy (spec §9.1).
//!
//! The engine emits one of 33 well-known event kinds at observable points in
//! its lifecycle. Each kind carries a payload describing what just happened
//! (tool input, session metadata, file path, etc.). Registered hooks subscribe
//! by `HookEventType` (the type-tag enum) and inspect the carried `HookEvent`
//! payload when invoked.

use lingxi_core::types::{AgentId, SessionId, ToolUseId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

/// Type-tag for every supported hook event. Used by [`crate::HookDefinition`]
/// to declare which events a hook subscribes to without copying the full
/// payload schema.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum HookEventType {
    /// About to invoke a tool; hook may block, mutate input, or annotate.
    PreToolUse,
    /// Tool completed successfully; hook may augment or post-process result.
    PostToolUse,
    /// Tool returned an error; hook may convert / suppress / re-raise.
    PostToolUseFailure,
    /// A new session has been opened.
    SessionStart,
    /// Session terminated (gracefully or otherwise).
    SessionEnd,
    /// Engine setup phase — one-time initialization after process start.
    Setup,
    /// User submitted a top-level prompt; hook may rewrite or block it.
    UserPromptSubmit,
    /// Agent / session reached a stop signal.
    Stop,
    /// Agent / session stop attempt failed.
    StopFailure,
    /// A subagent is ABOUT to be spawned — the hook may deny it or rewrite
    /// the agent type, model, cwd or background flag (claude-code `agent.spawn`,
    /// `_Bo` @2955987).
    ///
    /// ⚠️ Distinct from [`Self::SubagentStart`], which fires AFTER the spawn
    /// decision and cannot change it.
    AgentSpawn,
    /// A subagent (forked agent) has just been spawned.
    SubagentStart,
    /// A subagent has finished or been cancelled.
    SubagentStop,
    /// About to compact a long context window.
    PreCompact,
    /// Compaction finished; payload reports tokens freed.
    PostCompact,
    /// A requested model switch is about to run; hooks may allow, ask, or
    /// block the switch.
    PreModelSwitch,
    /// A model switch completed; hooks may add context for the next request.
    PostModelSwitch,
    /// Engine is about to ask the user (or auto-policy) for permission.
    PermissionRequest,
    /// A permission request was denied.
    PermissionDenied,
    /// A teammate / agent has gone idle.
    TeammateIdle,
    /// A task (background work item) was created.
    TaskCreated,
    /// A task transitioned to a terminal status.
    TaskCompleted,
    /// An MCP server requested user elicitation.
    Elicitation,
    /// User completed (or cancelled) an elicitation; result available.
    ElicitationResult,
    /// Engine configuration values changed at runtime.
    ConfigChange,
    /// A new git worktree was created.
    WorktreeCreate,
    /// An existing git worktree was removed.
    WorktreeRemove,
    /// Per-session / per-cwd instruction files were (re)loaded.
    InstructionsLoaded,
    /// The current working directory changed (e.g., `cd` tool).
    CwdChanged,
    /// A watched file mutated on disk.
    FileChanged,
    /// A user-visible notification was raised by the engine or a hook.
    Notification,
    /// All tool calls in a batch resolved; fired once after the per-tool
    /// `PostToolUse` hooks, before the next model request (#39, claude-code
    /// registry `nym`, BIN off 205713189: `PostToolBatch:G4t`).
    PostToolBatch,
    /// A slash-command or MCP-prompt was expanded into its underlying prompt
    /// (#39, BIN off 205713189: `UserPromptExpansion:b$t`).
    UserPromptExpansion,
    /// An assistant message delta is about to be displayed (per-flush). Fired
    /// synchronously with per-invocation telemetry suppressed (#39, BIN off
    /// 205713189: `MessageDisplay:aAt`).
    MessageDisplay,
    /// A working directory was added mid-session — `/add-dir`, or the SDK
    /// `register_repo_root` control request (2.1.219).
    DirectoryAdded,
}

/// Expansion source for an [`HookEvent::UserPromptExpansion`] — `slash_command`
/// (a `/`-prefixed slash command) or `mcp_prompt` (an MCP-server-provided
/// prompt). 1:1 with claude-code's input schema
/// `expansion_type:E.enum(["slash_command","mcp_prompt"])` (BIN off 200754686).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptExpansionType {
    /// A `/`-prefixed slash command.
    SlashCommand,
    /// An MCP-server-provided prompt.
    McpPrompt,
}

/// One entry in an [`HookEvent::PostToolBatch`]'s `tool_calls` array. 1:1 with
/// claude-code's `ggp` element schema (BIN off 200753653):
/// `{tool_name:string, tool_input:unknown, tool_use_id:string,
/// tool_response:unknown().optional()}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PostToolBatchCall {
    /// Canonical name of the tool (wire `tool_name`).
    pub tool_name: String,
    /// The tool input the engine dispatched (wire `tool_input`).
    pub tool_input: Value,
    /// Tool invocation ID (wire `tool_use_id`).
    pub tool_use_id: ToolUseId,
    /// Result payload returned by the tool (wire `tool_response`, optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_response: Option<Value>,
}

/// Source of a [`HookEvent::ConfigChange`] — which settings layer (or skills)
/// mutated on disk. 1:1 with claude-code's `CONFIG_CHANGE_SOURCES`
/// (`coreSchemas.ts:662-668`); the wire `source` value is the kebab/snake
/// literal each variant serializes to (e.g. `"user_settings"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigChangeSource {
    /// `~/.lingxi/settings.json` (user-global).
    UserSettings,
    /// `.lingxi/settings.json` (project-shared, checked in).
    ProjectSettings,
    /// `.lingxi/settings.local.json` (project-local, git-ignored).
    LocalSettings,
    /// Enterprise-managed policy settings (never blockable by hooks).
    PolicySettings,
    /// A skill definition changed.
    Skills,
}

/// Which memory tier an [`HookEvent::InstructionsLoaded`] file belongs to.
/// 1:1 with claude-code's `INSTRUCTIONS_MEMORY_TYPES`
/// (`coreSchemas.ts:688-693`). These serialize as `PascalCase` wire literals
/// (`"User"` / `"Project"` / `"Local"` / `"Managed"`), which is why the shared
/// type's derived spelling is used verbatim rather than a `rename_all` — the
/// sibling `InstructionsLoadReason` below is `snake_case`, and that difference
/// is the reference's, not an oversight.
///
/// Narrow on purpose: a bare `lingxi_core::types::Scope` would let this payload carry
/// `Team` or `Session`, which claude-code never sends.
pub use lingxi_core::types::SettingsScope as InstructionsMemoryType;

/// Why an [`HookEvent::InstructionsLoaded`] file was (re)loaded. 1:1 with
/// claude-code's `INSTRUCTIONS_LOAD_REASONS` (`coreSchemas.ts:680-686`); the
/// wire value is the `snake_case` literal each variant serializes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstructionsLoadReason {
    /// Eager load at session start.
    SessionStart,
    /// Lazy load triggered by traversing into a nested directory.
    NestedTraversal,
    /// Conditional rules whose `paths:` glob matched a touched file.
    PathGlobMatch,
    /// Loaded via an explicit `@include` directive.
    Include,
    /// Eager reload after a context compaction.
    Compact,
}

/// Elicitation presentation mode (`form` / `url`). 1:1 with the `mode` enum in
/// claude-code's `ElicitationHookInputSchema` (`coreSchemas.ts:634`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ElicitationMode {
    /// Render a structured form from `requested_schema`.
    Form,
    /// Open a URL for the user to complete out-of-band.
    Url,
}

/// Concrete payload for a hook event. The variant must match the
/// corresponding [`HookEventType`] returned by [`HookEvent::event_type`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HookEvent {
    /// About to invoke a tool. The hook may rewrite `tool_input` or return
    /// a `Block` decision to short-circuit the call.
    PreToolUse {
        /// Canonical name of the tool (e.g. `"Bash"`, `"Read"`).
        tool_name: String,
        /// Raw tool input JSON. Hooks may emit a mutated copy via
        /// [`crate::HookResponse::updated_input`].
        tool_input: Value,
        /// Unique ID of this tool invocation (correlates with `PostToolUse`).
        tool_use_id: ToolUseId,
    },
    /// A tool call completed successfully.
    PostToolUse {
        /// Canonical name of the tool.
        tool_name: String,
        /// The tool input the engine ultimately dispatched (post-mutation).
        tool_input: Value,
        /// Result payload returned by the tool.
        tool_output: Value,
        /// Tool invocation ID, matching the prior `PreToolUse`.
        tool_use_id: ToolUseId,
        /// Tool execution time in ms (excludes permission-prompt + hook time) —
        /// surfaced to the hook payload as `duration_ms` (claude-code 2.1.195).
        /// `None` when the firer does not time the call (e.g. tests).
        duration_ms: Option<u64>,
    },
    /// A tool call returned an error.
    PostToolUseFailure {
        /// Canonical name of the tool.
        tool_name: String,
        /// The tool input the engine ultimately dispatched (post-mutation),
        /// matching the `tool_input` carried by `PostToolUse`.
        tool_input: Value,
        /// Stringified error for log / display.
        error: String,
        /// Tool invocation ID, matching the prior `PreToolUse`.
        tool_use_id: ToolUseId,
        /// Tool execution time in ms (excludes permission-prompt + hook time) —
        /// surfaced as `duration_ms` (claude-code 2.1.195). `None` when untimed.
        duration_ms: Option<u64>,
    },
    /// A new session was opened.
    SessionStart {
        /// New session ID.
        session_id: SessionId,
        /// Origin of the session (e.g. `"cli"`, `"resume"`, `"api"`).
        source: String,
    },
    /// A session terminated.
    SessionEnd {
        /// Session that ended.
        session_id: SessionId,
        /// Stringified reason (`"user_exit"`, `"signal"`, `"error: ..."`).
        reason: String,
    },
    /// Engine startup setup phase. Fired once early in the lifecycle.
    Setup {
        /// Setup trigger (`init` / `maintenance`). claude keys `Setup` hook
        /// matchers on it (`getMatchingHooks` `i = r.trigger`) and threads it
        /// into the wire payload (`{hook_event_name:"Setup", trigger}`).
        trigger: String,
    },
    /// User submitted a top-level prompt. Hooks may rewrite or block.
    UserPromptSubmit {
        /// The user-supplied display prompt text.
        prompt: String,
        /// Exact JavaScript text accepted for this user submission. The display
        /// string above remains available to matchers and UI/log consumers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt_projection: Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    },
    /// Agent / session reached a stop signal.
    Stop {
        /// Stringified reason.
        reason: String,
    },
    /// Agent / session stop attempt failed.
    StopFailure {
        /// Stringified error.
        error: String,
    },
    /// A subagent was spawned.
    /// A subagent is about to be spawned; the hook may deny it or rewrite the
    /// spawn (claude-code `agent.spawn`). Fires BEFORE the decision, unlike
    /// [`Self::SubagentStart`].
    AgentSpawn {
        /// The subagent type the parent asked for.
        agent_type: String,
        /// Resolved model, when the request pinned one.
        model: Option<String>,
        /// Provider profile selected for the child model.
        model_profile: Option<String>,
        /// Working directory the subagent would run in.
        cwd: Option<String>,
        /// Whether the spawn was requested as a background run.
        background: bool,
        /// Parent agent that is spawning, if any.
        parent_agent_id: Option<AgentId>,
    },
    SubagentStart {
        /// Newly assigned agent ID.
        agent_id: AgentId,
        /// Agent type discriminator (e.g. `"general-purpose"`).
        agent_type: String,
        /// Parent agent that spawned this subagent, if any.
        parent_agent_id: Option<AgentId>,
    },
    /// A subagent finished or was cancelled.
    SubagentStop {
        /// Subagent ID.
        agent_id: AgentId,
        /// Stringified status (e.g. `"completed"`, `"cancelled"`).
        status: String,
        /// Agent type discriminator (e.g. `"general-purpose"`), `""` when
        /// unknown. claude-code carries this on the SubagentStop hook input
        /// (`agent_type: a ?? ""`) and keys SubagentStop matchers on it
        /// (`getMatchingHooks` `i = r.agent_type`). Mirrors [`Self::SubagentStart`].
        agent_type: String,
    },
    /// About to compact a long context window.
    PreCompact {
        /// Stringified reason (e.g. `"manual"`, `"threshold"`).
        reason: String,
        /// Optional `/compact` focus text already supplied by the caller. This
        /// is exposed to the hook as `custom_instructions` so it can refine or
        /// augment the same summary request.
        custom_instructions: Option<String>,
    },
    /// Compaction finished.
    PostCompact {
        /// Summary text produced by the compaction pass.
        summary: String,
        /// Number of tokens reclaimed by the compaction.
        tokens_freed: u64,
        /// Compaction trigger (`manual` / `auto`). claude keys `PostCompact`
        /// hook matchers on it (`getMatchingHooks` `i = r.trigger`) and threads
        /// it into the wire payload (`{hook_event_name:"PostCompact", trigger,
        /// compact_summary}`).
        trigger: String,
    },
    /// A requested model switch is about to run. A `PreModelSwitch` hook can
    /// allow, ask for confirmation, or block the switch through its response.
    PreModelSwitch {
        /// Model active before the switch.
        from_model: String,
        /// Model requested by the switch operation.
        to_model: String,
        /// Original requested model, when one was supplied by the caller.
        requested_model: Option<String>,
        /// What initiated the switch (`command`, `picker`, or `sdk`).
        source: String,
        /// Input-context token count used to estimate prompt-cache writes.
        context_tokens: u64,
        /// Whether the prompt cache is warm for the target model.
        prompt_cache_warm: bool,
        /// Prompt-cache lifetime (`5m` or `1h`).
        cache_ttl: String,
        /// Estimated cache-write cost in USD.
        estimated_cache_write_usd: f64,
        /// Pricing provenance (`configured`, `catalog`, or `default`).
        pricing: String,
    },
    /// A model switch completed. A `PostModelSwitch` hook may provide
    /// additional context for the next model request.
    PostModelSwitch {
        /// Model active before the switch.
        from_model: String,
        /// Model active after the switch.
        to_model: String,
        /// Original requested model, when one was supplied by the caller.
        requested_model: Option<String>,
        /// What initiated the switch (`command`, `picker`, `sdk`, `auto`, or
        /// `resume`).
        source: String,
        /// Input-context token count used to estimate prompt-cache writes.
        context_tokens: u64,
        /// Whether the prompt cache is warm for the target model.
        prompt_cache_warm: bool,
        /// Prompt-cache lifetime (`5m` or `1h`).
        cache_ttl: String,
        /// Estimated cache-write cost in USD.
        estimated_cache_write_usd: f64,
        /// Pricing provenance (`configured`, `catalog`, or `default`).
        pricing: String,
    },
    /// Engine is about to request permission.
    PermissionRequest {
        /// Tool the permission applies to.
        tool_name: String,
        /// Tool input that triggered the request.
        tool_input: Value,
        /// Engine-supplied human-readable reason for the prompt.
        reason: String,
    },
    /// A permission request was denied.
    PermissionDenied {
        /// Tool the permission applied to.
        tool_name: String,
        /// Tool input that was denied (wire `tool_input`). 1:1 with the
        /// `PermissionDeniedHookInputSchema` field (`coreSchemas.ts:466`).
        tool_input: Value,
        /// Tool invocation ID, matching the prior `PreToolUse`
        /// (wire `tool_use_id`, `coreSchemas.ts:467`).
        tool_use_id: ToolUseId,
        /// Stringified reason for the denial.
        reason: String,
    },
    /// A teammate's query loop stopped and it is about to park awaiting the
    /// next message ("about to go idle").
    ///
    /// Field set mirrors `TeammateIdleHookInputSchema` (`coreSchemas.ts:591-598`)
    /// so the executor arm can reproduce the wire payload byte-faithfully:
    /// `teammate_name` (required) + `team_name` (required). claude-code fires
    /// `executeTeammateIdleHooks` from `stopHooks.ts:403`, gated on
    /// `isTeammate()`, sourcing `teammate_name` from `getAgentName() ?? ''` and
    /// `team_name` from `getTeamName() ?? ''` — so `team_name` may be `""` when
    /// the firing scope cannot reach the team identity.
    TeammateIdle {
        /// Name of the teammate going idle (wire `teammate_name`, required).
        teammate_name: String,
        /// Team the teammate belongs to (wire `team_name`, required; `""` when
        /// the firing scope has no team identity).
        team_name: String,
    },
    /// A task was created.
    TaskCreated {
        /// Caller-supplied or engine-generated task ID.
        task_id: String,
        /// Task taxonomy bucket.
        task_type: String,
        /// Human-readable description for logs.
        description: String,
        /// Name of the teammate that created the task (wire `teammate_name`,
        /// optional). claude-code sources this from `getAgentName()`
        /// (`TaskCreateTool.ts:97`). `None` for the main thread / leader.
        teammate_name: Option<String>,
        /// Team the creating teammate belongs to (wire `team_name`, optional).
        /// claude-code sources this from `getTeamName()`
        /// (`TaskCreateTool.ts:98`).
        team_name: Option<String>,
    },
    /// A task transitioned to a terminal status.
    ///
    /// Field set mirrors `TaskCompletedHookInputSchema`
    /// (`coreSchemas.ts:614-625`) so the executor arm can reproduce the wire
    /// payload byte-faithfully: `task_subject` (required), `task_description` /
    /// `teammate_name` / `team_name` (optional). `status` is carried for routing
    /// only — it is NOT part of the wire payload (the schema has no `status`
    /// field; claude-code fires `executeTaskCompletedHooks` only on the terminal
    /// transition).
    TaskCompleted {
        /// Task ID matching the prior `TaskCreated`.
        task_id: String,
        /// Terminal status (e.g. `"completed"`, `"failed"`). Routing only — not
        /// serialized into the wire payload.
        status: String,
        /// Task subject/title (wire `task_subject`, required). claude-code
        /// sources this from `existingTask.subject` / `task.subject`.
        task_subject: String,
        /// Task description (wire `task_description`, optional). claude-code
        /// sources this from `existingTask.description` / `task.description`.
        task_description: Option<String>,
        /// Name of the teammate completing the task (wire `teammate_name`,
        /// optional). claude-code sources this from `getAgentName()`.
        teammate_name: Option<String>,
        /// Team the teammate belongs to (wire `team_name`, optional).
        /// claude-code sources this from `getTeamName()`.
        team_name: Option<String>,
    },
    /// An MCP server requested user elicitation.
    Elicitation {
        /// Name of the MCP server (wire `mcp_server_name`).
        server_name: String,
        /// Human-readable prompt shown to the user (wire `message`, required).
        message: String,
        /// Presentation mode (`form` / `url`), if specified.
        mode: Option<ElicitationMode>,
        /// URL to open when `mode == Url`, if specified.
        url: Option<String>,
        /// Server-assigned elicitation ID, if specified.
        elicitation_id: Option<String>,
        /// JSON Schema describing the requested form fields, if specified
        /// (wire `requested_schema`).
        requested_schema: Option<Value>,
    },
    /// User completed (or cancelled) an elicitation.
    ElicitationResult {
        /// Name of the MCP server that requested elicitation.
        server_name: String,
        /// Server-assigned elicitation ID, if specified.
        elicitation_id: Option<String>,
        /// Presentation mode used for the elicitation.
        mode: Option<ElicitationMode>,
        /// User-supplied result payload.
        result: Value,
    },
    /// Engine configuration changed at runtime.
    ConfigChange {
        /// Which settings layer (or skills) changed (wire `source`, required).
        source: ConfigChangeSource,
        /// Path to the changed file, when known (wire `file_path`, optional).
        file_path: Option<PathBuf>,
    },
    /// A new git worktree was created.
    WorktreeCreate {
        /// Requested worktree name (wire `name`, required). This is the only
        /// field in the claude-code `WorktreeCreate` wire schema; the hook's
        /// stdout returns the resolved path, so the input carries just `name`.
        name: String,
        /// Absolute path of the new worktree (engine-side context, not on the
        /// wire).
        path: PathBuf,
        /// Branch name checked out in the worktree (engine-side context, not
        /// on the wire).
        branch: String,
    },
    /// An existing git worktree was removed.
    WorktreeRemove {
        /// Absolute path of the removed worktree.
        path: PathBuf,
    },
    /// A per-cwd / per-session instruction file was (re)loaded. Fired once per
    /// file (claude-code `executeInstructionsLoadedHooks`), so the payload
    /// carries a single `file_path` rather than a list.
    InstructionsLoaded {
        /// The instruction file that was loaded (wire `file_path`, required).
        file_path: PathBuf,
        /// Which memory tier the file belongs to (wire `memory_type`, required).
        memory_type: InstructionsMemoryType,
        /// Why the file was (re)loaded (wire `load_reason`, required).
        load_reason: InstructionsLoadReason,
        /// `paths:` frontmatter globs that gated the load, if any (optional).
        globs: Option<Vec<String>>,
        /// File whose access triggered a lazy/conditional load, if any
        /// (wire `trigger_file_path`, optional).
        trigger_file_path: Option<PathBuf>,
        /// Parent instruction file that `@include`d this one, if any
        /// (wire `parent_file_path`, optional).
        parent_file_path: Option<PathBuf>,
    },
    /// The current working directory changed.
    CwdChanged {
        /// Previous cwd.
        old: PathBuf,
        /// New cwd.
        new: PathBuf,
    },
    /// A watched file mutated on disk.
    FileChanged {
        /// Path of the file that changed.
        path: PathBuf,
        /// Mutation kind (`"create"`, `"modify"`, `"delete"`, `"rename"`).
        kind: String,
    },
    /// A user-visible notification was raised.
    Notification {
        /// Human-readable message.
        message: String,
        /// Notification taxonomy (`"info"`, `"warn"`, `"error"`).
        kind: String,
    },
    /// All tool calls in a batch resolved (#39). Fired ONCE after every tool
    /// call in the batch has resolved (and after the per-tool `PostToolUse`
    /// hooks), before the next model request — claude-code `G4t`
    /// (BIN off 205710327): `{...,hook_event_name:"PostToolBatch",
    /// tool_calls:e}`.
    PostToolBatch {
        /// The full batch of resolved tool calls (wire `tool_calls`, required).
        tool_calls: Vec<PostToolBatchCall>,
    },
    /// A slash-command / MCP-prompt expansion (#39). claude-code `b$t`
    /// (BIN off 201270310): `{...,hook_event_name:"UserPromptExpansion",
    /// expansion_type:e,command_name:t,command_args:n,command_source:r,
    /// prompt:o}`.
    UserPromptExpansion {
        /// Whether the expanded prompt was a slash command or an MCP prompt
        /// (wire `expansion_type`, required).
        expansion_type: PromptExpansionType,
        /// The command name that was expanded (wire `command_name`, required).
        command_name: String,
        /// The raw argument string passed to the command (wire `command_args`,
        /// required; may be empty).
        command_args: String,
        /// The command's source/provenance (wire `command_source`, optional).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command_source: Option<String>,
        /// The expanded prompt text (wire `prompt`, required).
        prompt: String,
    },
    /// A working directory was added to the session mid-run (2.1.219) —
    /// `/add-dir` or the SDK `register_repo_root` control request.
    ///
    /// claude-code `a$t` (BIN off 237753662):
    /// `{...,hook_event_name:"DirectoryAdded",directory:e,source:t}`, dispatched
    /// with `matchQuery: t` — so a hook `matcher` is tested against the SOURCE,
    /// not the path. The dispatcher collects each result's `systemMessage`,
    /// dropping empties.
    DirectoryAdded {
        /// The directory that was added (wire `directory`, required).
        directory: String,
        /// What added it (wire `source`, required) — also the matcher query.
        source: String,
    },
    /// An assistant-message delta is about to be displayed (#39). Fired per
    /// flush, synchronously, with per-invocation telemetry suppressed —
    /// claude-code `aAt` (BIN off 205705090):
    /// `{...,hook_event_name:"MessageDisplay",turn_id:e.turnId,
    /// message_id:e.messageId,index:e.index,final:e.final,delta:e.delta}`.
    MessageDisplay {
        /// UUID of the current turn (wire `turn_id`, required).
        turn_id: String,
        /// UUID of the assistant message being displayed (wire `message_id`,
        /// required; stable across every flush of the same message).
        message_id: String,
        /// Zero-based index of this delta within the message (wire `index`,
        /// required).
        index: u64,
        /// Whether this is the final delta of the message (wire `final`,
        /// required). Renamed from the Rust keyword `final` via serde.
        #[serde(rename = "final")]
        is_final: bool,
        /// The delta text being displayed (wire `delta`, required).
        delta: String,
    },
}

/// A `hook_progress` progress message emitted once per matching hook *before*
/// that hook executes, mirroring claude-code's progress yield
/// (`utils/hooks.ts:2094-2116`):
///
/// ```ts
/// yield { message: { type: 'progress', data: {
///   type: 'hook_progress', hookEvent, hookName, command: getHookDisplayText(hook),
///   ...(hook.type === 'prompt' && { promptText: hook.prompt }),
///   ...('statusMessage' in hook && hook.statusMessage != null &&
///     { statusMessage: hook.statusMessage }),
/// }, … } }
/// ```
///
/// The spinner consumes [`Self::status_message`] in place of the default
/// `Running {event} hook…` line when it is `Some`. This crate ORIGINATES the
/// event (carrying the per-hook `statusMessage` text from the definition); the
/// TUI render that swaps it into the spinner line is presentation work tracked
/// separately (see the `hook_progress` component).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookProgressEvent {
    /// The hook event name (e.g. `"PreToolUse"`) — claude-code `hookEvent`.
    pub hook_event: String,
    /// Human-readable hook name — claude-code `hookName`.
    pub hook_name: String,
    /// Per-hook spinner override text. `Some` only when the hook declared a
    /// `statusMessage`; `None` falls back to the engine's generic running
    /// line. Mirrors the conditional-spread of claude-code's `statusMessage`
    /// field — the key is present in the TS payload only when non-null, which
    /// `Option` models exactly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_message: Option<String>,
}

impl HookEvent {
    /// Return the type-tag for this event payload. Used by the registry to
    /// match events against subscribed hooks without copying the payload.
    #[must_use]
    pub fn event_type(&self) -> HookEventType {
        match self {
            Self::PreToolUse { .. } => HookEventType::PreToolUse,
            Self::PostToolUse { .. } => HookEventType::PostToolUse,
            Self::PostToolUseFailure { .. } => HookEventType::PostToolUseFailure,
            Self::SessionStart { .. } => HookEventType::SessionStart,
            Self::SessionEnd { .. } => HookEventType::SessionEnd,
            Self::Setup { .. } => HookEventType::Setup,
            Self::UserPromptSubmit { .. } => HookEventType::UserPromptSubmit,
            Self::Stop { .. } => HookEventType::Stop,
            Self::StopFailure { .. } => HookEventType::StopFailure,
            Self::AgentSpawn { .. } => HookEventType::AgentSpawn,
            Self::SubagentStart { .. } => HookEventType::SubagentStart,
            Self::SubagentStop { .. } => HookEventType::SubagentStop,
            Self::PreCompact { .. } => HookEventType::PreCompact,
            Self::PostCompact { .. } => HookEventType::PostCompact,
            Self::PreModelSwitch { .. } => HookEventType::PreModelSwitch,
            Self::PostModelSwitch { .. } => HookEventType::PostModelSwitch,
            Self::PermissionRequest { .. } => HookEventType::PermissionRequest,
            Self::PermissionDenied { .. } => HookEventType::PermissionDenied,
            Self::TeammateIdle { .. } => HookEventType::TeammateIdle,
            Self::TaskCreated { .. } => HookEventType::TaskCreated,
            Self::TaskCompleted { .. } => HookEventType::TaskCompleted,
            Self::Elicitation { .. } => HookEventType::Elicitation,
            Self::ElicitationResult { .. } => HookEventType::ElicitationResult,
            Self::ConfigChange { .. } => HookEventType::ConfigChange,
            Self::WorktreeCreate { .. } => HookEventType::WorktreeCreate,
            Self::WorktreeRemove { .. } => HookEventType::WorktreeRemove,
            Self::InstructionsLoaded { .. } => HookEventType::InstructionsLoaded,
            Self::CwdChanged { .. } => HookEventType::CwdChanged,
            Self::FileChanged { .. } => HookEventType::FileChanged,
            Self::Notification { .. } => HookEventType::Notification,
            Self::PostToolBatch { .. } => HookEventType::PostToolBatch,
            Self::UserPromptExpansion { .. } => HookEventType::UserPromptExpansion,
            Self::MessageDisplay { .. } => HookEventType::MessageDisplay,
            Self::DirectoryAdded { .. } => HookEventType::DirectoryAdded,
        }
    }
}

#[cfg(test)]
mod event_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn event_type_roundtrips_for_new_events() {
        // #39: the three new variants map to their type-tags and the tags
        // serialize to their PascalCase wire names.
        for (ev, tag, name) in [
            (
                HookEvent::PostToolBatch { tool_calls: vec![] },
                HookEventType::PostToolBatch,
                "PostToolBatch",
            ),
            (
                HookEvent::UserPromptExpansion {
                    expansion_type: PromptExpansionType::SlashCommand,
                    command_name: "c".into(),
                    command_args: String::new(),
                    command_source: None,
                    prompt: "p".into(),
                },
                HookEventType::UserPromptExpansion,
                "UserPromptExpansion",
            ),
            (
                HookEvent::MessageDisplay {
                    turn_id: "t".into(),
                    message_id: "m".into(),
                    index: 0,
                    is_final: false,
                    delta: "d".into(),
                },
                HookEventType::MessageDisplay,
                "MessageDisplay",
            ),
            (
                HookEvent::DirectoryAdded {
                    directory: "/tmp/x".into(),
                    source: "add_dir".into(),
                },
                HookEventType::DirectoryAdded,
                "DirectoryAdded",
            ),
        ] {
            assert_eq!(ev.event_type(), tag);
            assert_eq!(serde_json::to_string(&tag).unwrap(), format!("\"{name}\""));
        }
    }

    #[test]
    fn post_tool_batch_call_omits_tool_response_when_none() {
        let call = PostToolBatchCall {
            tool_name: "Bash".into(),
            tool_input: json!({ "command": "ls" }),
            tool_use_id: ToolUseId::new(),
            tool_response: None,
        };
        let s = serde_json::to_string(&call).unwrap();
        assert!(s.contains(r#""tool_name":"Bash""#), "{s}");
        assert!(!s.contains("tool_response"), "None omits the key: {s}");
        // present when Some
        let call2 = PostToolBatchCall {
            tool_response: Some(json!("out")),
            ..call
        };
        let s2 = serde_json::to_string(&call2).unwrap();
        assert!(s2.contains(r#""tool_response":"out""#), "{s2}");
    }

    #[test]
    fn prompt_expansion_type_wire_values_are_snake_case() {
        assert_eq!(
            serde_json::to_string(&PromptExpansionType::SlashCommand).unwrap(),
            r#""slash_command""#
        );
        assert_eq!(
            serde_json::to_string(&PromptExpansionType::McpPrompt).unwrap(),
            r#""mcp_prompt""#
        );
    }
}
