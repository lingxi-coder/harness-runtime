//! Hook dispatch seam for inbound MCP requests.
//!
//! The mcp crate is deliberately decoupled from the `hooks` crate: it knows
//! nothing about `HookEvent` / `HookRegistry`. To let an incoming
//! `elicitation/create` consult the `Elicitation` hook (claude-code
//! `services/mcp/elicitationHandler.ts:91-107` + `runElicitationHooks`
//! lines 214-257), we define a NARROW trait here that the orchestrator (which
//! owns the hook registry) implements and injects.
//!
//! The shape is intentionally elicitation-specific rather than a generic
//! `dispatch(HookEvent, HookContext) -> AggregateHookResult` so the mcp crate
//! never has to import hooks types. The orchestrator's adapter translates an
//! [`ElicitationHookRequest`] into a `hooks::HookEvent::Elicitation`, fires the
//! registry, and folds the aggregate back into an [`ElicitationHookOutcome`].
//!
//! Default wiring is `None` => current behavior (strict no-op), matching the
//! `RawConnectionProvider` / auth-provider injection pattern already used by
//! [`crate::McpRegistry`].

use async_trait::async_trait;
use serde_json::Value;

/// Byte-faithful inputs for the `Elicitation` hook, mirroring the fields
/// `runElicitationHooks` forwards to `executeElicitationHooks`
/// (`elicitationHandler.ts:227-239`). Optional fields are `None` when the
/// server omitted them; the orchestrator threads each into the wire payload
/// (`hooks::ElicitationPayload`) exactly as claude-code does.
#[derive(Debug, Clone, Default)]
pub struct ElicitationHookRequest {
    /// Logical MCP server name (wire `mcp_server_name`).
    pub server_name: String,
    /// Human-readable prompt shown to the user (wire `message`, required).
    pub message: String,
    /// Presentation mode string (`"form"` / `"url"`), if the server set it.
    /// Kept as a raw string here so the mcp crate need not depend on the
    /// hooks `ElicitationMode` enum; the orchestrator maps it.
    pub mode: Option<String>,
    /// URL to open when `mode == "url"`, if specified.
    pub url: Option<String>,
    /// Server-assigned elicitation ID, if specified.
    pub elicitation_id: Option<String>,
    /// JSON Schema describing the requested form fields (wire
    /// `requested_schema`), if specified.
    pub requested_schema: Option<Value>,
}

/// Outcome of firing the `Elicitation` hook, mapped 1:1 onto the three
/// branches of `runElicitationHooks` (`elicitationHandler.ts:241-252`):
///
/// * [`Self::Respond`] — a hook returned an `elicitationResponse`; its
///   `{action, content}` becomes the elicitation answer.
/// * [`Self::Deny`] — a hook produced a `blockingError` (top-level
///   `decision: 'block'`, exit-2, or `action: 'decline'`); the elicitation
///   is declined.
/// * [`Self::Pass`] — no hook intervened; fall through to the host's default
///   behavior (the inbound handler's `{"action":"cancel"}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElicitationHookOutcome {
    /// No hook provided a response or block — use the default behavior.
    Pass,
    /// A hook provided the elicitation answer. `answer` is the full JSON object
    /// the handler returns to the server (e.g. `{"action":"accept",
    /// "content":{...}}`), already shaped so it can be returned verbatim.
    Respond(Value),
    /// A hook denied the elicitation. Equivalent to `{ action: 'decline' }`.
    Deny,
}

/// Seam for letting inbound MCP requests consult the engine's hook registry.
///
/// Implemented by the orchestrator over its `HookExecutorImpl`; injected into
/// [`crate::inbound::ElicitationCreateHandler`] (and forwarded through
/// [`crate::McpClient`] / [`crate::McpRegistry`]). When no dispatcher is wired
/// the inbound handler keeps its claude-code default behavior, so this is a
/// strict, opt-in superset of the prior code path.
///
/// Best-effort contract: an implementor MUST NOT propagate panics/errors that
/// would break the elicitation flow — on any internal failure it should return
/// [`ElicitationHookOutcome::Pass`] so the default behavior takes over.
#[async_trait]
pub trait HookDispatcher: Send + Sync {
    /// Fire the `Elicitation` hook for an incoming `elicitation/create`
    /// request and report how the hook resolved it.
    async fn dispatch_elicitation(&self, request: ElicitationHookRequest)
        -> ElicitationHookOutcome;
}
