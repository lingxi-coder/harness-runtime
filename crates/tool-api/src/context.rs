//! Per-invocation tool context.
//!
//! [`ToolUseContext`] carries everything a tool needs about the surrounding
//! call: conversation history, the active tool-use id, agent id, options,
//! and shared state (content replacement, file-state cache in Plan 10).

use crate::content_replacement::ContentReplacementState;
use crate::registry::ToolRegistry;
use lingxi_core::host::audio::{
    AudioError, AudioErrorKind, AudioInitiator, AudioOperationContext, AudioOperationId, AudioOwner,
};
use lingxi_core::host::tool_invoker::ToolExecutionPolicy;
use lingxi_core::types::{
    utf16_json::Utf16JsonProjection, AgentId, McpConnectionId, MessageId, SessionId, ToolUseId,
};
use lingxi_core::SessionState;
use media_api::{MediaOperationId, MediaSessionRef};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Per-invocation context passed to every [`crate::Tool`] method that needs
/// to reason about the current call (validation, permission, execution).
///
/// Cloning is cheap: most fields are `Arc`-shared or `Copy`. The file-state
/// cache slot lands in Plan 10.
#[derive(Clone)]
pub struct ToolUseContext {
    /// Static per-call options (debug flags, budget, system-prompt overrides).
    pub options: ToolUseOptions,
    /// Conversation history up to (but not including) the current call.
    pub messages: Vec<lingxi_core::types::ConversationMessage>,
    /// The tool-use id assigned by the model, if this call is bound to one.
    pub tool_use_id: Option<ToolUseId>,
    /// The current assistant message that owns this tool-use batch, if known.
    /// Main-loop dispatch sets this to the live assistant turn id so tools and
    /// telemetry hooks can attribute validation failures to the current
    /// request. Omitted for call sites that have no current assistant message
    /// (for example isolated unit tests and subagent helper invocations).
    pub assistant_message_id: Option<MessageId>,
    /// Raw assistant row for the currently dispatched completed block. This
    /// is independent of `messages` (the pre-query history snapshot) and is
    /// refreshed from trusted host facts for every nested invocation.
    pub assistant_message: Option<lingxi_core::types::ConversationMessage>,
    /// Tool-use blocks from earlier sibling rows in the same query, excluding
    /// the current assistant row. The host refreshes this on every call.
    pub same_turn_tool_uses: Vec<lingxi_core::types::ContentBlock>,
    /// Effective tool names visible to the model query that produced this call,
    /// including registered aliases. `None` means the caller did not supply a
    /// tool list; `Some([])` is an explicitly empty caller list.
    pub available_tool_names: Option<Vec<Utf16JsonProjection>>,
    /// The agent that issued this call, if known.
    pub agent_id: Option<AgentId>,
    /// Native per-context nested-memory triggers; independent of file cache.
    pub nested_memory_triggers: Arc<crate::nested_memory_triggers::NestedMemoryTriggers>,
    /// The DISPLAY NAME of the teammate that issued this call, if known
    /// (claude-code `getAgentName()`). For an in-process teammate this is its
    /// human name (e.g. `"researcher"`), NOT the `agent:<uuid>` form of
    /// [`Self::agent_id`]. `None` for the main thread / leader (TS
    /// `getAgentName()` returns `undefined`). The swarm-only `TaskUpdate`
    /// side-effects (auto-owner, owner-change mailbox notification) key on this
    /// NAME so on-disk owners / mailbox senders match what `getAgentStatuses`
    /// looks up (name, never `agent:<uuid>`).
    pub agent_name: Option<String>,
    /// The TEAM NAME the issuing teammate belongs to, if known (claude-code
    /// `getTeammateContext()?.teamName`). Consulted by `getTaskListId()`
    /// (priority 2) so in-process teammates resolve to the leader's on-disk task
    /// directory. `None` for the main thread / standalone sessions.
    pub team_name: Option<String>,
    /// Trusted session that originated a nested subagent tool invocation.
    /// Main-loop calls normally leave this unset because [`Self::session`]
    /// exposes the live session directly. Subagent dispatch has no writable
    /// session state, so recursive Agent, Fusion, and Workflow calls use this
    /// immutable identity to keep their cost and budget accounting attached to
    /// the originating conversation.
    pub origin_session_id: Option<SessionId>,
    /// Host-captured plugin caller/origin for nested `agent.spawn` events.
    /// Mod-rewritten JSON never supplies or overwrites this identity.
    pub agent_spawn_provenance: lingxi_core::host::subagent_spawn::AgentSpawnProvenance,
    /// Current Native query-function depth for a main-loop tool invocation.
    /// None means this call was not produced by a tracked model query.
    pub native_query_depth: Option<u32>,
    /// Trusted host origin for this turn. Scheduled turns cannot transfer
    /// ownership to interactive-only continuations such as media realtime.
    pub is_scheduled_turn: bool,
    /// Trusted host-selected execution policy for this invocation. The nested
    /// dispatch path copies it from `SubagentInvocationContext`; ordinary
    /// callers use [`ToolExecutionPolicy::Ordinary`].
    pub tool_execution_policy: ToolExecutionPolicy,
    /// Trusted effective permission mode for recursive Agent launches. Nested
    /// dispatch derives this from its host override or the enforcing live gate.
    pub trusted_effective_permission_mode: Option<String>,
    /// Invocation-local verdict from the bound classifier-only permission
    /// path. Tool bodies may annotate reports but cannot mint this result.
    pub classifier_only_review: Option<lingxi_core::host::handback::ReportReview>,
    /// Host snapshot inherited by ordinary and forked children.
    pub instruction_context: Option<lingxi_core::host::instructions::InstructionContext>,
    /// Shared content-replacement state. Populated in Task 3.
    pub content_replacement_state: Option<Arc<Mutex<ContentReplacementState>>>,
    /// Mutable session state (M4-04). Tools that mutate the conversation
    /// (`TodoWrite`, `EnterPlanMode`, `ExitPlanMode`) acquire the `Mutex`
    /// before reading or writing. `None` for legacy call sites that have not
    /// wired a session yet; M4-04 tools surface a clear error in that case.
    pub session: Option<Arc<Mutex<SessionState>>>,
    /// Parent's tool registry — `AgentTool` clones this `Arc` into the child
    /// subagent's invocation context so the recursion lock (parent and child
    /// share the same `Arc<ToolRegistry>`) is asserted via `Arc::ptr_eq` in
    /// M4-05 wiring tests. `None` for top-level invocations that have no
    /// parent agent yet.
    pub subagent_registry: Option<Arc<ToolRegistry>>,
    /// Per-call cancellation token. Fired by the streaming executor when a
    /// sibling Bash tool errors (or the user interrupts). Phase 1 carries it;
    /// the Bash/subprocess tools observe it in Phase 2 to kill in-flight work.
    /// `None` for batched/legacy call sites.
    pub cancel: Option<tokio_util::sync::CancellationToken>,
    /// The parent agent's already-rendered system prompt bytes, threaded onto a
    /// fork-subagent spawn so the child replays the parent's exact prompt (TS
    /// `forkContextMessages` / `override.systemPrompt = forkParentSystemPrompt`).
    /// `None` for the non-fork path and for call sites the orchestrator has not
    /// yet wired (`AgentTool` then runs the fork child with `FORK_AGENT`'s empty
    /// system prompt — functional, not byte-identical).
    pub fork_parent_system_prompt: Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    /// Per-agent working directory OVERRIDE (claude-code's `agentWorktree` /
    /// `cwd`, threaded into the agent's `AsyncLocalStorage` cwd that every tool
    /// reads via `Mt()`). `Some` ONLY for a subagent isolated in a git worktree
    /// (`isolation:"worktree"`) or given an explicit `cwd` — the dispatch invoker
    /// populates it from [`crate::SubagentInvocationContext::cwd`]. Filesystem and
    /// shell tools resolve their base directory from this when present, else from
    /// the shared session [`crate::BuiltinToolContext::workspace`]. `None` for the
    /// main thread and every non-isolated call (byte-identical to before).
    pub cwd: Option<std::path::PathBuf>,
    /// This agent's recursion depth — claude's `agentContext.depth` (`z6`:
    /// `"main"` ⇒ 0, else this value). The `Agent` tool reads it to set a
    /// spawned child's depth (`child = depth + 1`), and the subagent
    /// tool-resolver applies `CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH` (default 1).
    /// `0` for the main thread and every non-subagent call; the dispatch invoker
    /// overwrites it from [`crate::SubagentInvocationContext::depth`] for a
    /// subagent's own tool calls.
    pub depth: u32,
    /// Effective observer inherited from a parent subagent. The Agent tool
    /// copies this to a child only when the child has no direct observer.
    pub observer: Option<lingxi_core::host::subagent_spawn::ObserverSpec>,
    /// The session's observer PAIRINGS (oracle `toolUseContext.session
    /// .observers`). Distinct from [`Self::observer`], which is the
    /// declaration propagated to children; this is the live observed↔observer
    /// table an `ObserverReport` resolves its destination from, and the table
    /// `SendMessage` consults to refuse an observer. `None` for hosts that
    /// have not wired observers — the tool then refuses with the oracle's
    /// "not armed" wording rather than pretending a pairing exists.
    pub observer_pairings: Option<Arc<lingxi_core::host::observer_pairing::ObserverPairings>>,
    /// (`/rewind`) Pre-edit file-history backup hook. The `Edit`/`Write`/
    /// `NotebookEdit` tools call `track_edit(path)` through this BEFORE writing,
    /// so `/rewind` can restore the pre-edit content. `None` (tests / no
    /// checkpointing) makes every write untracked (no behavior change).
    pub file_history: Option<Arc<dyn lingxi_core::host::FileHistorySink>>,
    // File state cache wired in Plan 10.
}

/// Typed, host-bound continuation request for a media conversation session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaAgentHandoffRequest {
    /// Host-minted identity of the successful session-start operation.
    pub operation_id: MediaOperationId,
    /// Opaque reference returned by the host media service.
    pub session: MediaSessionRef,
    /// Trusted mode selected for the media session. Host handoff admission
    /// checks this before allowing the session to continue the Agent turn.
    pub mode: media_api::MediaSessionMode,
}

/// Trusted model route information for deciding whether a media session can
/// continue through the current Agent's selected provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaAgentCapabilityContext {
    /// Current host-bound conversation session.
    pub session_id: String,
    /// Current model identifier.
    pub model: String,
    /// Exact profile pinned to this session, if present.
    pub profile_name: Option<String>,
    /// Account scope derived from the pinned profile.
    pub account_scope: Option<String>,
}

/// Narrow callback that asks the current turn owner to continue as an Agent
/// media conversation after the ordinary tool result has been committed.
///
/// Implementations must bind the request to trusted invocation context and
/// return promptly. They must not reserve a second turn or start an Agent run
/// reentrantly from inside a tool call.
pub trait MediaAgentHandoffHandle: Send + Sync {
    /// Structural modes that may be offered by this host attachment. The
    /// default supports audio-only; video modes require an explicit host
    /// implementation. Invocation-specific checks still run at dispatch.
    fn supported_modes(&self) -> Vec<media_api::MediaSessionMode> {
        vec![media_api::MediaSessionMode::Audio]
    }

    /// Modes advertised for this exact session and provider route. Hosts with
    /// connection-scoped provider bindings should resolve them by the supplied
    /// trusted context instead of exposing a process-wide capability union.
    fn supported_modes_for_context(
        &self,
        context: &MediaAgentCapabilityContext,
    ) -> Vec<media_api::MediaSessionMode> {
        self.supported_modes()
            .into_iter()
            .filter(|mode| self.supports_mode(context, *mode))
            .collect()
    }

    /// Whether this exact session profile can attach the selected media mode.
    fn supports_mode(
        &self,
        _context: &MediaAgentCapabilityContext,
        mode: media_api::MediaSessionMode,
    ) -> bool {
        self.supported_modes().contains(&mode)
    }

    /// Queue a deferred handoff for this trusted main-loop invocation.
    fn request(
        &self,
        invocation: &ToolUseContext,
        capability_context: &MediaAgentCapabilityContext,
        request: MediaAgentHandoffRequest,
    ) -> Result<(), String>;
}

impl ToolUseContext {
    /// Snapshot trusted media routing identity from the exact current
    /// conversation state. This awaits the session lock so normal lock
    /// contention does not masquerade as an unsupported provider.
    pub async fn media_agent_capability_context(&self) -> Option<MediaAgentCapabilityContext> {
        let session = self.session.as_ref()?.lock().await;
        // ToolUseOptions is captured by the owning request builder. Prefer its
        // resolved route profile so a trusted default route remains usable
        // even when the session did not persist an explicit profile pin.
        let profile_name = self
            .options
            .model_profile
            .clone()
            .or_else(|| session.model_profile.clone());
        Some(MediaAgentCapabilityContext {
            session_id: session.session_id.as_uuid().to_string(),
            model: session.model.clone(),
            account_scope: profile_name
                .as_ref()
                .map(|profile| format!("profile:{profile}")),
            profile_name,
        })
    }

    /// Build device-audio ownership only from trusted host context. The
    /// per-call agent/tool IDs remain attribution and never become the stable
    /// recording owner.
    pub async fn audio_operation_context(
        &self,
        identity: AudioOperationId,
        timeout_budget_ms: Option<u64>,
        max_payload_bytes: u64,
    ) -> Result<AudioOperationContext, AudioError> {
        let session_id = if let Some(origin_session_id) = self.origin_session_id.as_ref() {
            origin_session_id.to_string()
        } else if let Some(session) = self.session.as_ref() {
            session.lock().await.session_id.to_string()
        } else {
            return Err(AudioError::new(
                AudioErrorKind::InvalidRequest,
                "audio call has no trusted session owner",
            ));
        };

        let initiator = AudioInitiator {
            agent_id: self.agent_id.as_ref().map(ToString::to_string),
            tool_use_id: self.tool_use_id.as_ref().map(ToString::to_string),
            request_id: None,
        };

        Ok(AudioOperationContext {
            identity,
            owner: AudioOwner::Session { session_id },
            initiator: (initiator.agent_id.is_some() || initiator.tool_use_id.is_some())
                .then_some(initiator),
            timeout_budget_ms,
            max_payload_bytes,
        })
    }

    /// Build a minimal context that carries ONLY `main_loop_model`; every other
    /// field is inert (`None` / empty / default).
    ///
    /// The turn loop seeds this with the live `session.model`, folds a tool
    /// batch's [`crate::tool_trait::ContextModifier`]s over it, and reads back
    /// the resolved `options.main_loop_model` to apply a skill's `model:`
    /// override (SKILLEXEC.3, model scope). It is never handed to a tool, so the
    /// inert fields are never observed.
    #[must_use]
    pub fn model_seed(main_loop_model: String) -> Self {
        Self {
            options: ToolUseOptions {
                debug: false,
                verbose: false,
                main_loop_model,
                model_profile: None,
                max_budget_nano_usd: None,
                mcp_clients: Vec::new(),
                is_non_interactive_session: true,
                custom_system_prompt: None,
                append_system_prompt: None,
            },
            messages: Vec::new(),
            tool_use_id: None,
            assistant_message_id: None,
            available_tool_names: None,
            assistant_message: None,
            same_turn_tool_uses: Vec::new(),
            agent_id: None,
            nested_memory_triggers: Arc::default(),
            agent_name: None,
            team_name: None,
            origin_session_id: None,
            agent_spawn_provenance: Default::default(),
            native_query_depth: None,
            is_scheduled_turn: false,
            instruction_context: None,
            tool_execution_policy: ToolExecutionPolicy::Ordinary,
            trusted_effective_permission_mode: None,
            classifier_only_review: None,
            content_replacement_state: None,
            session: None,
            subagent_registry: None,
            cancel: None,
            fork_parent_system_prompt: None,
            cwd: None,
            // Inert seed; model-only context is never handed to a tool.
            depth: 0,
            observer: None,
            observer_pairings: None,
            file_history: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_use_context_carries_optional_cancel_token() {
        let mut ctx = ToolUseContext::model_seed("opus".into());
        assert!(ctx.cancel.is_none());
        ctx.cancel = Some(tokio_util::sync::CancellationToken::new());
        assert!(ctx.cancel.is_some());
    }
}

/// Per-call options that travel inside [`ToolUseContext`].
#[derive(Clone)]
pub struct ToolUseOptions {
    /// Emit debug-level traces from this call.
    pub debug: bool,
    /// Emit verbose informational traces from this call.
    pub verbose: bool,
    /// Identifier of the model driving the main agent loop.
    pub main_loop_model: String,
    /// Active provider profile for [`Self::main_loop_model`], when pinned
    /// (e.g. `github-copilot`). Tools that need provider-aware behavior should
    /// use this instead of unrelated host-specific request builders.
    pub model_profile: Option<String>,
    /// Optional cost budget for this call, in nano-USD.
    pub max_budget_nano_usd: Option<u64>,
    /// MCP server connections available for this call.
    pub mcp_clients: Vec<McpConnectionId>,
    /// Whether the agent is running in a non-interactive session.
    pub is_non_interactive_session: bool,
    /// Optional source-preserving system prompt override (replaces the default).
    pub custom_system_prompt: Option<Utf16JsonProjection>,
    /// Optional source-preserving system prompt addendum (appended to the default).
    pub append_system_prompt: Option<Utf16JsonProjection>,
}
