//! `Tool` trait — the contract every tool implements.
//!
//! Static methods reason about the tool itself (name, schemas, capabilities);
//! dynamic methods reason about a specific `(input, ctx)` pair (permission,
//! concurrency safety, validation, execution).

use async_trait::async_trait;
use permission::PermissionResult;
use serde_json::Value;
use std::path::PathBuf;
use thiserror::Error;

use crate::context::ToolUseContext;
use crate::progress::ToolProgressSender;

/// Contract implemented by every tool the agent can call.
///
/// A `Tool` exposes static metadata (name, input/output schema, enablement,
/// concurrency hints) and dynamic behavior (permission check, validation,
/// execution). The dispatcher consults the static side to decide scheduling
/// and the dynamic side to actually run a call.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Canonical tool name (e.g. `"Read"`, `"Bash"`).
    fn name(&self) -> &str;

    /// Alternate names the dispatcher should accept for this tool.
    ///
    /// Defaults to no aliases.
    fn aliases(&self) -> &[&str] {
        &[]
    }

    /// Canonical V1 tool identity used by tool-pool configuration matching.
    fn underlying_v1_tool_name(&self) -> Option<&str> {
        None
    }

    /// Parent tool identity for a member of a split tool family.
    fn family_parent_tool_name(&self) -> Option<&str> {
        None
    }

    /// Short hint shown in tool-search UIs, if any.
    fn search_hint(&self) -> Option<&str> {
        None
    }

    /// Human-readable label shown to the user for this tool (claude-code
    /// `userFacingName()`), distinct from the canonical wire [`name`](Self::name).
    /// `None` means "use the canonical name" — claude-code falls back to the tool
    /// name when no override is supplied.
    fn user_facing_name(&self) -> Option<&str> {
        None
    }

    /// JSON Schema for the tool's input parameters.
    fn input_schema(&self) -> &Value;

    /// Owned schema snapshot for capabilities that can change while a
    /// registry is live. The wire builder calls this for each model request,
    /// so a capability update cannot leave a stale action list in the schema
    /// cache or ToolSearch view. Static tools keep the borrowed schema above.
    fn input_schema_snapshot(&self) -> Option<Value> {
        None
    }

    /// Revision token for a live schema snapshot. Include every non-content
    /// version component that changes its shape (for example a device service
    /// epoch and supported-operation revision) so per-session wire caches are
    /// invalidated before reusing serialized schemas.
    fn input_schema_revision(&self) -> Option<String> {
        None
    }

    /// Runtime parser schema, distinct from advertised foreign JSON Schema.
    /// MCP server schemas are inputJSONSchema on the wire; their local parser
    /// is the passthrough object inherited from H4 (Claude Code 2.1.263).
    fn input_validation_schema(&self) -> &Value {
        self.input_schema()
    }

    /// Native Zod-v4 issues for checks absent from exported JSON Schema.
    /// Preserve declaration order and object key insertion order: the fallback
    /// diagnostic is ZodError.message (JSON.stringify(issues, null, 2)).
    /// Return only refinement/custom checks; structural validation runs first.
    /// Validators must skip refinements on inputs whose base types cannot parse.
    fn input_validation_issues(&self, _input: &Value) -> Vec<Value> {
        Vec::new()
    }

    /// Use the shared native schema gate on nested invocations as well as the
    /// main turn. Opt-in tools use the default flat schema parser or override
    /// `parse_native_input` for native refinements and normalization.
    fn native_input_validation(&self) -> bool {
        false
    }

    /// Parse native schema defaults, stripping, and diagnostics at either dispatch boundary.
    /// Tools with refinements may override this while retaining the shared issue formatter.
    fn parse_native_input(
        &self,
        input: &Value,
    ) -> Option<Result<Value, crate::native_schema::NativeSchemaError>> {
        if !self.native_input_validation() {
            return None;
        }
        crate::native_schema::validate_flat_input(
            self.name(),
            self.input_validation_schema(),
            input,
        )
        .map(|result| {
            result.map(|()| {
                crate::native_schema::normalize_flat_input(input).unwrap_or_else(|| input.clone())
            })
        })
    }

    /// Optional JSON Schema for the tool's output.
    fn output_schema(&self) -> Option<&Value> {
        None
    }

    /// Whether this tool is enabled for the given static context
    /// (feature flags, environment, etc.).
    fn is_enabled(&self, ctx: &ToolStaticContext) -> bool;

    /// Whether this tool is sourced from an MCP server.
    fn is_mcp(&self) -> bool {
        false
    }

    /// Observe a static JSON-schema rejection before the dispatcher returns it
    /// to the model. Implementations may use this for rejection telemetry; the
    /// default deliberately has no side effects.
    async fn on_input_schema_rejected(
        &self,
        _input: &Value,
        _tool_use_id: Option<&str>,
        _assistant_message_id: Option<&protocol::MessageId>,
    ) {
    }

    /// Whether this successful result asks the query loop to end immediately.
    ///
    /// Claude Code carries this as `ToolResult.endsTurn`. It is deliberately a
    /// result-sensitive hook rather than a static tool capability so a tool may
    /// decide per invocation. The default keeps every existing tool unchanged.
    fn result_ends_turn(&self, _result: &ToolCallResult) -> bool {
        false
    }

    /// Optional MCP server routing role.  Native and generic tools return
    /// `None`; MCP per-tool entries may expose `comms` to coordinator routing.
    fn mcp_role(&self) -> Option<&str> {
        None
    }

    /// Whether this tool is sourced from an LSP server.
    fn is_lsp(&self) -> bool {
        false
    }

    /// Whether this tool may require user interaction mid-call
    /// (e.g. an interactive prompt).
    fn requires_user_interaction(&self) -> bool {
        false
    }

    /// Whether the dispatcher should defer this tool until later in the
    /// schedule (e.g. heavyweight tools queued behind quick ones).
    fn should_defer(&self) -> bool {
        false
    }

    /// Whether this tool must always be loaded in the prompt, even when the
    /// loader would otherwise prune it.
    fn always_load(&self) -> bool {
        false
    }

    /// Whether strict JSON-schema validation is enforced on the input.
    fn strict(&self) -> bool {
        false
    }

    /// Maximum size (in characters) of the tool's serialized result before
    /// truncation kicks in.
    fn max_result_size_chars(&self) -> usize;

    /// BYTE length above which the model-facing `tool_result` content is
    /// written to `<session>/tool-results/<id>.txt` and replaced by a
    /// `<persisted-output>` reference
    /// (see `orchestrator::tool_result_persistence`).
    ///
    /// This is claude-code 2.1.220's `maxResultSizeChars` folded through `M0u`
    /// — a PERSISTENCE threshold, NOT a truncation cap. `None` is the oracle's
    /// `maxResultSizeChars: 1/0` (`Infinity`, e.g. `Read` @ 2.1.220:235738242)
    /// whose `!Number.isFinite(t)` arm returns early and never persists.
    ///
    /// Deliberately SEPARATE from [`Self::max_result_size_chars`]: the port
    /// returns `30_000` there for `Read`/`Grep`/`Glob`/`Edit`, but a census of
    /// every persisted result the real binary has written on this machine
    /// (1 324 of them, across 3 271 transcripts) found **Bash and nothing
    /// else** — reusing that value would persist output the oracle never does.
    ///
    /// This is the RAW declared value; the orchestrator folds it against
    /// [`Self::persistence_threshold_ceiling`] exactly as `M0u` does
    /// (`Math.min(maxResultSizeChars, ceiling ?? 50000)`).
    fn persistence_threshold(&self) -> Option<usize> {
        None
    }

    /// `persistenceThresholdCeiling` — the per-tool cap `M0u` folds
    /// [`Self::persistence_threshold`] against. `None` selects the oracle's
    /// `AKr = 50000` default (2.1.220 BIN off **230268660**); the MCP tool
    /// factory (BIN off 232139111) is the only builtin that raises it, to
    /// `gor = 500000`.
    fn persistence_threshold_ceiling(&self) -> Option<usize> {
        None
    }

    /// Whether the tool is safe to run concurrently with other tools given
    /// this specific input. Read-only tools typically return `true`.
    fn is_concurrency_safe(&self, input: &Value) -> bool;

    /// Whether this invocation is read-only with respect to the workspace.
    fn is_read_only(&self, input: &Value) -> bool;

    /// Whether this invocation can destroy data irreversibly.
    fn is_destructive(&self, _input: &Value) -> bool {
        false
    }

    /// Whether the tool reaches outside the local workspace (network, etc.).
    fn is_open_world(&self, _input: &Value) -> bool {
        false
    }

    /// Classify this invocation as search / read / list, if applicable.
    /// Used by the loader to fold "Search&Read" tools into one slot.
    fn is_search_or_read(&self, _input: &Value) -> Option<SearchReadInfo> {
        None
    }

    /// How the tool should react when an in-flight call is interrupted.
    fn interrupt_behavior(&self, _input: &Value) -> InterruptBehavior {
        InterruptBehavior::Block
    }

    /// Normalize the input to a form suitable for replay / observability.
    ///
    /// Tools may strip secrets, redact paths, or canonicalize ordering.
    fn backfill_observable_input(&self, _input: &mut Value) {}

    /// Normalize a model-supplied input BEFORE JSON-schema validation — the
    /// port of claude-code's `Tool.coerceInput(input)`.
    ///
    /// The oracle threads it through the shared helper
    /// `K7(tool, input) = tool.inputSchema.safeParse(tool.coerceInput?.(input)?.input ?? input)`
    /// (2.1.238 BIN off **284321584**) and, on the execution path, inline at the
    /// top of `checkPermissionsAndCallTool` (BIN off **294282716**):
    ///
    /// ```js
    /// let h=r,g=null;
    /// if(e.coerceInput){ if(g=e.coerceInput(r), g!==null) h=g.input }
    /// let y=e.inputSchema.safeParse(h);
    /// ```
    ///
    /// So a coercion that fires REPLACES the input for schema validation,
    /// `validateInput`, the hooks and `call` alike; `None` (the oracle's `null`)
    /// leaves the raw input untouched. Implementations must be PURE and return
    /// `None` when they changed nothing — the oracle's own
    /// `return r.length ? {input:t, shapeClass:r.join(",")} : null` shape, which
    /// deliberately DISCARDS a partially-rewritten copy when no key was actually
    /// coerced (see [`crate::tool_trait::CoercedInput`]).
    ///
    /// Defaults to `None` so every existing `Tool` impl is unaffected
    /// (frozen-trait rule).
    fn coerce_input(&self, _input: &Value) -> Option<CoercedInput> {
        None
    }

    /// Validate the input before any side-effects.
    ///
    /// # Errors
    /// Returns [`ValidationError`] if the input fails tool-specific checks
    /// that go beyond the static JSON schema (e.g. path exists, regex
    /// compiles).
    async fn validate_input(
        &self,
        _input: &Value,
        _ctx: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        Ok(())
    }

    /// Compute the permission decision for this `(input, ctx)` pair.
    ///
    /// The dispatcher calls this before invoking [`Tool::call`]. Hooks may
    /// override the result (e.g. allow / deny via `PreToolUse`).
    async fn check_permissions(&self, input: &Value, ctx: &ToolUseContext) -> PermissionResult;

    /// Filesystem path this invocation primarily targets, if any.
    ///
    /// Used by the permission engine and file-state cache.
    fn get_path(&self, _input: &Value) -> Option<PathBuf> {
        None
    }

    /// Human-readable description of what this invocation will do.
    async fn description(&self, input: &Value, opts: &DescriptionOptions) -> String;

    /// Long-form tool prompt used when assembling the system prompt.
    async fn prompt(&self, opts: &PromptOptions) -> String;

    /// Execute the tool.
    ///
    /// # Errors
    /// Returns [`ToolError`] on permission denial, validation failure,
    /// I/O issues, abort, or file-state cache violations.
    async fn call(
        &self,
        input: Value,
        ctx: ToolUseContext,
        progress_tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError>;

    /// Short verb phrase for activity displays
    /// (e.g. `"Reading file"`, `"Running command"`).
    fn get_activity_description(&self, _input: &Value) -> Option<String> {
        None
    }

    /// User-facing display name for this `(input)` pair, overriding [`name`] in
    /// the UI when the tool wants a per-invocation label.
    ///
    /// Port of claude-code's `Tool.userFacingName(input)` (the `AgentTool`
    /// override lives at `UI.tsx:760-775` — it shows the subagent type, e.g.
    /// `"Explore"`, instead of the generic `"Agent"`). Defaults to `None` so
    /// every other `Tool` impl is unaffected (frozen-trait rule); a `None` means
    /// "use [`name`]".
    ///
    /// Distinct from the input-less [`user_facing_name`](Self::user_facing_name)
    /// (a static label used by tools like `StopTask`/`TaskOutput`); this
    /// per-invocation variant takes the call `input` so `AgentTool` can show the
    /// requested subagent type.
    ///
    /// [`name`]: Tool::name
    fn user_facing_name_for_input(&self, _input: &Value) -> Option<String> {
        None
    }

    /// Background color (a theme-color key / name string) for this tool's
    /// user-facing name badge, if any.
    ///
    /// Port of claude-code's `Tool.userFacingNameBackgroundColor(input)` (the
    /// `AgentTool` override lives at `UI.tsx:776-787` — it returns the agent's
    /// configured color via `getAgentColor(subagent_type)`). claude returns a
    /// `keyof Theme`; the Rust analog returns the color name string. Defaults to
    /// `None` so every other `Tool` impl is unaffected (frozen-trait rule).
    fn user_facing_name_background_color(&self, _input: &Value) -> Option<String> {
        None
    }
}

/// Static context passed to [`Tool::is_enabled`]: feature flags and other
/// host-level toggles that do not depend on a specific invocation.
#[derive(Debug, Clone, Default)]
pub struct ToolStaticContext {
    /// Boolean feature flags keyed by name.
    pub feature_flags: std::collections::HashMap<String, bool>,
    /// The session's canonical main-loop model id (claude-code `J$e()` =
    /// `mainLoopCanonical()`), or `None` when it is not known.
    ///
    /// Callers do not normally fill this: [`crate::ToolRegistry::available_tools`]
    /// supplies it from the registry's session-scoped model
    /// ([`crate::ToolRegistry::set_main_loop_model`]) whenever the incoming
    /// context leaves it empty, so every advertise path — main loop, subagent
    /// pool, Tool Search view — sees the same answer. That is deliberately the
    /// **session** model and never the calling agent's, matching the oracle.
    ///
    /// It must already be canonical (alias-resolved, `[1m]` stripped): the
    /// `OO()` gate in [`crate::todo_tools_gate`] matches it against the
    /// oracle's literal `^claude-([a-z]+)-(\d+(?:-\d+)*)$`, and a bare alias
    /// like `opus` fails that regex and silently leaves the gate open.
    pub main_loop_model: Option<String>,
}

/// Options controlling [`Tool::description`] output.
#[derive(Debug, Clone)]
pub struct DescriptionOptions {
    /// Whether the agent is running in a non-interactive session (CI, batch).
    pub is_non_interactive_session: bool,
}

/// Options controlling [`Tool::prompt`] output.
#[derive(Debug, Clone, Default)]
pub struct PromptOptions {
    /// Whether to include illustrative examples in the prompt.
    pub include_examples: bool,
    /// The session (or subagent) model id the wire `tools` array is being
    /// built for. claude-code calls `prompt({model})` per tool, and some
    /// prompts are model-gated — TodoWrite selects the short `FWd` vs. long
    /// `UWd` variant via the `Dh(model)` "simple system prompt" gate
    /// (`Xla(e)=Dh(e)?FWd:UWd`). `None` mirrors the binary's `Dh(undefined)`
    /// (returns `false` → the long prompt). Thread the real model from the
    /// turn-loop / subagent build sites so new models (e.g. `claude-opus-4-8`,
    /// which is outside `UWu`'s classic list) correctly pick `FWd`.
    pub model: Option<String>,
    /// Active provider profile for [`Self::model`] (e.g. `github-copilot`).
    /// Tool prompts that need provider-specific wording should use this instead
    /// of request-builder internals.
    pub model_profile: Option<String>,
}

/// Information returned by [`Tool::is_search_or_read`] describing how an
/// invocation behaves with respect to the workspace.
#[derive(Debug, Clone)]
pub struct SearchReadInfo {
    /// True if this invocation searches across files (grep-like).
    pub is_search: bool,
    /// True if this invocation reads a specific file or resource.
    pub is_read: bool,
    /// True if this invocation lists directory contents.
    pub is_list: bool,
}

/// A successful [`Tool::coerce_input`] rewrite — the port of the object
/// claude-code's `coerceInput` returns (`{input, shapeClass}`).
///
/// `shape_class` is the oracle's comma-joined list of the keys that were
/// coerced (e.g. `"timeout_ms"`, `"path,old_str"`). The oracle uses it only as
/// the `shapeClass` dimension of its `tengu_tool_input_coerced` analytics event;
/// it never reaches the model, so nothing on the wire depends on it. The port
/// keeps the field so the value is available to a future analytics wiring rather
/// than being recomputed.
#[derive(Debug, Clone)]
pub struct CoercedInput {
    /// The rewritten input the dispatcher must use from here on.
    pub input: Value,
    /// Comma-joined list of coerced keys, in the order the tool applied them.
    pub shape_class: String,
}

/// How a tool should react when an in-flight call is interrupted.
#[derive(Debug, Clone, Copy)]
pub enum InterruptBehavior {
    /// Cancel the in-flight call and return immediately.
    Cancel,
    /// Block until the in-flight call finishes naturally.
    Block,
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

/// Extra metadata for a successful tool result that ends the current turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolResultTurnEnd {
    /// Marker that won the oracle's `tool`-before-`mcp_meta` precedence.
    pub source: ToolResultTurnEndSource,
}

/// Classify whether a successful tool result requests turn termination.
///
/// This is the exact `qbt` precedence from Claude Code 2.1.252:
/// `toolEndsTurn` wins over MCP metadata, and an error result never ends the
/// turn even when either marker is present.
#[must_use]
pub fn tool_result_turn_end(
    tool_ends_turn: bool,
    is_error: bool,
    mcp_meta: Option<&serde_json::Value>,
) -> Option<ToolResultTurnEnd> {
    if is_error {
        return None;
    }
    if tool_ends_turn {
        return Some(ToolResultTurnEnd {
            source: ToolResultTurnEndSource::Tool,
        });
    }
    mcp_meta
        .and_then(serde_json::Value::as_object)
        .and_then(|meta| meta.get("_meta"))
        .and_then(serde_json::Value::as_object)
        .and_then(|meta| meta.get("claude/endTurn"))
        .and_then(serde_json::Value::as_bool)
        .filter(|value| *value)
        .map(|_| ToolResultTurnEnd {
            source: ToolResultTurnEndSource::McpMeta,
        })
}

/// A one-shot mutator a tool returns to adjust the turn's [`ToolUseContext`] —
/// the Rust twin of claude-code's `ToolResult.contextModifier`.
///
/// The turn loop folds a tool batch's modifiers POST-DISPATCH over a seed
/// context and reads the result back (SKILLEXEC.3, model scope: a Skill tool's
/// `model:` frontmatter switches the session's main-loop model for the rest of
/// the session). `Send` so the streaming concurrent dispatch can collect them
/// across `.await` points.
pub type ContextModifier = Box<dyn FnOnce(ToolUseContext) -> ToolUseContext + Send>;

/// Result of a successful [`Tool::call`].
///
/// `data` is the JSON payload returned to the model. `new_messages` carries
/// any extra conversation messages the tool wants to inject (rare).
/// `context_modifier` lets a tool mutate the [`ToolUseContext`] for
/// subsequent calls (e.g. record a side-effect). `mcp_meta` is opaque
/// metadata threaded through for MCP tools.
pub struct ToolCallResult {
    /// JSON payload returned to the model.
    pub data: Value,
    /// Optional faithful model-facing string. When `Some`, the dispatch uses
    /// this verbatim as the tool's model text (and the SDK frame's
    /// `tool_result.content`), keeping `data` as pure metadata. When `None`,
    /// the dispatch falls back to deriving the model text out of `data`.
    pub model_content: Option<String>,
    /// Extra conversation messages to inject after this call.
    pub new_messages: Vec<protocol::ConversationMessage>,
    /// Optional one-shot mutator for the [`ToolUseContext`].
    pub context_modifier: Option<ContextModifier>,
    /// Opaque per-call metadata (used by MCP tools).
    pub mcp_meta: Option<serde_json::Value>,
    /// Whether this result is an ERROR — drives the `tool_result` block's
    /// `is_error` flag. `false` for every native tool's success path (a native
    /// failure surfaces as an `Err`, which the dispatch flags separately); MCP
    /// tools set it from the server's `isError` so an MCP error RESULT (a logical
    /// failure, not a transport error) is flagged to the model 1:1 with claude-code.
    pub is_error: bool,
}

impl ToolCallResult {
    /// Build a result from a `data` payload only — the model text is derived
    /// from `data` by the dispatch (`model_content` stays `None`).
    pub fn from_data(data: Value) -> Self {
        Self {
            data,
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        }
    }
}

impl std::fmt::Debug for ToolCallResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `context_modifier` is a boxed `FnOnce` and cannot be `Debug`-printed.
        f.debug_struct("ToolCallResult")
            .field("data", &self.data)
            .field("model_content", &self.model_content)
            .field("new_messages", &self.new_messages)
            .field("mcp_meta", &self.mcp_meta)
            .field("is_error", &self.is_error)
            .finish_non_exhaustive()
    }
}

/// Error returned by [`Tool::validate_input`].
#[derive(Debug, Clone, Error)]
#[error("invalid tool input: {0}")]
pub struct ValidationError(
    /// Human-readable validation error message.
    pub String,
);

/// Error returned by [`Tool::call`] and related dispatcher operations.
#[derive(Debug, Clone, Error)]
#[non_exhaustive]
pub enum ToolError {
    /// No tool registered under the requested name.
    #[error("tool not found: {0}")]
    NotFound(String),
    /// Input failed validation (schema or tool-specific checks).
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// Permission engine denied the call.
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    /// Underlying I/O failure (network, disk, etc.).
    #[error("io: {0}")]
    Io(String),
    /// The call was aborted (interrupt, cancellation).
    #[error("aborted")]
    Aborted,
    /// Internal error: tool implementation bug or unexpected state.
    #[error("internal: {0}")]
    Internal(String),
    /// The call requires a live user interaction channel that this host does
    /// not provide. This is distinct from denial: no user decision was made.
    #[error("interaction required: {0}")]
    InteractionRequired(String),
    /// Edit attempted before the file was read in this session.
    /// Wired in Plan 10 via the file-state cache.
    #[error("edit before read: {path}")]
    EditWithoutRead {
        /// Path of the file the agent tried to edit.
        path: String,
    },
    /// File changed on disk after the agent last read it.
    /// Wired in Plan 10 via the file-state cache.
    #[error("file modified externally since last read: {path}")]
    FileModifiedExternally {
        /// Path of the file that changed externally.
        path: String,
    },
    /// The cached view of this file is partial; the agent must re-read it.
    /// Wired in Plan 10 via the file-state cache.
    #[error("partial view — please re-read {path}")]
    PartialViewMustReread {
        /// Path that the agent must re-read in full.
        path: String,
    },
    /// File content hash does not match the cached value.
    /// Wired in Plan 10 via the file-state cache.
    #[error("file content hash mismatch: {path}")]
    FileContentMismatch {
        /// Path of the file whose hash mismatched.
        path: String,
    },
    /// M4-01: file exceeds the size limit configured for the read.
    #[error("file too large: {size} bytes exceeds limit {limit}")]
    FileTooLarge {
        /// Actual size encountered, in bytes.
        size: u64,
        /// Configured maximum, in bytes.
        limit: u64,
    },
    /// M4-01: requested path resolves outside the trusted-dirs whitelist.
    #[error("path not in trusted directory: {path:?}")]
    PathBlocked {
        /// The canonicalised path that was rejected.
        path: std::path::PathBuf,
    },
    /// M4-01: file looks like a binary blob (NUL bytes in the head window).
    #[error("binary file detected at {path:?} ({reason})")]
    BinaryFile {
        /// Path of the file that was rejected.
        path: std::path::PathBuf,
        /// Short reason ("NUL byte at offset N").
        reason: &'static str,
    },
    /// M4-02: subprocess hit the watchdog (placeholder variant; emitter lives in M4-02).
    #[error("command timed out after {timeout_ms}ms")]
    Timeout {
        /// Configured watchdog, in milliseconds.
        timeout_ms: u64,
    },
    /// M4-01..M4-03: tool output exceeded `MAX_TOOL_OUTPUT_LENGTH`.
    #[error("output truncated at {limit} chars")]
    OutputTruncated {
        /// Char limit that was hit.
        limit: usize,
    },
    /// M4-05: subagent loop crashed.
    #[error("subagent failed: {0}")]
    SubagentFailed(String),
    /// M4-07: MCP server reported a tool-call failure.
    #[error("MCP tool error: {server}/{tool}: {detail}")]
    McpFailure {
        /// MCP server identifier.
        server: String,
        /// MCP tool name.
        tool: String,
        /// Server-supplied detail string.
        detail: String,
    },
    /// M4-07: LSP request failed.
    #[error("LSP tool error: {0}")]
    LspFailure(String),
    /// M4-03: HTTP transport error (DNS, connect, TLS, status code).
    #[error("transport error: {0}")]
    Transport(String),
}

impl ToolError {
    /// The BARE, model-facing message — the string claude-code would have
    /// thrown as `error.message` and rendered verbatim into the `tool_result`
    /// content via `formatError` (`utils/toolErrors.ts` returns `error.message`
    /// unmodified; `services/tools/toolExecution.ts:1691` feeds it raw into the
    /// `is_error` tool_result block).
    ///
    /// This is DISTINCT from [`std::fmt::Display`] (used for internal logging),
    /// which prepends a per-variant prefix (`invalid input: `, `internal: `,
    /// …). Those prefixes are a LingXi-internal convenience and must NOT leak
    /// into the wire bytes the model sees — claude emits only the inner message
    /// (e.g. `Agent type 'x' not found. Available agents: …`).
    ///
    /// For the message-carrying variants, the inner string IS exactly what the
    /// equivalent claude-code tool throws, so we return it bare. The structured
    /// variants (file-state, size, path, timeout, …) keep their full `Display`
    /// rendering — that text IS the message claude would surface, and there is
    /// no spurious prefix to strip.
    #[must_use]
    pub fn model_facing_message(&self) -> String {
        match self {
            // Single-string variants whose `Display` prepends a non-meaningful
            // prefix: the inner string is the claude `error.message`.
            ToolError::NotFound(s)
            | ToolError::InvalidInput(s)
            | ToolError::PermissionDenied(s)
            | ToolError::Io(s)
            | ToolError::Internal(s)
            | ToolError::InteractionRequired(s)
            | ToolError::SubagentFailed(s)
            | ToolError::LspFailure(s)
            | ToolError::Transport(s) => s.clone(),
            // Structured / prefix-free variants: `Display` already renders the
            // exact model-facing message (no LingXi-only prefix to strip).
            other => other.to_string(),
        }
    }
}

#[cfg(test)]
mod m4_01_error_variant_tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn file_too_large_display_format() {
        let e = ToolError::FileTooLarge {
            size: 300_000,
            limit: 262_144,
        };
        assert_eq!(
            e.to_string(),
            "file too large: 300000 bytes exceeds limit 262144"
        );
    }

    #[test]
    fn path_blocked_display_format() {
        let e = ToolError::PathBlocked {
            path: PathBuf::from("/etc/passwd"),
        };
        assert_eq!(
            e.to_string(),
            r#"path not in trusted directory: "/etc/passwd""#
        );
    }

    #[test]
    fn binary_file_display_format() {
        let e = ToolError::BinaryFile {
            path: PathBuf::from("/tmp/x.bin"),
            reason: "NUL byte at offset 0",
        };
        assert_eq!(
            e.to_string(),
            r#"binary file detected at "/tmp/x.bin" (NUL byte at offset 0)"#
        );
    }

    #[test]
    fn timeout_display_format() {
        let e = ToolError::Timeout { timeout_ms: 5000 };
        assert_eq!(e.to_string(), "command timed out after 5000ms");
    }

    #[test]
    fn output_truncated_display_format() {
        let e = ToolError::OutputTruncated { limit: 30_000 };
        assert_eq!(e.to_string(), "output truncated at 30000 chars");
    }

    /// `model_facing_message()` returns the BARE inner message (claude's
    /// `error.message`) — NO `invalid input: ` / `internal: ` Display prefix.
    /// claude renders this verbatim into the `is_error` tool_result content
    /// (`formatError` → `toolExecution.ts:1691`); the prefix is logging-only.
    #[test]
    fn model_facing_message_strips_variant_prefix() {
        // The two prefixes the AgentTool errors actually use.
        let invalid = ToolError::InvalidInput(
            "Agent type 'x' not found. Available agents: general-purpose".into(),
        );
        assert_eq!(
            invalid.model_facing_message(),
            "Agent type 'x' not found. Available agents: general-purpose"
        );
        // Display keeps the prefix (logging path) — the divergence the fix targets.
        assert_eq!(
            invalid.to_string(),
            "invalid input: Agent type 'x' not found. Available agents: general-purpose"
        );

        let internal = ToolError::Internal(
            "Agent 'x' requires MCP servers matching: github. MCP servers with tools: none.".into(),
        );
        assert_eq!(
            internal.model_facing_message(),
            "Agent 'x' requires MCP servers matching: github. MCP servers with tools: none."
        );

        // Other single-string variants are bare too.
        assert_eq!(
            ToolError::SubagentFailed("boom".into()).model_facing_message(),
            "boom"
        );

        // Structured variants keep their full Display (no spurious prefix).
        let timeout = ToolError::Timeout { timeout_ms: 5000 };
        assert_eq!(
            timeout.model_facing_message(),
            "command timed out after 5000ms"
        );
    }
}

#[cfg(test)]
mod user_facing_name_default_tests {
    use super::*;
    use crate::progress::ToolProgressSender;

    /// A minimal `Tool` that overrides nothing — exercises the defaulted
    /// `user_facing_name` / `user_facing_name_background_color` (G9 frozen-trait
    /// rule: every non-`AgentTool` tool sees `None`).
    struct BareTool {
        schema: Value,
    }

    #[async_trait]
    impl Tool for BareTool {
        fn name(&self) -> &str {
            "Bare"
        }
        fn input_schema(&self) -> &Value {
            &self.schema
        }
        fn is_enabled(&self, _: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            0
        }
        fn is_concurrency_safe(&self, _: &Value) -> bool {
            true
        }
        fn is_read_only(&self, _: &Value) -> bool {
            true
        }
        async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
            unimplemented!("not exercised")
        }
        async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
            String::new()
        }
        async fn prompt(&self, _: &PromptOptions) -> String {
            String::new()
        }
        async fn call(
            &self,
            _: Value,
            _: ToolUseContext,
            _: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            unimplemented!("not exercised")
        }
    }

    #[test]
    fn user_facing_name_methods_default_to_none() {
        let t = BareTool {
            schema: serde_json::json!({"type": "object"}),
        };
        let input = serde_json::json!({});
        assert_eq!(t.user_facing_name_for_input(&input), None);
        assert_eq!(t.user_facing_name_background_color(&input), None);
    }

    /// BASH-18: the new `coerce_input` seam is OPT-IN — a tool that does not
    /// override it returns `None`, which the dispatcher reads as "use the raw
    /// input" (oracle `e.coerceInput?.(t) ?? null`).
    #[test]
    fn coerce_input_defaults_to_none() {
        let t = BareTool {
            schema: serde_json::json!({"type": "object"}),
        };
        assert!(t
            .coerce_input(&serde_json::json!({ "timeout_ms": 5000 }))
            .is_none());
    }

    #[test]
    fn tool_result_turn_end_matches_tool_mcp_and_error_precedence() {
        let detected = tool_result_turn_end(
            false,
            false,
            Some(&serde_json::json!({
                "_meta": { "claude/endTurn": true }
            })),
        );
        assert_eq!(
            detected,
            Some(ToolResultTurnEnd {
                source: ToolResultTurnEndSource::McpMeta,
            })
        );
        assert_eq!(
            tool_result_turn_end(
                true,
                false,
                Some(&serde_json::json!({ "_meta": { "claude/endTurn": true } })),
            ),
            Some(ToolResultTurnEnd {
                source: ToolResultTurnEndSource::Tool,
            })
        );
        assert!(tool_result_turn_end(
            false,
            false,
            Some(&serde_json::json!({
                "_meta": { "claude/endTurn": false }
            }))
        )
        .is_none());
        assert!(tool_result_turn_end(
            false,
            false,
            Some(&serde_json::json!({
                "_meta": { "vendor/endTurn": true }
            }))
        )
        .is_none());
        assert!(tool_result_turn_end(true, true, None).is_none());
        assert!(tool_result_turn_end(
            false,
            true,
            Some(&serde_json::json!({ "_meta": { "claude/endTurn": true } })),
        )
        .is_none());
    }
}
