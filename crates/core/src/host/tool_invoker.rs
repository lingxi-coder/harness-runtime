//! `ToolInvoker` — narrow trait used by the M4-05 `AgentTool` to dispatch
//! sub-tool calls into a recursive subagent without taking a direct dep on
//! `lingxi-tools` (which would form a cycle).
//!
//! The recursion-lock invariant (a child subagent reuses the same
//! `Arc<ToolRegistry>` as its parent) is asserted in `lingxi-tools` tests via
//! `Arc::ptr_eq` on the trait-object passed through this surface.
//!
//! See M4-05 wiring follow-up plan.

use crate::types::{AgentId, MessageId, SessionId};
use async_trait::async_trait;
use serde_json::Value;
use std::any::Any;
use std::sync::Arc;
use thiserror::Error;

/// Resolve a Native `tool.call` middleware `ref` to a zero-based core-run
/// index. The wire reference is one-based; Native applies JavaScript's
/// `runs[ref - 1]`, including its `ToNumber` coercion, before the bounded array
/// lookup. `None` means that the value does not select one of the completed
/// runs (including `ref: 0`).
#[must_use]
pub fn tool_call_ref_index(reference: &Value, run_count: usize) -> Option<usize> {
    if run_count == 0 {
        return None;
    }
    let numeric_ref = js_number(reference)?;
    let index = numeric_ref - 1.0;
    if !index.is_finite() || index < 0.0 || index.fract() != 0.0 || index >= run_count as f64 {
        return None;
    }
    Some(index as usize)
}

fn js_number(value: &Value) -> Option<f64> {
    match value {
        Value::Null => Some(0.0),
        Value::Bool(value) => Some(if *value { 1.0 } else { 0.0 }),
        Value::Number(value) => value.as_f64(),
        Value::String(value) => js_number_from_string(value),
        Value::Array(values) => js_number_from_string(&js_array_to_string(values)?),
        Value::Object(_) => None,
    }
}

fn js_number_from_string(value: &str) -> Option<f64> {
    let value = value.trim_matches(is_ecmascript_whitespace);
    if value.is_empty() {
        return Some(0.0);
    }
    if let Some(digits) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        return js_radix_number(digits, 16);
    }
    if let Some(digits) = value
        .strip_prefix("0b")
        .or_else(|| value.strip_prefix("0B"))
    {
        return js_radix_number(digits, 2);
    }
    if let Some(digits) = value
        .strip_prefix("0o")
        .or_else(|| value.strip_prefix("0O"))
    {
        return js_radix_number(digits, 8);
    }
    value.parse::<f64>().ok()
}

fn js_radix_number(digits: &str, radix: u32) -> Option<f64> {
    if digits.is_empty() {
        return None;
    }
    let mut value = 0.0;
    for byte in digits.bytes() {
        let digit = match byte {
            b'0'..=b'9' => u32::from(byte - b'0'),
            b'a'..=b'f' => u32::from(byte - b'a') + 10,
            b'A'..=b'F' => u32::from(byte - b'A') + 10,
            _ => return None,
        };
        if digit >= radix {
            return None;
        }
        value = value * f64::from(radix) + f64::from(digit);
    }
    Some(value)
}

fn js_array_to_string(values: &[Value]) -> Option<String> {
    values
        .iter()
        .map(js_array_element_to_string)
        .collect::<Option<Vec<_>>>()
        .map(|values| values.join(","))
}

fn js_array_element_to_string(value: &Value) -> Option<String> {
    match value {
        // Array join renders null/undefined elements as the empty string.
        Value::Null => Some(String::new()),
        Value::Bool(value) => Some(value.to_string()),
        Value::Number(value) => value.as_f64().map(|value| value.to_string()),
        Value::String(value) => Some(value.clone()),
        Value::Array(values) => js_array_to_string(values),
        Value::Object(_) => Some("[object Object]".into()),
    }
}

fn is_ecmascript_whitespace(character: char) -> bool {
    // Rust includes U+0085 NEXT LINE in `char::is_whitespace`, but ECMAScript
    // StringToNumber trimming does not. U+FEFF is ECMAScript whitespace even
    // though Rust does not classify it that way.
    character.is_whitespace() && character != '\u{0085}' || character == '\u{FEFF}'
}

/// Trusted host-selected execution policy for one nested tool invocation.
///
/// This is deliberately not serialized and is never derived from model input.
/// The agent runtime selects it from the resolved agent definition before it
/// constructs [`SubagentInvocationContext`]. A receiver may narrow behavior
/// for a trusted policy, but must never widen an ordinary invocation based on
/// untrusted JSON, a tool name, or telemetry labels.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolExecutionPolicy {
    /// Normal Agent/subagent behavior, including WebFetch's existing apply
    /// side-query when the composition root provides one.
    #[default]
    Ordinary,
    /// Hidden Fusion panel behavior. WebFetch may retrieve and convert locally,
    /// but must not make an internal model/side-query call.
    FusionPanel,
}

/// Host-owned conversation state carried only for nested explicit forks.
#[derive(Debug, Clone)]
pub struct SubagentForkContext {
    /// The dispatching agent's actual conversation, including lazy policies.
    pub messages: Vec<crate::types::ConversationMessage>,
    /// The exact rendered parent system prompt, used by explicit forks.
    pub system_prompt: Option<String>,
}

/// Per-call invocation context handed to a [`ToolInvoker`].
///
/// Carries the identity of the parent agent (used by the recursion-lock test
/// to verify the child inherits the parent's registry / budget) plus any
/// metadata the trait abstraction needs to expose without leaking the
/// concrete `ToolUseContext` from `lingxi-tools`.
#[derive(Debug, Clone)]
pub struct SubagentInvocationContext {
    /// Exact input supplied for this physical call, independent of retained
    /// context snapshots. A receiver must reject a mismatching display value.
    pub input_projection: Option<crate::types::utf16_json::Utf16JsonProjection>,
    /// Cancellation scope for this physical tool call. A streamed Agent tool
    /// use owns a distinct token; receivers must copy it into the concrete
    /// `ToolUseContext` rather than reusing a previous call's token.
    pub cancellation_token: tokio_util::sync::CancellationToken,
    pub permission_pause_observer: Option<crate::host::permission_gate::PermissionPauseObserver>,
    /// Parent agent id (the agent that is dispatching the child).
    pub parent_agent_id: Option<AgentId>,
    /// Trusted originating session for nested Agent/Fusion budget scoping.
    /// Receivers must propagate this value explicitly and must not infer it
    /// from hook/session metadata.
    pub origin_session_id: Option<SessionId>,
    /// Trusted host-selected execution policy. This is copied unchanged into
    /// the concrete tool-use context path, including the
    /// workspace-lease entrypoint; it is not model-controlled.
    pub tool_execution_policy: ToolExecutionPolicy,
    /// Inherited instruction context and lazy discovery cursor.
    pub instruction_context: Option<crate::host::instructions::InstructionContext>,
    /// Actual parent history needed by the Agent tool's explicit fork path.
    /// Other tools can omit this to avoid copying their agent's transcript.
    pub fork_context: Option<SubagentForkContext>,
    /// DISPLAY NAME of the teammate dispatching this tool call, if known
    /// (claude-code `getAgentName()` — the teammate's human name, e.g.
    /// `"researcher"`, NOT the `agent:<uuid>` form). `None` for the main
    /// thread / leader. Mapped straight into
    /// `ToolUseContext.agent_name` so the swarm-only `TaskUpdate` side-effects
    /// (auto-owner, owner-change mailbox notification) key on the name.
    pub agent_name: Option<String>,
    /// TEAM NAME the dispatching teammate belongs to, if known (claude-code
    /// `getTeammateContext()?.teamName`). Mapped into `ToolUseContext.team_name`
    /// so `getTaskListId()` resolves an in-process teammate to the leader's
    /// on-disk task directory. `None` for the main thread / standalone session.
    pub team_name: Option<String>,
    /// Whether the dispatching subagent runs ASYNC (backgrounded). claude-code's
    /// `runAgent` sets the child tools' `isNonInteractiveSession: true` for an
    /// async agent (else it inherits the parent's flag, default `false`) —
    /// `runAgent.ts:668-672`. Mapped into `ToolUseContext.is_non_interactive_session`.
    pub is_async: bool,
    /// Effective owning-session mode for this dispatch. This is distinct from
    /// `is_async`: a synchronous child of scheduled/headless work must still
    /// keep every invoked tool non-interactive.
    pub is_non_interactive_session: bool,
    /// Whether the dispatching subagent may SURFACE permission prompts to the
    /// user (claude-code's permission-prompt eligibility). Threaded from
    /// `SubagentContext.can_show_permission_prompts`. When `true` (a named
    /// in-process teammate), a tool call that needs permission surfaces in the
    /// main session ATTRIBUTED to this worker (the `● @name` badge); when `false`
    /// the worker is not presented as a permission-prompt origin. Consulted by
    /// the dispatch invoker to decide whether to attach a
    /// [`crate::host::permission_gate::PromptWorker`] to the gate check.
    pub can_show_permission_prompts: bool,
    /// Per-agent working directory OVERRIDE — `Some` when the dispatching
    /// subagent is isolated in a git worktree (`isolation:"worktree"`) or was
    /// given an explicit `cwd`. The invoker maps it into `ToolUseContext.cwd` so
    /// the agent's filesystem + shell tools operate in that directory instead of
    /// the shared session workspace (claude-code's per-agent `agentWorktree`/cwd
    /// `AsyncLocalStorage`). `None` for the main thread / a non-isolated agent.
    pub cwd: Option<std::path::PathBuf>,
    /// The assistant message's `tool_use` block id this dispatch is for — the
    /// REAL id a stdio `can_use_tool` request should carry (claude-code
    /// `createCanUseTool(toolUseID)`), so the host can correlate + dedup the
    /// subagent's permission prompt. 1:1 with
    /// [`crate::host::permission_gate::PermissionCheckContext::tool_use_id`]: the
    /// dispatch invoker maps it straight into the gate check context so the
    /// subagent path is byte-faithful to the main loop's. `None` for a dispatch
    /// site that has no originating block id (test fixtures / legacy callers),
    /// in which case the gate mints a fresh id exactly as before.
    pub tool_use_id: Option<String>,
    /// Stable id of the assistant message that contains this tool-use block.
    /// This is the real model-authored message id (not the provider request id
    /// and never a synthesized replacement) and is forwarded to
    /// `ToolUseContext` for per-call telemetry correlation. `None` is reserved
    /// for callers that do not originate from an assistant message.
    pub assistant_message_id: Option<MessageId>,
    /// The DISPATCHING agent's recursion depth (claude `agentContext.depth`).
    /// The dispatch invoker maps it into `ToolUseContext.depth`, so a recursive
    /// `Agent` call inside the dispatched tool computes the child's depth and the
    /// subagent tool-resolver can apply the configured spawn-depth cap. `0` for the main
    /// thread / a top-level dispatch (and every legacy/test call site).
    pub depth: u32,
    /// Observer declaration inherited from the dispatching agent. Recursive
    /// Agent calls use it only when the selected child has no declaration of
    /// its own; `observe_subagents:false` and the depth cap stop propagation.
    pub observer: Option<crate::host::subagent_spawn::ObserverSpec>,
    /// The DISPATCHING subagent's OWN resolved main-loop model (claude-code
    /// `runAgent.ts:678` seeds each child's `mainLoopModel: resolvedAgentModel`,
    /// so a NESTED `Agent` call inside a subagent resolves its child's model
    /// against the IMMEDIATE parent's resolved model, not the top-level main-loop
    /// model). The dispatch invoker maps it into `ToolUseContext.options.main_loop_model`
    /// so a recursive `Agent` tool call reads the parent's model (claude
    /// `AgentTool.tsx:418` `toolUseContext.options.mainLoopModel`). `None` for the
    /// main thread / legacy call sites (⇒ the invoker keeps its placeholder model).
    pub parent_model: Option<String>,
    /// Provider profile paired with [`Self::parent_model`]. Nested Agent calls
    /// must inherit both values; carrying only the wire id is ambiguous when
    /// multiple configured providers expose the same model.
    pub parent_model_profile: Option<String>,
    /// Trusted plugin caller/origin captured when this agent was spawned.
    /// Receivers preserve the exact missing/null/value distinctions and must
    /// never infer provenance from model input or hook output.
    pub agent_spawn_provenance: crate::host::subagent_spawn::AgentSpawnProvenance,
    /// The actual per-agent ToolUseContext state after earlier nested tool
    /// effects. Core keeps this host-owned type opaque; tool-api is the only
    /// crate that downcasts it to its concrete ToolUseContext container.
    /// Each agent loop owns its own state and supplies it to later calls.
    pub tool_context_state: Option<ToolInvocationContextState>,
    /// Current agent history at this dispatch. This is refreshed on every
    /// call, so a persisted ToolUseContext snapshot can never replay stale
    /// messages after a tool result or injected message has been appended.
    pub current_history: Vec<crate::types::ConversationMessage>,
    /// The accepted assistant row for the completed block being handled.
    /// This remains separate from `current_history`, which is the pre-query
    /// snapshot and must not acquire the live assistant row by concatenation.
    pub assistant_message: Option<crate::types::ConversationMessage>,
    /// Tool-use blocks from earlier sibling rows in this same query. The
    /// current row is excluded; callers refresh this per physical invocation.
    pub same_turn_tool_uses: Vec<crate::types::ContentBlock>,
    /// The dispatching subagent's EFFECTIVE permission mode as a WIRE string
    /// (claude-code 2.1.207 Agent `mode` → the child's
    /// `toolPermissionContext.mode`, `wKe`/`ve`). `Some("plan")` ⇒ the dispatch
    /// permission gate authorizes THIS call under that mode (a `mode:"plan"` child
    /// gates mutations — `Edit`/`Write`/`Bash` — while reads stay frictionless);
    /// `None` ⇒ the gate uses its live/boot mode (the main thread / a spawn with no
    /// mode override — byte-identical to before). Mapped straight into
    /// [`crate::host::permission_gate::PermissionCheckContext::mode_override`].
    pub mode_override: Option<String>,
    /// Trusted source metadata for source-restricted permission prompt actions.
    /// `None` is fail-closed and must not be inferred by the receiver.
    pub request_source: Option<crate::host::permission_gate::PermissionRequestSource>,
    /// Command-deny rules FROZEN when a background fork launched, replayed for
    /// every tool call this subagent makes (claude `freezeCommandDenies`).
    ///
    /// Upstream rebuilds the permission context from LIVE app state on resume,
    /// so a settings edit made while a fork was parked could REMOVE a deny that
    /// was in force when it launched. These rules are re-applied as a
    /// `disallowed_tools` permission LAYER, which the fold applies ON TOP of the
    /// base policy — so a frozen deny WINS over a live rule that would now allow
    /// the same command.
    ///
    /// The port is exposed in BOTH directions, so this is not a cross-session-only
    /// concern: `PolicyPermissionGate::apply_permission_update` mutates the gate's
    /// `live_state` deny rules in-process (`addRules` / `replaceRules` /
    /// `removeRules`), so a host permission update can remove an in-force deny
    /// while a fork is parked in the SAME process. The scoping record additionally
    /// outlives the process, and a cross-session resume reads it against a freshly
    /// loaded policy.
    ///
    /// Note the converse gap, which this field does not close: the snapshot is
    /// taken from the BOOT policy (`PermissionPolicy::deny_rules`), not from the
    /// gate's `live_state`, so a deny ADDED at runtime is never frozen.
    ///
    /// Empty ⇒ no layer is added and the fold is byte-identical to before.
    pub frozen_command_denies: Vec<String>,
}

/// Opaque, host-owned context state carried through the provider-neutral Core
/// boundary. The concrete runtime type is selected by the caller and checked
/// with [`Self::downcast_arc`]; Core never imports tool-api.
#[derive(Clone)]
pub struct ToolInvocationContextState {
    inner: Arc<dyn Any + Send + Sync>,
}

impl ToolInvocationContextState {
    /// Wrap a concrete host context without exposing its type to Core.
    #[must_use]
    pub fn new<T: Any + Send + Sync>(value: Arc<T>) -> Self {
        Self { inner: value }
    }

    /// Borrow the contained host context by cloning its Arc. A failed type
    /// check leaves this state intact and available for a correct downcast.
    pub fn downcast_arc<T: Any + Send + Sync>(
        &self,
    ) -> Result<Arc<T>, ToolInvocationContextStateError> {
        Arc::clone(&self.inner)
            .downcast::<T>()
            .map_err(|_| ToolInvocationContextStateError::TypeMismatch)
    }
}

impl std::fmt::Debug for ToolInvocationContextState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolInvocationContextState")
            .finish_non_exhaustive()
    }
}

/// A context-state downcast failed without consuming or mutating the state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ToolInvocationContextStateError {
    /// The erased state was not the requested concrete host context type.
    #[error("tool invocation context state has a different concrete type")]
    TypeMismatch,
}

trait ErasedToolInvocationContextModifier: Send + Sync {
    fn apply_boxed(
        &self,
        context: Box<dyn Any + Send>,
    ) -> Result<Box<dyn Any + Send>, ToolInvocationContextModifierError>;
}

struct OnceToolInvocationContextModifier<T, F> {
    modifier: std::sync::Mutex<Option<F>>,
    marker: std::marker::PhantomData<fn(T)>,
}

impl<T, F> ErasedToolInvocationContextModifier for OnceToolInvocationContextModifier<T, F>
where
    T: Any + Send + 'static,
    F: FnOnce(T) -> T + Send + 'static,
{
    fn apply_boxed(
        &self,
        context: Box<dyn Any + Send>,
    ) -> Result<Box<dyn Any + Send>, ToolInvocationContextModifierError> {
        let context = context
            .downcast::<T>()
            .map_err(|_| ToolInvocationContextModifierError::TypeMismatch)?;
        let modifier = self
            .modifier
            .lock()
            .map_err(|_| ToolInvocationContextModifierError::Poisoned)?
            .take()
            .ok_or(ToolInvocationContextModifierError::AlreadyApplied)?;
        Ok(Box::new(modifier(*context)))
    }
}

/// A one-shot host callback that mutates a concrete context without making
/// Core depend on tool-api's `ToolUseContext` type. Clones share the same
/// `FnOnce`: only the first successful apply can consume it. The closure is
/// required to be `Send`, not `Sync`.
#[derive(Clone)]
pub struct ToolInvocationContextModifier {
    inner: Arc<dyn ErasedToolInvocationContextModifier>,
}

impl ToolInvocationContextModifier {
    /// Store one typed context mutation behind Core's erased host boundary.
    #[must_use]
    pub fn new<T, F>(modifier: F) -> Self
    where
        T: Any + Send + 'static,
        F: FnOnce(T) -> T + Send + 'static,
    {
        Self {
            inner: Arc::new(OnceToolInvocationContextModifier::<T, F> {
                modifier: std::sync::Mutex::new(Some(modifier)),
                marker: std::marker::PhantomData,
            }),
        }
    }

    /// Apply this one-shot callback to a context of its declared type.
    /// A type mismatch does not consume the callback, so the caller can retry
    /// with the correct concrete context.
    pub fn apply<T: Any + Send + 'static>(
        &self,
        context: T,
    ) -> Result<T, ToolInvocationContextModifierError> {
        self.inner
            .apply_boxed(Box::new(context))?
            .downcast::<T>()
            .map(|context| *context)
            .map_err(|_| ToolInvocationContextModifierError::TypeMismatch)
    }
}

impl std::fmt::Debug for ToolInvocationContextModifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolInvocationContextModifier")
            .finish_non_exhaustive()
    }
}

/// Failure to apply a host-owned nested context modifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ToolInvocationContextModifierError {
    /// The supplied context type did not match the modifier's concrete type.
    #[error("tool invocation context modifier received a different concrete type")]
    TypeMismatch,
    /// The shared one-shot callback has already been applied.
    #[error("tool invocation context modifier was already applied")]
    AlreadyApplied,
    /// The callback mutex was poisoned by a prior panic.
    #[error("tool invocation context modifier lock was poisoned")]
    Poisoned,
}

/// Failure modes for [`ToolInvoker::invoke`].
#[derive(Debug, Error)]
pub enum ToolInvokerError {
    /// The named tool was not found in the registry.
    #[error("ToolInvoker: tool '{0}' not found")]
    NotFound(String),
    /// The tool surfaced an invalid input.
    #[error("ToolInvoker: invalid input: {0}")]
    InvalidInput(String),
    /// A schema or tool precondition rejected input before execution.
    #[error("ToolInvoker: input validation failed: {0}")]
    Validation(String),
    /// Permission policy terminated the owning prompt-avoiding agent.
    ///
    /// Unlike an ordinary denial, callers must not convert this into a
    /// recoverable `tool_result` and continue the model loop.
    #[error("{0}")]
    Abort(String),
    /// Any other internal failure.
    #[error("ToolInvoker: internal error: {0}")]
    Internal(String),
}

impl ToolInvokerError {
    /// The bare model-facing message — claude's `formatError(error)` =
    /// `error.message`, WITHOUT the LingXi-internal `ToolInvoker: …` `Display`
    /// prefix. Used as the subagent tool_result content so the child model
    /// never sees an `invalid input: `/`internal error: ` variant prefix
    /// (which is `Display`-only, for logging). Mirrors
    /// [`tool_api::ToolError::model_facing_message`]. `NotFound` carries only
    /// the tool name, so it is rendered into a full message here.
    #[must_use]
    pub fn model_facing_message(&self) -> String {
        match self {
            Self::NotFound(name) => format!("tool '{name}' not found"),
            Self::InvalidInput(s) | Self::Validation(s) | Self::Abort(s) | Self::Internal(s) => {
                s.clone()
            }
        }
    }
    /// Native result wrappers differ for pre-execution validation and thrown errors.
    #[must_use]
    pub fn model_tool_result_content(&self) -> String {
        match self {
            Self::Validation(message) => format!("<tool_use_error>{message}</tool_use_error>"),
            _ => format!("Error: {}", self.model_facing_message()),
        }
    }
}

/// Why a successful tool result ended the current turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolResultTurnEndSource {
    /// The native tool result returned `endsTurn: true`.
    Tool,
    /// The tool result's MCP `_meta` block requested turn termination.
    McpMeta,
}

impl ToolResultTurnEndSource {
    /// Analytics wire value used by `tengu_mcp_tool_result_ended_turn`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::McpMeta => "mcp_meta",
        }
    }
}

/// Trusted successful-result control, separate from model-facing JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolResultTurnEnd {
    /// Marker that won the oracle's `tool`-before-`mcp_meta` precedence.
    pub source: ToolResultTurnEndSource,
}

/// A tool's structured result plus its independent model-facing text.
#[derive(Debug, Clone)]
pub struct ToolInvocationResult {
    /// Tool-reported failure, distinct from an invocation/transport failure.
    pub is_error: bool,
    /// Structured result retained for tool consumers and media handling.
    pub data: Value,
    /// Optional prose supplied by the tool's model-facing result mapper.
    pub model_content: Option<String>,
    /// Extra conversation messages returned by the underlying tool.
    pub new_messages: Vec<crate::types::ConversationMessage>,
    /// One-shot mutation of the concrete per-agent ToolUseContext, kept
    /// separate from model-only reminders in [`Self::context`].
    pub context_modifier: Option<ToolInvocationContextModifier>,
    /// Opaque tool metadata retained for host-side result handling.
    pub mcp_meta: Option<Value>,
    /// Trusted host-computed control. Model input or result JSON cannot set it.
    pub turn_end: Option<ToolResultTurnEnd>,
    /// Model-only reminders attached by `tool.call` after the tool result.
    /// Exact JavaScript UTF-16 units remain paired with their display strings
    /// until the owning Agent loop projects them into history and attachments.
    pub context: crate::types::utf16_json::Utf16JsonProjection,
    /// Actual ToolUseContext used for this call, carried forward by the owning
    /// agent loop so future invocations can preserve applied mutations.
    pub context_state: Option<ToolInvocationContextState>,
}

/// Tool invocation seam used by `AgentTool` to recurse into the registry.
///
/// Concrete impls live in `lingxi-tools` (production wrapper around
/// `ToolRegistry`) and in test fixtures (recording mock).
#[async_trait]
pub trait ToolInvoker: Send + Sync + Any {
    /// Release this Agent's computer inputs after its loop has settled.
    async fn cleanup_computer_inputs(
        &self,
        _agent_id: AgentId,
        _origin_session_id: Option<SessionId>,
    ) -> Result<(), ToolInvokerError> {
        Ok(())
    }

    /// Trusted current enforcing mode. Wrappers preserve their actual local
    /// policy override, or forward the inner bound gate's live value.
    fn permission_mode(&self) -> Option<String> {
        None
    }

    /// Re-render a Mod replacement with the named tool's model-text mapper.
    fn map_result_text(&self, _name: &str, _result: &Value) -> Option<String> {
        None
    }

    /// Tool mapper's logical error flag for a replacement result, if defined.
    fn map_result_is_error(&self, _name: &str, _result: &Value) -> Option<bool> {
        None
    }

    /// Validate a replacement result without executing the tool body.
    fn validate_output(&self, _name: &str, _output: &Value) -> Result<(), String> {
        Ok(())
    }

    /// Resolve concurrency safety from the actual registered tool selected for
    /// this call. `None` means this invoker cannot resolve the tool; callers
    /// must not infer safety from a tool name or model-facing JSON.
    fn tool_is_concurrency_safe(&self, _name: &str, _input: &Value) -> Option<bool> {
        None
    }

    /// Invoke the tool named `name` with the supplied JSON `input`.
    async fn invoke(
        &self,
        name: &str,
        input: Value,
        ctx: SubagentInvocationContext,
    ) -> Result<Value, ToolInvokerError>;

    /// Invoke with an ephemeral workspace lease. Implementations that do not
    /// participate in lease-aware permission enforcement retain the legacy
    /// behavior by delegating to [`Self::invoke`].
    async fn invoke_with_workspace_lease(
        &self,
        name: &str,
        input: Value,
        ctx: SubagentInvocationContext,
        _workspace_lease_token: Option<u64>,
    ) -> Result<Value, ToolInvokerError> {
        self.invoke(name, input, ctx).await
    }

    /// Preserve model-facing text without replacing the structured result.
    async fn invoke_detailed(
        &self,
        name: &str,
        input: Value,
        ctx: SubagentInvocationContext,
        workspace_lease_token: Option<u64>,
    ) -> Result<ToolInvocationResult, ToolInvokerError> {
        self.invoke_with_workspace_lease(name, input, ctx, workspace_lease_token)
            .await
            .map(|data| ToolInvocationResult {
                is_error: false,
                data,
                model_content: None,
                new_messages: Vec::new(),
                context_modifier: None,
                mcp_meta: None,
                turn_end: None,
                context: crate::types::utf16_json::Utf16JsonProjection::plain(
                    serde_json::Value::Array(Vec::new()),
                ),
                context_state: None,
            })
    }

    /// Invoke one host-supplied child tool through the same enforcing dispatch
    /// path. The opaque carrier is minted by the runtime, never model JSON.
    /// Wrappers must forward it unchanged; unsupported hosts fail closed.
    async fn invoke_supplied_detailed(
        &self,
        name: &str,
        _input: Value,
        _ctx: SubagentInvocationContext,
        _workspace_lease_token: Option<u64>,
        _supplied: Arc<dyn Any + Send + Sync>,
    ) -> Result<ToolInvocationResult, ToolInvokerError> {
        Err(ToolInvokerError::Internal(format!(
            "Host-supplied tool {name} dispatch is unavailable"
        )))
    }

    /// Cast to `&dyn Any` for downcast-based test introspection.
    /// Default impl works for all `Sized + 'static` implementors.
    fn as_any(&self) -> &dyn Any;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::Arc;

    #[test]
    fn trait_is_object_safe() {
        let _: Option<Arc<dyn ToolInvoker>> = None;
    }

    #[test]
    fn native_tool_call_ref_uses_one_based_js_number_coercion() {
        for (reference, count, expected) in [
            (serde_json::json!(1), 2, Some(0)),
            (serde_json::json!(2.0), 2, Some(1)),
            (serde_json::json!("1"), 2, Some(0)),
            (serde_json::json!(" 2 "), 2, Some(1)),
            (serde_json::json!(true), 2, Some(0)),
            (serde_json::json!([1]), 2, Some(0)),
            (serde_json::json!([2]), 2, Some(1)),
            (serde_json::json!([[1]]), 2, Some(0)),
            (serde_json::json!("0x2"), 2, Some(1)),
            (serde_json::json!("0b10"), 2, Some(1)),
            (serde_json::json!("0o2"), 2, Some(1)),
            (serde_json::json!("\u{feff}1"), 2, Some(0)),
            (serde_json::json!(false), 2, None),
            (serde_json::json!(null), 2, None),
            (serde_json::json!([]), 2, None),
            (serde_json::json!([null]), 2, None),
            (serde_json::json!({}), 2, None),
            (serde_json::json!(0), 2, None),
            (serde_json::json!(3), 2, None),
            (serde_json::json!(1.5), 2, None),
            (serde_json::json!("1.5"), 2, None),
            (serde_json::json!("bad"), 2, None),
            (serde_json::json!("NaN"), 2, None),
            (serde_json::json!("Infinity"), 2, None),
            (serde_json::json!("\u{85}1"), 2, None),
            (serde_json::json!(1), 0, None),
        ] {
            assert_eq!(
                tool_call_ref_index(&reference, count),
                expected,
                "reference={reference}, run_count={count}"
            );
        }
    }

    #[test]
    fn native_tool_call_ref_numbers_round_through_ieee754_before_bounds_check() {
        let large_integer = serde_json::json!(9_007_199_254_740_993_u64);
        assert_eq!(js_number(&large_integer), Some(9_007_199_254_740_992.0));
        assert_eq!(tool_call_ref_index(&large_integer, 2), None);
    }

    #[test]
    fn context_state_downcast_failure_keeps_the_snapshot_available() {
        let state = ToolInvocationContextState::new(Arc::new(42_u32));

        assert!(matches!(
            state.downcast_arc::<String>(),
            Err(ToolInvocationContextStateError::TypeMismatch)
        ));
        assert_eq!(
            *state.downcast_arc::<u32>().expect("original type remains"),
            42
        );
    }

    #[test]
    fn context_modifier_accepts_send_not_sync_fn_once_and_applies_once_across_clones() {
        // Cell is Send but not Sync. The adapter must not strengthen the
        // ToolCallResult FnOnce contract to require Sync.
        let counter = Cell::new(7_u32);
        let modifier = ToolInvocationContextModifier::new(move |mut context: Vec<u32>| {
            context.push(counter.get());
            counter.set(counter.get() + 1);
            context
        });
        let cloned = modifier.clone();

        assert_eq!(
            modifier.apply(Vec::<u32>::new()).expect("first apply"),
            vec![7_u32]
        );
        assert!(matches!(
            cloned.apply(Vec::<u32>::new()),
            Err(ToolInvocationContextModifierError::AlreadyApplied)
        ));
    }

    #[test]
    fn context_modifier_type_error_does_not_consume_the_fn_once() {
        let modifier = ToolInvocationContextModifier::new(|value: Vec<u8>| value);

        assert!(matches!(
            modifier.apply(String::from("wrong type")),
            Err(ToolInvocationContextModifierError::TypeMismatch)
        ));
        assert_eq!(
            modifier.apply(vec![1_u8, 2]).expect("correct type"),
            vec![1_u8, 2]
        );
    }
}
