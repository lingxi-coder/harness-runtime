//! `ToolInvoker` — narrow trait used by the M4-05 `AgentTool` to dispatch
//! sub-tool calls into a recursive subagent without taking a direct dep on
//! `lingxi-tools` (which would form a cycle).
//!
//! The recursion-lock invariant (a child subagent reuses the same
//! `Arc<ToolRegistry>` as its parent) is asserted in `lingxi-tools` tests via
//! `Arc::ptr_eq` on the trait-object passed through this surface.
//!
//! See M4-05 wiring follow-up plan.

use crate::types::utf16_json::{JsonValueView, Utf16JsonProjection};
use crate::types::{AgentId, MessageId, SessionId};
use async_trait::async_trait;
use std::any::Any;
use std::sync::Arc;
use thiserror::Error;

/// Resolve a Native `tool.call` middleware `ref` to a zero-based core-run
/// index. The wire reference is one-based; Native applies JavaScript's
/// `runs[ref - 1]`, including its `ToNumber` coercion, before the bounded array
/// lookup. `None` means that the value does not select one of the completed
/// runs (including `ref: 0`).
#[must_use]
pub fn tool_call_ref_index(reference: JsonValueView<'_>, run_count: usize) -> Option<usize> {
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

fn js_number(value: JsonValueView<'_>) -> Option<f64> {
    if let Some(value) = value.as_number() {
        return Some(value);
    }
    if value.is_null() {
        return Some(0.0);
    }
    if let Some(value) = value.as_bool() {
        return Some(if value { 1.0 } else { 0.0 });
    }
    if let Some(value) = value.code_units() {
        return js_number_from_string(&String::from_utf16(value).ok()?);
    }
    if value.array_len().is_some() {
        return js_number_from_string(&js_array_to_string(value)?);
    }
    None
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

fn js_array_to_string(value: JsonValueView<'_>) -> Option<String> {
    enum Task<'a> {
        Value(JsonValueView<'a>),
        Comma,
    }

    let count = value.array_len()?;
    let mut pending = Vec::with_capacity(count.saturating_mul(2));
    for index in (0..count).rev() {
        pending.push(Task::Value(value.get_index(index)?));
        if index > 0 {
            pending.push(Task::Comma);
        }
    }
    let mut result = String::new();
    while let Some(task) = pending.pop() {
        match task {
            Task::Comma => result.push(','),
            Task::Value(value) => {
                if value.is_null() {
                    // Array join renders null/undefined elements as empty text.
                } else if let Some(text) = value.as_bool() {
                    result.push_str(if text { "true" } else { "false" });
                } else if let Some(number) = value.as_number() {
                    result.push_str(&js_number_to_string(number));
                } else if let Some(units) = value.code_units() {
                    result.push_str(&String::from_utf16(units).ok()?);
                } else if let Some(count) = value.array_len() {
                    for index in (0..count).rev() {
                        pending.push(Task::Value(value.get_index(index)?));
                        if index > 0 {
                            pending.push(Task::Comma);
                        }
                    }
                } else if value.object_entries().is_some() {
                    result.push_str("[object Object]");
                } else {
                    return None;
                }
            }
        }
    }
    Some(result)
}

fn js_number_to_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value == f64::INFINITY {
        return "Infinity".into();
    }
    if value == f64::NEG_INFINITY {
        return "-Infinity".into();
    }
    if value == 0.0 {
        return "0".into();
    }
    ryu_js::Buffer::new().format_finite(value).to_owned()
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
    pub system_prompt: Option<crate::types::utf16_json::Utf16JsonProjection>,
}

/// Per-call invocation context handed to a [`ToolInvoker`].
///
/// Carries the identity of the parent agent (used by the recursion-lock test
/// to verify the child inherits the parent's registry / budget) plus any
/// metadata the trait abstraction needs to expose without leaking the
/// concrete `ToolUseContext` from `lingxi-tools`.
#[derive(Debug, Clone)]
pub struct SubagentInvocationContext {
    /// Cancellation scope for this physical tool call. A streamed Agent tool
    /// use owns a distinct token; receivers must copy it into the concrete
    /// `ToolUseContext` rather than reusing a previous call's token.
    pub cancellation_token: tokio_util::sync::CancellationToken,
    pub permission_pause_observer: Option<crate::host::permission_gate::PermissionPauseObserver>,
    /// Parent agent id (the agent that is dispatching the child).
    pub parent_agent_id: Option<AgentId>,
    /// Current Native query depth of the caller when known. This is not the
    /// separate Agent spawn-nesting depth and remains None for non-query calls.
    pub native_query_depth: Option<u32>,
    /// Current caller's effective tool names for Native bundled-skill gates.
    /// Canonical names and aliases are captured together; `None` and `Some([])`
    /// retain their distinct Native meanings.
    pub available_tool_names: Option<Vec<Utf16JsonProjection>>,
    /// Trusted originating session for nested Agent/Fusion budget scoping.
    /// Receivers must propagate this value explicitly and must not infer it
    /// from hook/session metadata.
    pub origin_session_id: Option<SessionId>,
    /// Runtime-only origin generation inherited from the launching session.
    /// This accompanies the immutable identity across nested tool calls.
    pub session_origin: Option<Arc<dyn crate::host::subagent_spawn::SessionOriginCapability>>,
    /// Trusted host-selected execution policy. This is copied unchanged into
    /// the concrete tool-use context; it is not model-controlled.
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
    /// Empty ⇒ the gate receives no frozen command-deny prefix.
    pub frozen_command_denies: Vec<Utf16JsonProjection>,
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

/// Whether the process tree was proven to have stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProcessTreeStatus {
    /// The producer has an authoritative receipt that the tree was reaped.
    VerifiedReaped,
    /// Cleanup was requested or attempted without proof that the tree exited.
    Unverified,
}

/// Evidence captured when a process result is incomplete.
///
/// EOF and truncation describe independent facts: reaching EOF does not mean
/// every byte was retained, and an untruncated prefix does not prove EOF.
/// `exit_code` is `None` when the real exit status is unknown, independently
/// of whether the process tree has been reaped.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UnverifiedCleanupOutputParts {
    /// Native raw-stdio session identity, when one was assigned.
    pub session_id: Option<String>,
    /// Captured stdout. Invalid UTF-8 may already have been replaced by the producer.
    pub stdout: String,
    /// Captured stderr. Invalid UTF-8 may already have been replaced by the producer.
    pub stderr: String,
    /// The stdout prefix does not include all observed output.
    pub stdout_truncated: bool,
    /// The stderr prefix does not include all observed output.
    pub stderr_truncated: bool,
    /// The producer observed EOF for stdout.
    pub stdout_eof: bool,
    /// The producer observed EOF for stderr.
    pub stderr_eof: bool,
    /// The observed exit code; absent rather than fabricated if unknown.
    pub exit_code: Option<i32>,
    /// The host's execution deadline elapsed.
    pub timed_out: bool,
    /// Whether termination was requested; `None` means the fact is unknown.
    /// A request does not itself prove that any process stopped.
    pub termination_requested: Option<bool>,
    /// Whether the producer proved that the process tree was reaped.
    pub tree_status: ProcessTreeStatus,
    /// Diagnostic detail about the incomplete result or cleanup status.
    pub cleanup_detail: String,
    /// The original read or stop failure, if one occurred.
    pub original_error: Option<String>,
}

impl std::fmt::Debug for UnverifiedCleanupOutputParts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnverifiedCleanupOutputParts")
            .field("has_session_id", &self.session_id.is_some())
            .field("stdout_bytes", &self.stdout.len())
            .field("stderr_bytes", &self.stderr.len())
            .field("stdout_truncated", &self.stdout_truncated)
            .field("stderr_truncated", &self.stderr_truncated)
            .field("stdout_eof", &self.stdout_eof)
            .field("stderr_eof", &self.stderr_eof)
            .field("exit_code", &self.exit_code)
            .field("timed_out", &self.timed_out)
            .field("termination_requested", &self.termination_requested)
            .field("tree_status", &self.tree_status)
            .finish_non_exhaustive()
    }
}

/// Maximum retained Unicode scalar values per captured stream.
pub const MAX_UNVERIFIED_OUTPUT_CHARS: usize = 30_000;

/// Maximum retained Unicode scalar values for each process diagnostic.
pub const MAX_UNVERIFIED_METADATA_CHARS: usize = 4_096;

/// Whether diagnostic metadata was truncated and a digest of its full input.
#[derive(Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UnverifiedCleanupOutputMetadata {
    /// The cleanup detail was shortened before being retained.
    pub cleanup_detail_truncated: bool,
    /// SHA-256 of the original cleanup detail when it was shortened.
    pub cleanup_detail_sha256: Option<String>,
    /// The original operation error was shortened before being retained.
    pub original_error_truncated: bool,
    /// SHA-256 of the original operation error when it was shortened.
    pub original_error_sha256: Option<String>,
}

/// A read-only snapshot with bounded streams, never a successful process result.
/// Diagnostics are capped when retained; session identity must be validated
/// before binding, and decoding untrusted input needs an earlier size limit.
#[derive(Clone, PartialEq, Eq)]
pub struct UnverifiedCleanupOutput {
    parts: UnverifiedCleanupOutputParts,
    metadata: UnverifiedCleanupOutputMetadata,
}

impl UnverifiedCleanupOutput {
    /// Bound each stream to the Shell output character limit. Retain the
    /// producer's truncation flags even if no additional truncation is needed.
    #[must_use]
    pub fn new(mut parts: UnverifiedCleanupOutputParts) -> Self {
        fn bounded_prefix(value: &str) -> (String, bool) {
            let end = value
                .char_indices()
                .nth(MAX_UNVERIFIED_OUTPUT_CHARS)
                .map_or(value.len(), |(index, _)| index);
            (value[..end].to_owned(), end < value.len())
        }
        let (stdout, stdout_truncated) = bounded_prefix(&parts.stdout);
        let (stderr, stderr_truncated) = bounded_prefix(&parts.stderr);
        parts.stdout = stdout;
        parts.stderr = stderr;
        parts.stdout_truncated |= stdout_truncated;
        parts.stderr_truncated |= stderr_truncated;
        fn bound_diagnostic(value: &mut String) -> (bool, Option<String>) {
            let end = value
                .char_indices()
                .nth(MAX_UNVERIFIED_METADATA_CHARS)
                .map_or(value.len(), |(index, _)| index);
            let truncated = end < value.len();
            let digest = truncated.then(|| {
                format!(
                    "{:x}",
                    <sha2::Sha256 as sha2::Digest>::digest(value.as_bytes())
                )
            });
            *value = value[..end].to_owned();
            (truncated, digest)
        }
        let (cleanup_detail_truncated, cleanup_detail_sha256) =
            bound_diagnostic(&mut parts.cleanup_detail);
        let (original_error_truncated, original_error_sha256) = parts
            .original_error
            .as_mut()
            .map_or((false, None), bound_diagnostic);
        Self {
            parts,
            metadata: UnverifiedCleanupOutputMetadata {
                cleanup_detail_truncated,
                cleanup_detail_sha256,
                original_error_truncated,
                original_error_sha256,
            },
        }
    }

    /// The retained output and all producer-supplied observations.
    #[must_use]
    pub fn parts(&self) -> &UnverifiedCleanupOutputParts {
        &self.parts
    }

    /// Truncation information for the retained diagnostic metadata.
    #[must_use]
    pub fn metadata(&self) -> &UnverifiedCleanupOutputMetadata {
        &self.metadata
    }
}

impl std::fmt::Debug for UnverifiedCleanupOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnverifiedCleanupOutput")
            .field("parts", &self.parts)
            .field(
                "cleanup_detail_truncated",
                &self.metadata.cleanup_detail_truncated,
            )
            .field(
                "original_error_truncated",
                &self.metadata.original_error_truncated,
            )
            .finish()
    }
}

impl std::fmt::Display for UnverifiedCleanupOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("process cleanup could not be verified")
    }
}

impl serde::Serialize for UnverifiedCleanupOutput {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(serde::Serialize)]
        struct Borrowed<'a> {
            parts: &'a UnverifiedCleanupOutputParts,
            metadata: &'a UnverifiedCleanupOutputMetadata,
        }
        serde::Serialize::serialize(
            &Borrowed {
                parts: &self.parts,
                metadata: &self.metadata,
            },
            serializer,
        )
    }
}

impl<'de> serde::Deserialize<'de> for UnverifiedCleanupOutput {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        struct Owned {
            parts: UnverifiedCleanupOutputParts,
            #[serde(default)]
            metadata: UnverifiedCleanupOutputMetadata,
        }
        let incoming = <Owned as serde::Deserialize>::deserialize(deserializer)?;
        let valid_digest = |digest: &Option<String>| {
            digest.as_ref().is_some_and(|digest| {
                digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        };
        if incoming.metadata.cleanup_detail_truncated
            != valid_digest(&incoming.metadata.cleanup_detail_sha256)
            || (!incoming.metadata.cleanup_detail_truncated
                && incoming.metadata.cleanup_detail_sha256.is_some())
            || incoming.metadata.original_error_truncated
                != valid_digest(&incoming.metadata.original_error_sha256)
            || (!incoming.metadata.original_error_truncated
                && incoming.metadata.original_error_sha256.is_some())
            || (incoming.metadata.original_error_truncated
                && incoming.parts.original_error.is_none())
        {
            return Err(serde::de::Error::custom(
                "invalid diagnostic truncation metadata",
            ));
        }
        let mut output = Self::new(incoming.parts);
        if incoming.metadata.cleanup_detail_truncated {
            output.metadata.cleanup_detail_truncated = true;
            output.metadata.cleanup_detail_sha256 = incoming.metadata.cleanup_detail_sha256;
        }
        if incoming.metadata.original_error_truncated {
            output.metadata.original_error_truncated = true;
            output.metadata.original_error_sha256 = incoming.metadata.original_error_sha256;
        }
        Ok(output)
    }
}

/// The shape of a reported exit field that did not provide a valid exit code.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ReportedExitCode {
    /// The field was absent.
    Missing,
    /// The field was explicitly null.
    Null,
    /// The field was neither a number nor null.
    InvalidType,
    /// A non-finite number, retaining its IEEE-754 representation.
    NonFinite { bits: u64 },
    /// A non-integral number, retaining its IEEE-754 representation.
    NonInteger { bits: u64 },
    /// An integral number outside the supported exit-code range.
    OutOfRange { bits: u64 },
}

/// A shell stream as reported by the tool, without invented EOF information.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ReportedShellStream {
    /// The field was absent.
    Missing,
    /// The field was explicitly null.
    Null,
    /// The field was not a string.
    InvalidType,
    /// Exact source UTF-16, possibly truncated only by this observation.
    Present {
        /// Original UTF-16 units, including unpaired surrogates.
        utf16: Vec<u16>,
        /// This observation discarded source units because of its own limit.
        host_truncated: bool,
    },
}

impl std::fmt::Debug for ReportedShellStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => f.write_str("Missing"),
            Self::Null => f.write_str("Null"),
            Self::InvalidType => f.write_str("InvalidType"),
            Self::Present {
                utf16,
                host_truncated,
            } => f
                .debug_struct("Present")
                .field("utf16_units", &utf16.len())
                .field("host_truncated", host_truncated)
                .finish(),
        }
    }
}

/// Independently reported shell fields; missing flags remain unknown.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReportedShellPayloadParts {
    /// Reported standard output.
    pub stdout: ReportedShellStream,
    /// Reported standard error.
    pub stderr: ReportedShellStream,
    /// The absent or invalid exit code field.
    pub exit_code: ReportedExitCode,
    /// The source's combined truncation flag, if supplied as a boolean.
    pub combined_truncated: Option<bool>,
    /// The source's timeout flag, if supplied as a boolean.
    pub timed_out: Option<bool>,
    /// The tool transport's independent error flag.
    pub tool_result_is_error: bool,
    /// The payload's independent error flag, if supplied as a boolean.
    pub payload_is_error: Option<bool>,
    /// The payload's interrupted flag, if supplied as a boolean.
    pub interrupted: Option<bool>,
}

/// A bounded observation of an invalid or missing shell result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportedShellPayload {
    parts: ReportedShellPayloadParts,
}

impl ReportedShellPayload {
    /// Retain at most the output limit in Unicode scalars per stream while
    /// preserving the exact UTF-16 units and not splitting surrogate pairs.
    #[must_use]
    pub fn new(mut parts: ReportedShellPayloadParts) -> Self {
        fn bound(stream: &mut ReportedShellStream) {
            let ReportedShellStream::Present {
                utf16,
                host_truncated,
            } = stream
            else {
                return;
            };
            let mut index = 0;
            let mut scalars = 0;
            while index < utf16.len() && scalars < MAX_UNVERIFIED_OUTPUT_CHARS {
                let high = (0xd800..=0xdbff).contains(&utf16[index]);
                if high
                    && utf16
                        .get(index + 1)
                        .is_some_and(|low| (0xdc00..=0xdfff).contains(low))
                {
                    index += 2;
                } else {
                    index += 1;
                }
                scalars += 1;
            }
            *host_truncated |= index < utf16.len();
            *utf16 = utf16[..index].to_vec();
        }
        bound(&mut parts.stdout);
        bound(&mut parts.stderr);
        Self { parts }
    }

    /// The retained fields and their independent source flags.
    #[must_use]
    pub fn parts(&self) -> &ReportedShellPayloadParts {
        &self.parts
    }
}

impl serde::Serialize for ReportedShellPayload {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serde::Serialize::serialize(&self.parts, serializer)
    }
}

impl<'de> serde::Deserialize<'de> for ReportedShellPayload {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let parts = <ReportedShellPayloadParts as serde::Deserialize>::deserialize(deserializer)?;
        Ok(Self::new(parts))
    }
}

/// The independent operation whose error accompanied an incomplete outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PriorFailureOrigin {
    /// A child Agent's terminal error, which may include provider failure.
    Agent,
    /// An independent provider stream or request error.
    Provider,
    /// A Mod dispatcher or plugin error.
    Mod,
    /// A terminal computer-input cleanup error.
    InputCleanup,
    /// A nested tool was not found; the text is the reported tool name.
    ToolNotFound,
    /// A nested tool rejected its input.
    ToolInvalidInput,
    /// A nested tool failed schema or precondition validation.
    ToolValidation,
    /// A nested tool was aborted.
    ToolAbort,
    /// A nested tool returned an internal failure.
    ToolInternal,
}

/// A bounded observation of an independent error, never proof of cleanup.
/// Raw text is intentionally absent from Debug and Display. Oversize errors
/// retain a prefix and SHA-256 over their original UTF-16LE units; the digest
/// is not the original text and does not prove it was stored elsewhere.
#[derive(Clone, PartialEq, Eq, serde::Serialize)]
pub struct PriorFailureContext {
    origin: PriorFailureOrigin,
    utf16: Vec<u16>,
    original_units: Option<usize>,
    truncated: bool,
    original_sha256: Option<String>,
}

impl<'de> serde::Deserialize<'de> for PriorFailureContext {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        struct Wire {
            origin: PriorFailureOrigin,
            utf16: Vec<u16>,
            original_units: Option<usize>,
            truncated: bool,
            original_sha256: Option<String>,
        }
        let wire = <Wire as serde::Deserialize>::deserialize(deserializer)?;
        let retained = Self::from_utf16(wire.origin, &wire.utf16);
        let valid_hash = wire.original_sha256.as_ref().is_some_and(|hash| {
            hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        });
        let valid_length = match wire.original_units {
            Some(length) if wire.truncated => length > wire.utf16.len(),
            Some(length) => length == wire.utf16.len(),
            None => wire.utf16.is_empty() && !wire.truncated,
        };
        if retained.truncated
            || !valid_length
            || wire.truncated != valid_hash
            || (!wire.truncated && wire.original_sha256.is_some())
        {
            return Err(serde::de::Error::custom("invalid prior failure context"));
        }
        Ok(Self {
            origin: wire.origin,
            utf16: retained.utf16,
            original_units: wire.original_units,
            truncated: wire.truncated,
            original_sha256: wire.original_sha256,
        })
    }
}

/// Maximum retained independent errors attached to one incomplete outcome.
pub const MAX_PRIOR_FAILURE_CONTEXTS: usize = 16;

impl PriorFailureContext {
    fn from_units(origin: PriorFailureOrigin, units: impl Iterator<Item = u16>) -> Self {
        use sha2::Digest;
        let mut source = units.peekable();
        let mut retained = Vec::new();
        let mut original_units = 0_usize;
        let mut scalars = 0_usize;
        let mut hash = sha2::Sha256::new();
        while let Some(unit) = source.next() {
            hash.update(unit.to_le_bytes());
            original_units = original_units.saturating_add(1);
            let pair = (0xd800..=0xdbff).contains(&unit)
                && source
                    .peek()
                    .is_some_and(|low| (0xdc00..=0xdfff).contains(low));
            let low = if pair { source.next() } else { None };
            if let Some(low) = low {
                hash.update(low.to_le_bytes());
                original_units = original_units.saturating_add(1);
            }
            if scalars < MAX_UNVERIFIED_METADATA_CHARS {
                retained.push(unit);
                if let Some(low) = low {
                    retained.push(low);
                }
            }
            scalars = scalars.saturating_add(1);
        }
        let truncated = scalars > MAX_UNVERIFIED_METADATA_CHARS;
        Self {
            origin,
            utf16: retained,
            original_units: Some(original_units),
            truncated,
            original_sha256: truncated.then(|| format!("{:x}", hash.finalize())),
        }
    }

    /// Record a UTF-8 error without allocating an unbounded UTF-16 copy.
    #[must_use]
    pub fn from_text(origin: PriorFailureOrigin, text: &str) -> Self {
        Self::from_units(origin, text.encode_utf16())
    }

    /// Record exact UTF-16, including unpaired surrogate units.
    #[must_use]
    pub fn from_utf16(origin: PriorFailureOrigin, units: &[u16]) -> Self {
        Self::from_units(origin, units.iter().copied())
    }

    /// Snapshot an ordinary ToolInvokerError without confusing it with the
    /// separate incomplete process result.
    #[must_use]
    pub fn from_tool_error(error: &ToolInvokerError) -> Option<Self> {
        let (origin, projection) = match error {
            ToolInvokerError::NotFound(name) => {
                return Some(Self::from_text(PriorFailureOrigin::ToolNotFound, name));
            }
            ToolInvokerError::Abort(message) => {
                return Some(Self::from_text(PriorFailureOrigin::ToolAbort, message));
            }
            ToolInvokerError::InvalidInput(value) => (PriorFailureOrigin::ToolInvalidInput, value),
            ToolInvokerError::Validation(value) => (PriorFailureOrigin::ToolValidation, value),
            ToolInvokerError::Internal(value) => (PriorFailureOrigin::ToolInternal, value),
            ToolInvokerError::CleanupUnverified(_) => return None,
        };
        Some(
            match projection
                .at_pointer("")
                .and_then(|value| value.code_units())
            {
                Some(units) => Self::from_utf16(origin, units),
                None => Self {
                    origin,
                    utf16: Vec::new(),
                    original_units: None,
                    truncated: false,
                    original_sha256: None,
                },
            },
        )
    }

    /// Source error category.
    #[must_use]
    pub fn origin(&self) -> PriorFailureOrigin {
        self.origin
    }
    /// Exact retained prefix; an absent source is distinguished by `original_units`.
    #[must_use]
    pub fn utf16(&self) -> &[u16] {
        &self.utf16
    }
    /// Original unit count, or `None` for a non-string error projection.
    #[must_use]
    pub fn original_units(&self) -> Option<usize> {
        self.original_units
    }
    /// Whether this observation omitted part of the source text.
    #[must_use]
    pub fn truncated(&self) -> bool {
        self.truncated
    }
    /// Digest of the complete UTF-16LE source only when a prefix was retained.
    #[must_use]
    pub fn original_sha256(&self) -> Option<&str> {
        self.original_sha256.as_deref()
    }
}

impl std::fmt::Debug for PriorFailureContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PriorFailureContext")
            .field("origin", &self.origin)
            .field("retained_units", &self.utf16.len())
            .field("original_units", &self.original_units)
            .field("truncated", &self.truncated)
            .finish_non_exhaustive()
    }
}

/// An incomplete process outcome carried through the scheduler and host.
#[derive(Clone, PartialEq, Eq)]
pub enum CleanupUnverifiedReason {
    /// A cleanup failure without captured process output.
    Message(String),
    /// Bounded output and original evidence; sharing avoids copying it for
    /// every queued tool stopped by the same failure.
    Output(Arc<UnverifiedCleanupOutput>),
    /// A shell response lacking a valid exit status, with its source fields.
    ReportedPayload(Arc<ReportedShellPayload>),
    /// An independently observed error accompanying the incomplete outcome.
    WithContext {
        /// The original incomplete outcome, unchanged.
        reason: Arc<CleanupUnverifiedReason>,
        /// Bounded independent error observations.
        contexts: Vec<PriorFailureContext>,
        /// Contexts omitted after the count limit; their contents are unavailable.
        omitted_context_count: usize,
    },
    /// Independent incomplete outcomes from concurrent tool invocations.
    Multiple(Vec<CleanupUnverifiedReason>),
}

impl std::fmt::Debug for CleanupUnverifiedReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message(_) => f.write_str("Message(<redacted>)"),
            Self::Output(output) => f.debug_tuple("Output").field(output).finish(),
            Self::ReportedPayload(payload) => {
                f.debug_tuple("ReportedPayload").field(payload).finish()
            }
            Self::WithContext {
                reason,
                contexts,
                omitted_context_count,
            } => f
                .debug_struct("WithContext")
                .field("reason", reason)
                .field("contexts", contexts)
                .field("omitted_context_count", omitted_context_count)
                .finish(),
            Self::Multiple(reasons) => f.debug_tuple("Multiple").field(reasons).finish(),
        }
    }
}

impl serde::Serialize for CleanupUnverifiedReason {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(serde::Serialize)]
        enum Borrowed<'a> {
            Message(&'a str),
            Output(&'a UnverifiedCleanupOutput),
            ReportedPayload(&'a ReportedShellPayload),
            WithContext {
                reason: &'a CleanupUnverifiedReason,
                contexts: &'a [PriorFailureContext],
                omitted_context_count: usize,
            },
            Multiple(&'a [CleanupUnverifiedReason]),
        }
        match self {
            Self::Message(message) => {
                serde::Serialize::serialize(&Borrowed::Message(message), serializer)
            }
            Self::Output(output) => {
                serde::Serialize::serialize(&Borrowed::Output(output), serializer)
            }
            Self::ReportedPayload(payload) => {
                serde::Serialize::serialize(&Borrowed::ReportedPayload(payload), serializer)
            }
            Self::WithContext {
                reason,
                contexts,
                omitted_context_count,
            } => serde::Serialize::serialize(
                &Borrowed::WithContext {
                    reason,
                    contexts,
                    omitted_context_count: *omitted_context_count,
                },
                serializer,
            ),
            Self::Multiple(reasons) => {
                serde::Serialize::serialize(&Borrowed::Multiple(reasons), serializer)
            }
        }
    }
}

impl<'de> serde::Deserialize<'de> for CleanupUnverifiedReason {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        enum Owned {
            Message(String),
            Output(UnverifiedCleanupOutput),
            ReportedPayload(ReportedShellPayload),
            WithContext {
                reason: Box<CleanupUnverifiedReason>,
                contexts: Vec<PriorFailureContext>,
                omitted_context_count: usize,
            },
            Multiple(Vec<CleanupUnverifiedReason>),
        }
        match <Owned as serde::Deserialize>::deserialize(deserializer)? {
            Owned::Message(message) => Ok(Self::Message(message)),
            Owned::Output(output) => Ok(Self::Output(Arc::new(output))),
            Owned::ReportedPayload(payload) => Ok(Self::ReportedPayload(Arc::new(payload))),
            Owned::WithContext {
                reason,
                mut contexts,
                mut omitted_context_count,
            } => {
                if contexts.len() > MAX_PRIOR_FAILURE_CONTEXTS {
                    omitted_context_count = omitted_context_count
                        .saturating_add(contexts.len() - MAX_PRIOR_FAILURE_CONTEXTS);
                    contexts.truncate(MAX_PRIOR_FAILURE_CONTEXTS);
                    contexts.shrink_to_fit();
                }
                Ok(Self::WithContext {
                    reason: Arc::from(reason),
                    contexts,
                    omitted_context_count,
                })
            }
            Owned::Multiple(reasons) => Ok(Self::Multiple(reasons)),
        }
    }
}

impl From<String> for CleanupUnverifiedReason {
    fn from(message: String) -> Self {
        Self::Message(message)
    }
}

impl From<&str> for CleanupUnverifiedReason {
    fn from(message: &str) -> Self {
        Self::Message(message.to_owned())
    }
}

impl From<Box<UnverifiedCleanupOutput>> for CleanupUnverifiedReason {
    fn from(output: Box<UnverifiedCleanupOutput>) -> Self {
        Self::Output(Arc::from(output))
    }
}

impl CleanupUnverifiedReason {
    /// Attach an independently observed failure without relabeling it as a
    /// second process outcome. The context itself is not cleanup proof.
    #[must_use]
    pub fn with_context(self, context: PriorFailureContext) -> Self {
        match self {
            Self::WithContext {
                reason,
                mut contexts,
                mut omitted_context_count,
            } => {
                if contexts.len() < MAX_PRIOR_FAILURE_CONTEXTS {
                    contexts.push(context);
                } else {
                    omitted_context_count = omitted_context_count.saturating_add(1);
                }
                Self::WithContext {
                    reason,
                    contexts,
                    omitted_context_count,
                }
            }
            reason => Self::WithContext {
                reason: Arc::new(reason),
                contexts: vec![context],
                omitted_context_count: 0,
            },
        }
    }
    /// The producer's diagnostic detail, without the host-facing prefix.
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Self::Message(message) => message,
            Self::Output(output) => &output.parts().cleanup_detail,
            Self::ReportedPayload(_) => "shell result did not contain a valid exit status",
            Self::WithContext { reason, .. } => reason.detail(),
            Self::Multiple(_) => "process outcome evidence is incomplete",
        }
    }
}

impl std::fmt::Display for CleanupUnverifiedReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message(_) | Self::Output(_) => {
                f.write_str("process cleanup could not be verified")
            }
            Self::ReportedPayload(_) => {
                f.write_str("shell result did not contain a valid exit status")
            }
            Self::WithContext { reason, .. } => write!(f, "{reason} (additional error retained)"),
            Self::Multiple(reasons) => {
                write!(f, "{} process outcomes are incomplete", reasons.len())
            }
        }
    }
}

/// Upper bounds for a host implementing cleanup-owner admission. A host may
/// enforce lower limits; it must reject admission before the side effect when
/// it cannot reserve the requested space.
pub const MAX_CLEANUP_OWNERS_PER_SESSION: usize = 32;
/// Global owner-slot ceiling across all sessions in the host process.
pub const MAX_CLEANUP_OWNERS_PER_PROCESS: usize = 256;
/// Maximum reserved in-memory evidence bytes for one owner.
pub const MAX_CLEANUP_EVIDENCE_BYTES_PER_OWNER: usize = 512 * 1024;
/// Maximum number of observations in one owner's aggregate.
pub const MAX_CLEANUP_OBSERVATIONS_PER_OWNER: usize = 32;
/// Maximum recursive aggregate depth accepted for one owner's evidence.
pub const MAX_CLEANUP_EVIDENCE_DEPTH: usize = 32;

/// A host-assigned correlation ID. It is not an authentication credential.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct CleanupOriginId(String);

impl CleanupOriginId {
    /// Accept a bounded identifier minted by the trusted host.
    #[must_use]
    pub fn new(value: &str) -> Option<Self> {
        (!value.is_empty()
            && value.len() <= 128
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')
            }))
        .then(|| Self(value.to_owned()))
    }

    /// The host-assigned identifier, for an authorized in-process consumer.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for CleanupOriginId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CleanupOriginId(<redacted>)")
    }
}

/// Trusted classification of the capability that can start external work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupOwnerOrigin {
    /// A child Agent invocation.
    Agent,
    /// A background local Agent invocation.
    BackgroundAgent,
    /// A Dream task.
    Dream,
    /// A specific host-owned hook invocation.
    Hook { invocation_id: CleanupOriginId },
    /// A specific Fusion run and task.
    Fusion {
        run_id: CleanupOriginId,
        task_id: CleanupOriginId,
    },
    /// A specific workflow run and agent call.
    Workflow {
        run_id: CleanupOriginId,
        call_id: CleanupOriginId,
    },
}

/// Admission facts supplied by trusted host code before starting external work.
/// The authority independently verifies session, recipient, and generation;
/// none of these correlation fields authenticates a caller on its own.
#[derive(Clone)]
pub struct CleanupOwnerRequest {
    /// Canonical host session ID.
    pub session_id: SessionId,
    /// The Agent receiving the work, when applicable.
    pub recipient: Option<AgentId>,
    /// The host-owned invocation or task.
    pub origin: CleanupOwnerOrigin,
    /// Correlation with a tool call, if available and host-validated.
    pub tool_use_id: Option<crate::types::ToolUseId>,
    /// Memory reserved before starting work; at most the owner evidence limit.
    pub evidence_budget_bytes: usize,
}

/// Fixed, non-sensitive admission failures. An unavailable authority rejects
/// high-risk work before spawning; it does not claim a process exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CleanupAdmissionError {
    /// Host lifecycle capability or cold recovery is unavailable.
    #[error("cleanup admission is unavailable")]
    Unavailable,
    /// A prior owner in this or an older generation remains unresolved.
    #[error("session cleanup is unresolved")]
    Fenced,
    /// The host cannot reserve bounded owner or evidence capacity.
    #[error("cleanup admission capacity is exhausted")]
    Capacity,
    /// A caller's session or recipient does not match the trusted authority.
    #[error("cleanup admission identity did not match")]
    IdentityMismatch,
}

/// What happened when an owner reported evidence. Both states remain fatal;
/// an unavailable record must never be represented as a complete observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupRecordDisposition {
    /// The complete admitted observation is held by the host owner.
    Retained,
    /// Some evidence could not be retained or accounted for.
    EvidenceUnavailable,
}

/// A fixed indication that an admitted operation no longer has a waiter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupAbandonCause {
    /// The original caller dropped its wait.
    CallerDropped,
    /// A bounded wait expired.
    DeadlineExpired,
    /// The responsible supervisor became unavailable.
    SupervisorUnavailable,
}

/// Opaque, host-minted capability for one admitted owner. A report fences the
/// session atomically and is not a verified-reaping receipt. The host must
/// keep polling and own any actual task after its caller abandons the wait.
pub trait CleanupOwner: Send + Sync {
    /// Retain or explicitly mark unavailable the incomplete observation.
    fn report_unverified(&self, reason: CleanupUnverifiedReason) -> CleanupRecordDisposition;
    /// Report an abandoned wait while keeping the actual owner alive.
    fn report_abandoned(&self, cause: CleanupAbandonCause) -> CleanupRecordDisposition;
}

/// An admitted model request, cancelled by the host if an owner becomes fatal.
/// The host must keep its admission until this capability is dropped.
pub trait CleanupModelPermit: Send + Sync {
    /// Cancellation is notification only; it does not prove any tree exited.
    fn cancellation_token(&self) -> tokio_util::sync::CancellationToken;
}

/// Host-owned session/generation authority. A concrete implementation must
/// atomically reserve a unique, non-reusable owner identity and the evidence
/// quota before allowing a side effect. It must retain pending work and late
/// receipts independently of the caller, enforce the limits above, reject old
/// or mismatched owners, and keep older unresolved generations fenced. Only a
/// trusted supervisor can establish verified cleanup; this interface exposes
/// no caller-controlled success method. Missing durable recovery is fatal on
/// restart, not permission to forget unresolved owners.
pub trait CleanupAdmissionAuthority: Send + Sync {
    /// Atomically check the session fence and reserve owner/evidence capacity.
    fn reserve(
        &self,
        request: CleanupOwnerRequest,
    ) -> Result<Arc<dyn CleanupOwner>, CleanupAdmissionError>;
    /// Atomically admit a model request against the same session fence.
    fn admit_model(
        &self,
        session_id: SessionId,
    ) -> Result<Arc<dyn CleanupModelPermit>, CleanupAdmissionError>;
    /// Inspect the fence, without using this alone to authorize a new request.
    fn check_session(&self, session_id: SessionId) -> Result<(), CleanupAdmissionError>;
}

/// Failure modes for [`ToolInvoker::invoke`].
#[derive(Debug, Error)]
pub enum ToolInvokerError {
    /// The named tool was not found in the registry.
    #[error("ToolInvoker: tool '{0}' not found")]
    NotFound(String),
    /// The tool surfaced an invalid input.
    #[error("ToolInvoker: invalid input")]
    InvalidInput(Utf16JsonProjection),
    /// A schema or tool precondition rejected input before execution.
    #[error("ToolInvoker: input validation failed")]
    Validation(Utf16JsonProjection),
    /// Permission policy terminated the owning prompt-avoiding agent.
    ///
    /// Unlike an ordinary denial, callers must not convert this into a
    /// recoverable `tool_result` and continue the model loop.
    #[error("{0}")]
    Abort(String),
    /// A tool's process outcome is incomplete and the caller must not continue
    /// the model loop as if the tool had returned an ordinary recoverable error.
    #[error("{0}")]
    CleanupUnverified(CleanupUnverifiedReason),
    /// Any other internal failure.
    #[error("ToolInvoker: internal error: {}", .0.string_at("").unwrap_or_default())]
    Internal(Utf16JsonProjection),
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
    pub fn model_facing_message(&self) -> Utf16JsonProjection {
        match self {
            Self::NotFound(name) => format!("tool '{name}' not found").into(),
            Self::InvalidInput(message) | Self::Validation(message) | Self::Internal(message) => {
                message.clone()
            }
            Self::Abort(message) => message.clone().into(),
            Self::CleanupUnverified(reason) => reason.to_string().into(),
        }
    }
    /// Native result wrappers differ for pre-execution validation and thrown errors.
    #[must_use]
    pub fn model_tool_result_content(&self) -> Utf16JsonProjection {
        match self {
            Self::Validation(message) => {
                wrap_source_error_text("<tool_use_error>", message, "</tool_use_error>")
            }
            _ => wrap_source_error_text("Error: ", &self.model_facing_message(), ""),
        }
    }
}

fn wrap_source_error_text(
    prefix: &str,
    message: &Utf16JsonProjection,
    suffix: &str,
) -> Utf16JsonProjection {
    let message_units = message
        .string_units("")
        .expect("ToolInvoker model-facing error messages are root strings");
    let mut units = prefix.encode_utf16().collect::<Vec<_>>();
    units.extend(message_units);
    units.extend(suffix.encode_utf16());
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("UTF-16 error wrappers have a valid display form")
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
    pub data: crate::types::utf16_json::Utf16JsonProjection,
    /// Optional prose supplied by the tool's model-facing result mapper.
    pub model_content: Option<crate::types::utf16_json::Utf16JsonProjection>,
    /// Extra conversation messages returned by the underlying tool.
    pub new_messages: Vec<crate::types::ConversationMessage>,
    /// One-shot mutation of the concrete per-agent ToolUseContext, kept
    /// separate from model-only reminders in [`Self::context`].
    pub context_modifier: Option<ToolInvocationContextModifier>,
    /// Opaque tool metadata retained for host-side result handling.
    pub mcp_meta: Option<crate::types::utf16_json::Utf16JsonProjection>,
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
    fn map_result_text(
        &self,
        _name: &str,
        _result: &crate::types::utf16_json::Utf16JsonProjection,
    ) -> Option<crate::types::utf16_json::Utf16JsonProjection> {
        None
    }

    /// Tool mapper's logical error flag for a replacement result, if defined.
    fn map_result_is_error(
        &self,
        _name: &str,
        _result: &crate::types::utf16_json::Utf16JsonProjection,
    ) -> Option<bool> {
        None
    }

    /// Parse a replacement result through the registered tool's native output
    /// parser without executing the tool body. The returned projection is the
    /// parser's transformed value, not merely a validation verdict.
    async fn parse_output(
        &self,
        _name: &str,
        _output: &crate::types::utf16_json::Utf16JsonProjection,
    ) -> Result<crate::types::utf16_json::Utf16JsonProjection, ToolInvokerError> {
        Err(ToolInvokerError::Internal(
            "Source tool output parser is unavailable".into(),
        ))
    }

    /// Resolve concurrency safety from the actual registered tool selected for
    /// this call. `None` means this invoker cannot resolve the tool; callers
    /// must not infer safety from a tool name or model-facing JSON.
    fn tool_is_concurrency_safe(
        &self,
        _name: &str,
        _input: &crate::types::utf16_json::Utf16JsonProjection,
    ) -> Option<bool> {
        None
    }

    /// Invoke the tool named `name` with the supplied JSON `input`.
    async fn invoke(
        &self,
        name: &str,
        input: crate::types::utf16_json::Utf16JsonProjection,
        ctx: SubagentInvocationContext,
    ) -> Result<crate::types::utf16_json::Utf16JsonProjection, ToolInvokerError>;

    /// Preserve model-facing text without replacing the structured result.
    async fn invoke_detailed(
        &self,
        name: &str,
        input: crate::types::utf16_json::Utf16JsonProjection,
        ctx: SubagentInvocationContext,
    ) -> Result<ToolInvocationResult, ToolInvokerError> {
        self.invoke(name, input, ctx)
            .await
            .map(|data| ToolInvocationResult {
                is_error: false,
                data,
                model_content: None,
                new_messages: Vec::new(),
                context_modifier: None,
                mcp_meta: None,
                turn_end: None,
                context: crate::types::utf16_json::Utf16JsonProjection::parse("[]")
                    .expect("empty JSON array is valid"),
                context_state: None,
            })
    }

    /// Invoke one host-supplied child tool through the same enforcing dispatch
    /// path. The opaque carrier is minted by the runtime, never model JSON.
    /// Wrappers must forward it unchanged; unsupported hosts fail closed.
    async fn invoke_supplied_detailed(
        &self,
        name: &str,
        _input: crate::types::utf16_json::Utf16JsonProjection,
        _ctx: SubagentInvocationContext,
        _supplied: Arc<dyn Any + Send + Sync>,
    ) -> Result<ToolInvocationResult, ToolInvokerError> {
        Err(ToolInvokerError::Internal(
            format!("Host-supplied tool {name} dispatch is unavailable").into(),
        ))
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
            ("1", 2, Some(0)),
            ("2.0", 2, Some(1)),
            ("\"1\"", 2, Some(0)),
            ("\" 2 \"", 2, Some(1)),
            ("true", 2, Some(0)),
            ("[1]", 2, Some(0)),
            ("[2]", 2, Some(1)),
            ("[[1]]", 2, Some(0)),
            ("\"0x2\"", 2, Some(1)),
            ("\"0b10\"", 2, Some(1)),
            ("\"0o2\"", 2, Some(1)),
            ("\"\\ufeff1\"", 2, Some(0)),
            ("false", 2, None),
            ("null", 2, None),
            ("[]", 2, None),
            ("[null]", 2, None),
            ("{}", 2, None),
            ("0", 2, None),
            ("3", 2, None),
            ("1.5", 2, None),
            ("\"1.5\"", 2, None),
            ("\"bad\"", 2, None),
            ("\"NaN\"", 2, None),
            ("\"Infinity\"", 2, None),
            ("\"\\u00851\"", 2, None),
            ("1", 0, None),
        ] {
            let projection = crate::types::utf16_json::Utf16JsonProjection::parse(reference)
                .expect("test input is JSON");
            assert_eq!(
                tool_call_ref_index(projection.at_pointer("").unwrap(), count),
                expected,
                "reference={reference}, run_count={count}"
            );
        }
    }

    #[test]
    fn native_tool_call_ref_numbers_round_through_ieee754_before_bounds_check() {
        let large_integer =
            crate::types::utf16_json::Utf16JsonProjection::parse("9007199254740993").unwrap();
        let reference = large_integer.at_pointer("").unwrap();
        assert_eq!(js_number(reference), Some(9_007_199_254_740_992.0));
        assert_eq!(tool_call_ref_index(reference, 2), None);
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

#[cfg(test)]
mod source_error_projection_tests {
    use super::ToolInvokerError;
    use crate::types::utf16_json::Utf16JsonProjection;

    #[test]
    fn internal_error_preserves_source_text_in_tool_result() {
        let detail = Utf16JsonProjection::parse(r#""permission \ud800 denied""#).unwrap();
        let error = ToolInvokerError::Internal(detail.clone());
        assert_eq!(
            error.model_facing_message().string_units(""),
            detail.string_units("")
        );
        assert_eq!(
            error.model_tool_result_content().string_units(""),
            Some(
                "Error: permission "
                    .encode_utf16()
                    .chain([0xd800])
                    .chain(" denied".encode_utf16())
                    .collect()
            )
        );
    }

    #[test]
    fn validation_wrapper_preserves_lone_surrogate_error_text() {
        let detail = Utf16JsonProjection::parse(r#""bad \ud800 value""#).unwrap();
        let error = ToolInvokerError::Validation(detail);
        let content = error.model_tool_result_content();
        assert_eq!(
            content.string_units(""),
            Some(
                "<tool_use_error>bad "
                    .encode_utf16()
                    .chain([0xd800])
                    .chain(" value</tool_use_error>".encode_utf16())
                    .collect()
            )
        );
    }
}

#[cfg(test)]
mod incomplete_output_tests {
    use super::*;

    #[test]
    fn bounded_metadata_retains_hash_and_survives_structured_roundtrip() {
        let cleanup = format!("{}tail", "界".repeat(MAX_UNVERIFIED_METADATA_CHARS * 64));
        let original = format!("{}tail", "错".repeat(MAX_UNVERIFIED_METADATA_CHARS * 64));
        let digest = |value: &str| {
            format!(
                "{:x}",
                <sha2::Sha256 as sha2::Digest>::digest(value.as_bytes())
            )
        };
        let output = UnverifiedCleanupOutput::new(UnverifiedCleanupOutputParts {
            session_id: Some("session".into()),
            stdout: "partial out".into(),
            stderr: "partial err".into(),
            stdout_truncated: false,
            stderr_truncated: false,
            stdout_eof: false,
            stderr_eof: true,
            exit_code: None,
            timed_out: false,
            termination_requested: Some(true),
            tree_status: ProcessTreeStatus::Unverified,
            cleanup_detail: cleanup.clone(),
            original_error: Some(original.clone()),
        });
        assert_eq!(
            output.parts().cleanup_detail.chars().count(),
            MAX_UNVERIFIED_METADATA_CHARS
        );
        assert_eq!(
            output
                .parts()
                .original_error
                .as_ref()
                .unwrap()
                .chars()
                .count(),
            MAX_UNVERIFIED_METADATA_CHARS
        );
        assert!(output.parts().cleanup_detail.capacity() < cleanup.len() / 2);
        assert!(output.parts().original_error.as_ref().unwrap().capacity() < original.len() / 2);
        assert!(output.metadata().cleanup_detail_truncated);
        assert!(output.metadata().original_error_truncated);
        assert_eq!(
            output.metadata().cleanup_detail_sha256.as_deref(),
            Some(digest(&cleanup).as_str())
        );
        assert_eq!(
            output.metadata().original_error_sha256.as_deref(),
            Some(digest(&original).as_str())
        );
        assert_eq!(output.to_string(), "process cleanup could not be verified");
        assert!(!format!("{output:?}").contains("partial out"));
        let mut spare_detail = String::with_capacity(1_000_000);
        spare_detail.push_str("detail");
        let mut spare_original = String::with_capacity(1_000_000);
        spare_original.push_str("original");
        let mut parts = output.parts().clone();
        parts.cleanup_detail = spare_detail;
        parts.original_error = Some(spare_original);
        let short = UnverifiedCleanupOutput::new(parts);
        assert!(short.parts().cleanup_detail.capacity() < 100);
        assert!(short.parts().original_error.as_ref().unwrap().capacity() < 100);
        assert!(!short.metadata().cleanup_detail_truncated);
        assert!(!short.metadata().original_error_truncated);
        assert_eq!(short.metadata().cleanup_detail_sha256, None);
        assert_eq!(short.metadata().original_error_sha256, None);
        let reason = CleanupUnverifiedReason::Multiple(vec![
            CleanupUnverifiedReason::Message("another incomplete process".into()),
            CleanupUnverifiedReason::Output(Arc::new(output)),
        ]);
        assert!(!format!("{reason:?}").contains("partial out"));
        let encoded = serde_json::to_string(&reason).unwrap();
        let decoded: CleanupUnverifiedReason = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, reason);
    }

    #[test]
    fn reported_shell_retains_unknown_fields_and_exact_utf16_boundary() {
        let mut source = vec![b'a' as u16; MAX_UNVERIFIED_OUTPUT_CHARS - 1];
        source.extend([0xd83d, 0xde00, 0xd800]);
        let mut spare_stderr = Vec::with_capacity(1_000_000);
        spare_stderr.extend([0xdc00, b'x' as u16]);
        let payload = ReportedShellPayload::new(ReportedShellPayloadParts {
            stdout: ReportedShellStream::Present {
                utf16: source,
                host_truncated: false,
            },
            stderr: ReportedShellStream::Present {
                utf16: spare_stderr,
                host_truncated: false,
            },
            exit_code: ReportedExitCode::NonInteger {
                bits: 1.5_f64.to_bits(),
            },
            combined_truncated: None,
            timed_out: Some(false),
            tool_result_is_error: true,
            payload_is_error: None,
            interrupted: Some(true),
        });
        let ReportedShellStream::Present {
            utf16,
            host_truncated,
        } = &payload.parts().stdout
        else {
            panic!("stdout must remain a reported string");
        };
        assert!(*host_truncated);
        assert_eq!(utf16.len(), MAX_UNVERIFIED_OUTPUT_CHARS + 1);
        assert_eq!(&utf16[utf16.len() - 2..], &[0xd83d, 0xde00]);
        assert!(
            matches!(&payload.parts().stderr, ReportedShellStream::Present { utf16, host_truncated: false } if utf16.as_slice() == [0xdc00, b'x' as u16])
        );
        assert!(
            matches!(&payload.parts().stderr, ReportedShellStream::Present { utf16, .. } if utf16.capacity() < 100)
        );
        assert_eq!(payload.parts().combined_truncated, None);
        assert_eq!(payload.parts().payload_is_error, None);
        let reason = CleanupUnverifiedReason::ReportedPayload(Arc::new(payload));
        let batch = CleanupUnverifiedReason::Multiple(vec![reason.clone()]);
        assert_eq!(batch.detail(), "process outcome evidence is incomplete");
        let encoded = serde_json::to_string(&reason).unwrap();
        let decoded: CleanupUnverifiedReason = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, reason);
    }
}
