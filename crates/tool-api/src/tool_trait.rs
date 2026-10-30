//! `Tool` trait — the contract every tool implements.
//!
//! Static methods reason about the tool itself (name, schemas, capabilities);
//! dynamic methods reason about a specific `(input, ctx)` pair (permission,
//! concurrency safety, validation, execution).

use async_trait::async_trait;
use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonBuilder, Utf16JsonProjection};
use permission::PermissionResult;
use serde_json::Value;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;

use crate::context::ToolUseContext;
use crate::progress::ToolProgressSender;

/// A host resource that a media-backed tool invocation needs exclusively.
///
/// Native realtime owns the microphone and speaker for the lifetime of its
/// session, while camera operations can remain available alongside it. Keeping
/// the claims on the registered tool lets callers apply the same conflict
/// policy without relying on model-facing tool names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaResource {
    /// Live microphone capture or input.
    Microphone,
    /// Camera or photo-library capture/observation.
    Camera,
    /// Live audio playback.
    Speaker,
}

/// Narrow execution-time metadata used to arbitrate live media resources.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolExecutionMetadata {
    /// Exclusive resources required by this specific invocation.
    pub exclusive_media_resources: Vec<MediaResource>,
}

impl ToolExecutionMetadata {
    /// Whether this invocation conflicts with an already-held media resource.
    #[must_use]
    pub fn conflicts_with(&self, held: &[MediaResource]) -> bool {
        self.exclusive_media_resources
            .iter()
            .any(|required| held.contains(required))
    }
}

/// The four source facts Native `Mx` checks before exposing a wrapped V1
/// tool through `$.tool.call`. A wrapper is hidden only when the tool declares
/// every fact with the matching Native type.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NativeModToolBatchWrapperFacts<'a> {
    /// Defined string field `underlyingV1ToolName`.
    pub underlying_v1_tool_name: Option<&'a str>,
    /// Defined string field `entryFieldName`.
    pub entry_field_name: Option<&'a str>,
    /// Whether `perEntryHookInputs` is a function.
    pub per_entry_hook_inputs_is_function: bool,
    /// Whether `reassemble` is a function.
    pub reassemble_is_function: bool,
}

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

    /// Host-derived MCP provenance, never reconstructed from an FQN or alias.
    fn mcp_tool_identity(&self) -> Option<&crate::McpToolIdentity> {
        None
    }

    /// Bind a restricted invocation lease to transport-backed tools.
    /// Ordinary builtin dispatch is checked by the registry and invoker.
    fn bind_session_tool_lease(&self, _lease: crate::SessionToolGrantLease) {}

    /// Alternate names the dispatcher should accept for this tool.
    ///
    /// Defaults to no aliases.
    fn aliases(&self) -> &[&str] {
        &[]
    }

    /// Exact wrapper-shape facts consumed by Native `$.tool.call` catalog
    /// filtering. Ordinary tools must return the default (all missing) value;
    /// the unrelated [`Self::underlying_v1_tool_name`] pool-matching field is
    /// not enough to identify a batch wrapper.
    fn native_mod_tool_batch_wrapper_facts(&self) -> NativeModToolBatchWrapperFacts<'_> {
        NativeModToolBatchWrapperFacts::default()
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
    fn search_hint(&self) -> Option<Utf16JsonProjection> {
        None
    }

    /// Human-readable label shown to the user for this tool (claude-code
    /// `userFacingName()`), distinct from the canonical wire [`name`](Self::name).
    /// `None` means "use the canonical name" — claude-code falls back to the tool
    /// name when no override is supplied.
    fn user_facing_name(&self) -> Option<&str> {
        None
    }

    /// Source-owned JSON Schema for the tool's input parameters.
    fn input_schema(&self) -> &Utf16JsonProjection;

    /// Owned schema snapshot for capabilities that can change while a
    /// registry is live. The wire builder calls this for each model request,
    /// so a capability update cannot leave a stale action list in the schema
    /// cache or ToolSearch view. Static tools keep the borrowed schema above.
    fn input_schema_snapshot(&self) -> Option<Utf16JsonProjection> {
        None
    }

    /// Owned schema snapshot for the exact request being built. Tools whose
    /// schema depends on the selected session route can use the same trusted
    /// model/profile snapshot as their prompt without reading live session
    /// state synchronously.
    fn input_schema_snapshot_for_prompt(
        &self,
        _options: &PromptOptions,
    ) -> Option<Utf16JsonProjection> {
        self.input_schema_snapshot()
    }

    /// Authoritative source facts used by the pre-row streamed-input repair.
    /// `Unknown` is not equivalent to an absent schema or an absent exemption
    /// callback. Production registration and dispatch must reject it instead
    /// of silently skipping native input repair.
    fn input_repair_metadata(&self) -> ToolInputRepairMetadata;

    /// Apply Native's tool-specific streamed-row normalization after the
    /// schema-directed and Unicode repair stages. Native only has this switch
    /// for Read, Skill, Bash, Edit, and Write; every other tool keeps its input
    /// unchanged at this phase. This is deliberately separate from the later
    /// local Zod/native validation path.
    fn prepare_input_before_row(
        &self,
        input: &Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ToolError> {
        Ok(input.clone())
    }

    /// Revision token for a live schema snapshot. Include every non-content
    /// version component that changes its shape (for example a device service
    /// epoch and supported-operation revision) so per-session wire caches are
    /// invalidated before reusing serialized schemas.
    fn input_schema_revision(&self) -> Option<String> {
        None
    }

    /// Revision corresponding to [`Self::input_schema_snapshot_for_prompt`].
    /// The default preserves the static/live revision behavior for existing
    /// tools.
    fn input_schema_revision_for_prompt(&self, _options: &PromptOptions) -> Option<String> {
        self.input_schema_revision()
    }

    /// Provider-native desktop adaptation for this registered tool, when supported.
    fn native_computer_capabilities(
        &self,
    ) -> Option<lingxi_llm_client::protocol::computer::ComputerCapabilities> {
        None
    }

    /// Convert protocol data into this tool's ordinary input without executing it.
    fn lower_computer_operation(
        &self,
        _operation: &lingxi_llm_client::protocol::computer::ComputerOperation,
        _frame: &lingxi_llm_client::protocol::computer::ComputerFrame,
    ) -> Result<Value, ToolError> {
        Err(ToolError::InvalidInput(
            "native computer adaptation unavailable".into(),
        ))
    }

    /// Return an authorized, current observation belonging to the calling owner.
    async fn native_computer_frame(
        &self,
        _ctx: &ToolUseContext,
    ) -> Result<Option<lingxi_llm_client::protocol::computer::ComputerFrame>, ToolError> {
        Ok(None)
    }

    /// Retain desktop ownership across an ordered native sequence and its observation.
    async fn begin_computer_sequence(&self, _ctx: &ToolUseContext) -> Result<(), ToolError> {
        Ok(())
    }

    /// End a sequence while retaining ownership of inputs still held by the owner.
    async fn end_computer_sequence(&self, _ctx: &ToolUseContext) -> Result<(), ToolError> {
        Ok(())
    }

    /// Release inputs held by this owner at cancellation or lifecycle completion.
    async fn cleanup_computer_inputs(&self, _ctx: &ToolUseContext) -> Result<(), ToolError> {
        Ok(())
    }

    /// Apply the final model-visible observation after output hooks and Mods.
    async fn computer_model_output(
        &self,
        _ctx: &ToolUseContext,
        _content: &Utf16JsonProjection,
        _blocks: Option<&[Utf16JsonProjection]>,
    ) -> Result<(), ToolError> {
        Ok(())
    }

    /// Require a fresh observation after recovery or a host geometry reset.
    async fn invalidate_computer_observation(
        &self,
        _ctx: &ToolUseContext,
    ) -> Result<(), ToolError> {
        Ok(())
    }

    /// Runtime parser schema, distinct from advertised foreign JSON Schema.
    /// MCP server schemas are inputJSONSchema on the wire; their local parser
    /// is the passthrough object inherited from H4 (Claude Code 2.1.263).
    fn input_validation_schema(&self) -> &Utf16JsonProjection {
        self.input_schema()
    }

    /// Native Zod-v4 issues for checks absent from exported JSON Schema.
    /// Preserve declaration order and object key insertion order: the fallback
    /// diagnostic is ZodError.message (JSON.stringify(issues, null, 2)).
    /// Return only refinement/custom checks; structural validation runs first.
    /// Validators must skip refinements on inputs whose base types cannot parse.
    fn input_validation_issues(&self, _input: &Utf16JsonProjection) -> Vec<Utf16JsonProjection> {
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
        input: &Utf16JsonProjection,
    ) -> Option<Result<Utf16JsonProjection, crate::native_schema::NativeSchemaError>> {
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
    fn output_schema(&self) -> Option<&Utf16JsonProjection> {
        None
    }

    /// Parse a tool result through the tool's Native output parser, returning
    /// its parsed value (the equivalent of Zod `safeParse(...).data`). Tools
    /// without an output schema keep their input unchanged. A schema-bearing
    /// tool must provide its source-native parser; it must not silently treat
    /// the schema as an Ajv profile or accept the unparsed input.
    async fn parse_output(
        &self,
        output: &Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ToolError> {
        if self.output_schema().is_none() {
            Ok(output.clone())
        } else {
            Err(ToolError::InvalidInput(
                format!(
                    "Native output parser is not mapped for schema-bearing tool '{}'",
                    self.name()
                )
                .into(),
            ))
        }
    }

    /// Model-facing text for a Mod-supplied result. Returning `None` selects
    /// the shared generic result mapper. This must not execute the tool body.
    fn map_result_text(&self, _result: &Utf16JsonProjection) -> Option<Utf16JsonProjection> {
        None
    }

    /// Error bit produced by a tool-specific result mapper. For example,
    /// Bash marks an interrupted result as an error even when it has output.
    fn map_result_is_error(&self, _result: &Utf16JsonProjection) -> Option<bool> {
        None
    }

    /// Whether this tool is enabled for the given static context
    /// (feature flags, environment, etc.).
    fn is_enabled(&self, ctx: &ToolStaticContext) -> bool;

    /// Whether this tool is sourced from an MCP server.
    fn is_mcp(&self) -> bool {
        false
    }

    /// Trusted per-call ceiling projected on the Native Mod `tool.check`
    /// surface. Implementations return a value only when they have an
    /// authoritative host fact for this exact input; the dispatcher does not
    /// infer organization policy from `is_mcp`, tool names, or payload fields.
    async fn tool_check_permission_ceiling(
        &self,
        _input: &Utf16JsonProjection,
    ) -> Option<lingxi_core::host::McpPermissionCeiling> {
        None
    }

    /// Observe a static JSON-schema rejection before the dispatcher returns it
    /// to the model. Implementations may use this for rejection telemetry; the
    /// default deliberately has no side effects.
    async fn on_input_schema_rejected(
        &self,
        _input: &Utf16JsonProjection,
        _tool_use_id: Option<&str>,
        _assistant_message_id: Option<&lingxi_core::types::MessageId>,
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

    /// Require the bound Auto classifier even when rules or hooks allow the
    /// call. Policy is host-owned tool metadata, never model arguments.
    fn classifier_only(&self) -> Option<lingxi_core::host::permission_gate::ClassifierOnlyPolicy> {
        None
    }

    /// Action reviewed by the classifier-only path after native input parsing.
    fn classifier_only_action(&self, input: &Utf16JsonProjection) -> String {
        input
            .to_json_string()
            .expect("validated source input serializes as JSON")
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
    fn is_concurrency_safe(&self, input: &Utf16JsonProjection) -> bool;

    /// Resources required by this invocation, derived from trusted tool input.
    ///
    /// This is deliberately limited to host media arbitration. Tools without
    /// native media requirements keep the empty default; media tools should
    /// conservatively report every resource selected by their typed input.
    fn execution_metadata(&self, _input: &Utf16JsonProjection) -> ToolExecutionMetadata {
        ToolExecutionMetadata::default()
    }

    /// Input-independent media resources required by every valid invocation.
    ///
    /// Catalog builders use this conservative common requirement before a
    /// model has chosen an input. Multi-mode tools should return only resources
    /// shared by all valid modes, then report the selected resources from
    /// [`Self::execution_metadata`] at dispatch time.
    fn catalog_execution_metadata(&self) -> ToolExecutionMetadata {
        ToolExecutionMetadata::default()
    }

    /// Whether this invocation is read-only with respect to the workspace.
    fn is_read_only(&self, input: &Utf16JsonProjection) -> bool;

    /// Whether this invocation can destroy data irreversibly.
    fn is_destructive(&self, _input: &Utf16JsonProjection) -> bool {
        false
    }

    /// Whether the tool reaches outside the local workspace (network, etc.).
    fn is_open_world(&self, _input: &Utf16JsonProjection) -> bool {
        false
    }

    /// Classify this invocation as search / read / list, if applicable.
    /// Used by the loader to fold "Search&Read" tools into one slot.
    fn is_search_or_read(&self, _input: &Utf16JsonProjection) -> Option<SearchReadInfo> {
        None
    }

    /// How the tool should react when an in-flight call is interrupted.
    fn interrupt_behavior(&self, _input: &Utf16JsonProjection) -> InterruptBehavior {
        InterruptBehavior::Block
    }

    /// Whether an interruptible invocation handles cancellation itself and
    /// returns only after bounded cleanup or an explicit uncertain outcome.
    fn await_cleanup_on_interrupt(&self) -> bool {
        false
    }

    /// Normalize the input to a form suitable for replay / observability.
    ///
    /// Tools may strip secrets, redact paths, or canonicalize ordering.
    fn backfill_observable_input(&self, _input: &mut Utf16JsonProjection) {}

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
    fn coerce_input(&self, _input: &Utf16JsonProjection) -> Option<CoercedInput> {
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
        _input: &Utf16JsonProjection,
        _ctx: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        Ok(())
    }

    /// Compute the permission decision for this `(input, ctx)` pair.
    ///
    /// The dispatcher calls this before invoking [`Tool::call`]. Hooks may
    /// override the result (e.g. allow / deny via `PreToolUse`).
    async fn check_permissions(
        &self,
        input: &Utf16JsonProjection,
        ctx: &ToolUseContext,
    ) -> PermissionResult;

    /// Filesystem path this invocation primarily targets, if any.
    ///
    /// Used by the permission engine and file-state cache.
    fn get_path(&self, _input: &Utf16JsonProjection) -> Option<PathBuf> {
        None
    }

    /// Human-readable description of what this invocation will do.
    async fn description(&self, input: &Utf16JsonProjection, opts: &DescriptionOptions) -> String;

    /// Long-form source-backed tool prompt used when assembling the system prompt.
    async fn prompt(&self, opts: &PromptOptions) -> Utf16JsonProjection;

    /// Execute the tool.
    ///
    /// # Errors
    /// Returns [`ToolError`] on permission denial, validation failure,
    /// I/O issues, abort, or file-state cache violations.
    async fn call(
        &self,
        input: Utf16JsonProjection,
        ctx: ToolUseContext,
        progress_tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError>;

    /// Short verb phrase for activity displays
    /// (e.g. `"Reading file"`, `"Running command"`).
    fn get_activity_description(&self, _input: &Utf16JsonProjection) -> Option<String> {
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
    fn user_facing_name_for_input(&self, _input: &Utf16JsonProjection) -> Option<String> {
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
    fn user_facing_name_background_color(&self, _input: &Utf16JsonProjection) -> Option<String> {
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

/// Model-visible skill suggestions for Bash's precommit guidance.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct BashPrecommitSkills {
    /// A model-visible `verify` skill loaded from a custom skill/command directory.
    pub custom_verify: bool,
    /// A model-visible `simplify` skill loaded from a custom skill/command directory.
    pub custom_simplify: bool,
    /// `code-review` is model-visible and its suggestion setting is enabled.
    pub code_review: bool,
}

/// Options controlling [`Tool::prompt`] output.
#[derive(Debug, Clone, Default)]
pub struct PromptOptions {
    /// Whether to include illustrative examples in the prompt.
    pub include_examples: bool,
    /// Trusted session whose current model route is producing this tool
    /// definition. `None` is used for child-agent requests that do not own the
    /// host conversation session.
    pub session_id: Option<lingxi_core::types::SessionId>,
    /// Runtime-only immutable session generation for Mod-backed prompts.
    pub session_origin: Option<Arc<dyn lingxi_core::host::subagent_spawn::SessionOriginCapability>>,
    /// The caller has a session owner; absence of a capability must not bind
    /// its Mod prompt to some other currently selected session.
    pub require_session_origin: bool,
    /// Whether this definition is for a host-scheduled turn. Interactive-only
    /// capabilities use this to avoid advertising unsupported continuations.
    pub is_scheduled_turn: bool,
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
    /// Whether this exact request can load the bundled `workflow-authoring` skill.
    /// This is captured per request so concurrent queries cannot observe another
    /// request's Skill-tool availability.
    pub workflow_authoring_available: bool,
    /// Skill suggestions from the owning session's current eligible skill set.
    /// Callers suppress these when the request does not offer the `Skill` tool.
    pub bash_precommit_skills: BashPrecommitSkills,
    /// Registry-owned session activation generation. Clear/resume increments
    /// it so a reused Bash tool starts a fresh precommit wording latch.
    pub bash_precommit_session_generation: u64,
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

/// Provenance of a tool's input JSON Schema as carried in a host ToolSpec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolInputSchemaProvenance {
    /// Generated from a bounded, local builtin schema producer.
    GeneratedBuiltin,
    /// Received from a source-owned definition such as MCP, Mod, or plugin metadata.
    SourceNative,
}

/// Which schema Native's pre-row JSON repair sees for a tool definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolInputRepairSchema {
    /// Use the exact advertised Source schema (including the current live
    /// schema snapshot, if present).
    Advertised,
    /// The Native tool has no `inputJSONSchema` for the repair call.
    Absent,
    /// A separately retained Source schema, only when authoritative metadata
    /// proves Native's repair schema differs from the advertised schema.
    SourceNative(Utf16JsonProjection),
}

/// Source facts required to apply the pre-row schema repair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolInputRepairMetadata {
    /// The source did not establish repair facts. This is distinct from a
    /// known-absent schema and must not silently suppress the repair stage.
    Unknown,
    /// Authoritative facts for the registered tool definition.
    Known {
        /// Native's schema selection for the pre-row repair stage.
        schema: ToolInputRepairSchema,
        /// Whether the schema was generated locally or retained from an external source.
        schema_provenance: ToolInputSchemaProvenance,
        /// `None` means no native exemption callback; `Some(empty)` is an
        /// explicit empty callback result. Both exempt no keys.
        exempt_top_level_keys: Option<HashSet<Vec<u16>>>,
    },
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
    pub input: Utf16JsonProjection,
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

pub use lingxi_core::host::tool_invoker::{ToolResultTurnEnd, ToolResultTurnEndSource};

/// Classify whether a successful tool result requests turn termination.
///
/// Matches Claude Code 2.1.286's `dEo` in the dispatcher's single-result
/// boolean domain: `toolEndsTurn` wins over MCP metadata, and an error result
/// never ends the turn even when either marker is present. Whole raw frames
/// and truthy non-booleans are separate native evidence, not coerced inputs.
#[must_use]
pub fn tool_result_turn_end(
    tool_ends_turn: bool,
    is_error: bool,
    mcp_meta: Option<&Utf16JsonProjection>,
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
        .and_then(|meta| meta.get_property("_meta"))
        .and_then(|meta| meta.get_property("claude/endTurn"))
        .and_then(JsonValueView::as_bool)
        .filter(|value| *value)
        .map(|_| ToolResultTurnEnd {
            source: ToolResultTurnEndSource::McpMeta,
        })
}

#[cfg(test)]
#[path = "tool_turn_end_oracle_tests.rs"]
mod tool_turn_end_oracle_tests;

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
    pub data: Utf16JsonProjection,
    /// Optional faithful model-facing string. When `Some`, the dispatch uses
    /// this verbatim as the tool's model text (and the SDK frame's
    /// `tool_result.content`), keeping `data` as pure metadata. When `None`,
    /// the dispatch falls back to deriving the model text out of `data`.
    pub model_content: Option<Utf16JsonProjection>,
    /// Extra conversation messages to inject after this call.
    pub new_messages: Vec<lingxi_core::types::ConversationMessage>,
    /// Optional one-shot mutator for the [`ToolUseContext`].
    pub context_modifier: Option<ContextModifier>,
    /// Opaque per-call metadata (used by MCP tools).
    pub mcp_meta: Option<Utf16JsonProjection>,
    /// Whether this result is an ERROR — drives the `tool_result` block's
    /// `is_error` flag. `false` for every native tool's success path (a native
    /// failure surfaces as an `Err`, which the dispatch flags separately); MCP
    /// tools set it from the server's `isError` so an MCP error RESULT (a logical
    /// failure, not a transport error) is flagged to the model 1:1 with claude-code.
    pub is_error: bool,
    /// Host-only signal that this Agent call already fired its SubagentStart
    /// hook. It must not be encoded in model-visible JSON, since output
    /// parsing strips unknown fields and Mod hooks may replace the payload.
    pub subagent_hooks_fired: bool,
}

impl ToolCallResult {
    /// Build a result from a `data` payload only — the model text is derived
    /// from `data` by the dispatch (`model_content` stays `None`).
    pub fn from_data(data: Utf16JsonProjection) -> Self {
        Self {
            data,
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
            subagent_hooks_fired: false,
        }
    }

    /// Attach a Rust-owned UTF-8 model-facing string. When text originates in
    /// an external source, use `source_text_projection_units` with its exact
    /// UTF-16 code units instead.
    #[must_use]
    pub fn with_model_text(mut self, text: &str) -> Self {
        self.model_content = Some(source_text_projection(text));
        self
    }
}

/// Import a schema generated locally from a bounded builtin definition.
/// Never use this for MCP, Mod, plugin, or other source-owned metadata.
#[must_use]
pub fn generated_builtin_schema(schema: &Value) -> Utf16JsonProjection {
    Utf16JsonProjection::from_serde_value(schema)
        .expect("locally generated builtin schema is representable as JSON")
}

/// Import a locally generated builtin result assembled only from literals,
/// validated typed values, or UTF-8-decoded process/filesystem text. Do not
/// use this for arbitrary values, or for MCP, Mod, plugin, or other source-owned
/// payloads; when a Source projection already exists, pass it directly.
#[must_use]
pub fn generated_builtin_result(result: &Value) -> Utf16JsonProjection {
    Utf16JsonProjection::from_serde_value(result)
        .expect("locally generated builtin result is representable as JSON")
}

/// Build a JSON string projection for Rust-owned UTF-8 tool text. External
/// MCP/plugin text that already has UTF-16 source units must use
/// [`source_text_projection_units`] instead.
#[must_use]
pub fn source_text_projection(text: &str) -> Utf16JsonProjection {
    let mut builder = Utf16JsonBuilder::new();
    let root = builder.push_string(text);
    builder
        .finish(root)
        .expect("a single Rust string is a valid JSON projection")
}

/// Build a JSON string projection without replacing unpaired UTF-16 units.
#[must_use]
pub fn source_text_projection_units(units: &[u16]) -> Utf16JsonProjection {
    let mut builder = Utf16JsonBuilder::new();
    let root = builder.push_string_units(units.to_vec());
    builder
        .finish(root)
        .expect("a single UTF-16 string is a valid JSON projection")
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
            .field("subagent_hooks_fired", &self.subagent_hooks_fired)
            .finish_non_exhaustive()
    }
}

/// Error returned by [`Tool::validate_input`].
#[derive(Debug, Clone, Error)]
#[error("invalid tool input")]
pub struct ValidationError(
    /// Human-readable validation error message as an exact root-string source.
    pub Utf16JsonProjection,
);

pub use lingxi_core::host::tool_invoker::{
    CleanupUnverifiedReason, ProcessTreeStatus, ReportedExitCode, ReportedShellPayload,
    ReportedShellPayloadParts, ReportedShellStream, UnverifiedCleanupOutput,
    UnverifiedCleanupOutputMetadata, UnverifiedCleanupOutputParts, MAX_UNVERIFIED_METADATA_CHARS,
    MAX_UNVERIFIED_OUTPUT_CHARS,
};

/// Error returned by [`Tool::call`] and related dispatcher operations.
#[derive(Clone, Error)]
#[non_exhaustive]
pub enum ToolError {
    /// No tool registered under the requested name.
    #[error("tool not found: {0}")]
    NotFound(String),
    /// Input failed validation (schema or tool-specific checks).
    // The model-facing message is source text and may contain unpaired UTF-16
    // units. Keep it as a root-string projection; Display remains a concise
    // logging label while `model_facing_message` returns the exact payload.
    #[error("invalid input")]
    InvalidInput(Utf16JsonProjection),
    /// Permission engine denied the call.
    #[error("permission denied: {}", .0.string_at("").unwrap_or_default())]
    PermissionDenied(Utf16JsonProjection),
    /// Underlying I/O failure (network, disk, etc.). The model-facing detail
    /// remains an exact source string; `Display` is a logging label.
    #[error("io")]
    Io(Utf16JsonProjection),
    /// The call was aborted (interrupt, cancellation).
    #[error("aborted")]
    Aborted,
    /// The process may still be running because its cleanup could not be verified.
    #[error("process cleanup could not be verified")]
    CleanupUnverified(String),
    /// The process outcome is incomplete. Captured output must remain an
    /// observation even if the tree was reaped but the exit code is unknown.
    #[error("process result is incomplete")]
    CleanupUnverifiedOutput(Box<UnverifiedCleanupOutput>),
    /// One or more process observations are incomplete. No item is an
    /// ordinary tool result; preserve each observation for the host.
    #[error("process outcome evidence is incomplete")]
    CleanupUnverifiedBatch(Vec<CleanupUnverifiedReason>),
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
    /// A spawned subagent terminated with an error. Preserve the allocated
    /// child's identity so the host can retain the correct lifecycle route.
    #[error("subagent failed: {}", .reason.string_at("").unwrap_or_default())]
    SubagentFailed {
        /// The allocated child, distinct from an admission failure.
        agent_id: lingxi_core::types::AgentId,
        /// Exact model-facing failure reason.
        reason: Utf16JsonProjection,
        /// An existing host lifecycle worker already owns the global stop.
        terminal_hooks_owned: bool,
    },
    /// M4-07: MCP server reported a tool-call failure.
    #[error("MCP tool error: {server}/{tool}")]
    McpFailure {
        /// MCP server identifier.
        server: String,
        /// MCP tool name.
        tool: String,
        /// Server-supplied detail string.
        detail: Utf16JsonProjection,
    },
    /// M4-07: LSP request failed. The model-facing message preserves the
    /// exact source text; `Display` remains a short logging label.
    #[error("LSP tool error")]
    LspFailure(Utf16JsonProjection),
    /// M4-03: HTTP transport error (DNS, connect, TLS, status code).
    #[error("transport error: {}", .0.string_at("").unwrap_or_default())]
    Transport(Utf16JsonProjection),
}

impl std::fmt::Debug for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(value) => f.debug_tuple("NotFound").field(value).finish(),
            Self::InvalidInput(value) => f.debug_tuple("InvalidInput").field(value).finish(),
            Self::PermissionDenied(value) => {
                f.debug_tuple("PermissionDenied").field(value).finish()
            }
            Self::Io(value) => f.debug_tuple("Io").field(value).finish(),
            Self::Aborted => f.write_str("Aborted"),
            Self::CleanupUnverified(_) => f.write_str("CleanupUnverified(<redacted>)"),
            Self::CleanupUnverifiedOutput(output) => f
                .debug_tuple("CleanupUnverifiedOutput")
                .field(output)
                .finish(),
            Self::CleanupUnverifiedBatch(reasons) => f
                .debug_tuple("CleanupUnverifiedBatch")
                .field(reasons)
                .finish(),
            Self::Internal(value) => f.debug_tuple("Internal").field(value).finish(),
            Self::InteractionRequired(value) => {
                f.debug_tuple("InteractionRequired").field(value).finish()
            }
            Self::EditWithoutRead { path } => f
                .debug_struct("EditWithoutRead")
                .field("path", path)
                .finish(),
            Self::FileModifiedExternally { path } => f
                .debug_struct("FileModifiedExternally")
                .field("path", path)
                .finish(),
            Self::PartialViewMustReread { path } => f
                .debug_struct("PartialViewMustReread")
                .field("path", path)
                .finish(),
            Self::FileContentMismatch { path } => f
                .debug_struct("FileContentMismatch")
                .field("path", path)
                .finish(),
            Self::FileTooLarge { size, limit } => f
                .debug_struct("FileTooLarge")
                .field("size", size)
                .field("limit", limit)
                .finish(),
            Self::PathBlocked { path } => {
                f.debug_struct("PathBlocked").field("path", path).finish()
            }
            Self::BinaryFile { path, reason } => f
                .debug_struct("BinaryFile")
                .field("path", path)
                .field("reason", reason)
                .finish(),
            Self::Timeout { timeout_ms } => f
                .debug_struct("Timeout")
                .field("timeout_ms", timeout_ms)
                .finish(),
            Self::OutputTruncated { limit } => f
                .debug_struct("OutputTruncated")
                .field("limit", limit)
                .finish(),
            Self::SubagentFailed {
                agent_id,
                reason,
                terminal_hooks_owned,
            } => f
                .debug_struct("SubagentFailed")
                .field("agent_id", agent_id)
                .field("reason", reason)
                .field("terminal_hooks_owned", terminal_hooks_owned)
                .finish(),
            Self::McpFailure {
                server,
                tool,
                detail,
            } => f
                .debug_struct("McpFailure")
                .field("server", server)
                .field("tool", tool)
                .field("detail", detail)
                .finish(),
            Self::LspFailure(value) => f.debug_tuple("LspFailure").field(value).finish(),
            Self::Transport(value) => f.debug_tuple("Transport").field(value).finish(),
        }
    }
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
    pub fn model_facing_message(&self) -> Utf16JsonProjection {
        match self {
            ToolError::InvalidInput(message)
            | ToolError::PermissionDenied(message)
            | ToolError::Transport(message)
            | ToolError::Io(message)
            | ToolError::LspFailure(message) => message.clone(),
            ToolError::SubagentFailed { reason, .. } => reason.clone(),
            ToolError::McpFailure { detail, .. } => detail.clone(),
            // Single-string variants whose `Display` prepends a non-meaningful
            // prefix: the inner string is the claude `error.message`.
            ToolError::NotFound(s) | ToolError::Internal(s) | ToolError::InteractionRequired(s) => {
                s.clone().into()
            }
            // Structured / prefix-free variants: `Display` already renders the
            // exact model-facing message (no LingXi-only prefix to strip).
            other => other.to_string().into(),
        }
    }
}

#[cfg(test)]
mod unverified_cleanup_output_tests {
    use super::*;

    #[test]
    fn retains_bounded_output_and_independent_evidence() {
        assert_eq!(
            MAX_UNVERIFIED_OUTPUT_CHARS,
            crate::util::output_truncation::MAX_TOOL_OUTPUT_LENGTH
        );
        let output = UnverifiedCleanupOutput::new(UnverifiedCleanupOutputParts {
            session_id: Some("session-1".into()),
            stdout: format!("{}tail", "字".repeat(MAX_UNVERIFIED_OUTPUT_CHARS)),
            stderr: "stderr-prefix".into(),
            stdout_truncated: false,
            stderr_truncated: true,
            stdout_eof: true,
            stderr_eof: false,
            exit_code: None,
            timed_out: true,
            termination_requested: Some(true),
            tree_status: ProcessTreeStatus::VerifiedReaped,
            cleanup_detail: "exit status not observed".into(),
            original_error: Some("stop read failed".into()),
        });
        let parts = output.parts();
        assert_eq!(parts.stdout.chars().count(), MAX_UNVERIFIED_OUTPUT_CHARS);
        assert!(!parts.stdout.contains("tail"));
        assert_eq!(parts.stderr, "stderr-prefix");
        assert!(parts.stdout_truncated);
        assert!(parts.stderr_truncated);
        assert!(parts.stdout_eof);
        assert!(!parts.stderr_eof);
        assert_eq!(parts.exit_code, None);
        assert!(parts.timed_out);
        assert_eq!(parts.termination_requested, Some(true));
        assert_eq!(parts.tree_status, ProcessTreeStatus::VerifiedReaped);
        assert_eq!(parts.original_error.as_deref(), Some("stop read failed"));
        assert_eq!(parts.session_id.as_deref(), Some("session-1"));
        let error = ToolError::CleanupUnverifiedOutput(Box::new(output));
        assert!(!format!("{error:?}").contains("stderr-prefix"));
        assert!(!format!("{error:?}").contains('字'));
        assert!(!error.to_string().contains("stderr-prefix"));
        assert_eq!(error.to_string(), "process result is incomplete");
        let error = ToolError::CleanupUnverified("secret native diagnostic".into());
        assert!(!format!("{error:?}").contains("secret native diagnostic"));
        assert!(!error.to_string().contains("secret native diagnostic"));
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
            invalid.model_facing_message().string_at(""),
            Some("Agent type 'x' not found. Available agents: general-purpose")
        );
        // Display is a concise UTF-8 logging label; the exact source message
        // remains available through `model_facing_message` and Debug.
        assert_eq!(invalid.to_string(), "invalid input");

        let exact_message = Utf16JsonProjection::parse(r#""bad \ud800 value""#).unwrap();
        let exact_error = ToolError::InvalidInput(exact_message.clone());
        assert_eq!(exact_error.model_facing_message(), exact_message);
        assert_eq!(
            exact_error.model_facing_message().string_units(""),
            Some(vec![
                'b' as u16, 'a' as u16, 'd' as u16, ' ' as u16, 0xd800, ' ' as u16, 'v' as u16,
                'a' as u16, 'l' as u16, 'u' as u16, 'e' as u16,
            ])
        );

        let internal = ToolError::Internal(
            "Agent 'x' requires MCP servers matching: github. MCP servers with tools: none.".into(),
        );
        assert_eq!(
            internal.model_facing_message().string_at(""),
            Some("Agent 'x' requires MCP servers matching: github. MCP servers with tools: none.")
        );

        // Other single-string variants are bare too.
        assert_eq!(
            ToolError::SubagentFailed {
                agent_id: lingxi_core::types::AgentId::new(),
                reason: "boom".into(),
                terminal_hooks_owned: false,
            }
            .model_facing_message()
            .string_at(""),
            Some("boom")
        );

        // Structured variants keep their full Display (no spurious prefix).
        let timeout = ToolError::Timeout { timeout_ms: 5000 };
        assert_eq!(
            timeout.model_facing_message().string_at(""),
            Some("command timed out after 5000ms")
        );
    }

    #[test]
    fn source_error_variants_keep_lone_utf16_units_in_model_messages() {
        let source =
            Utf16JsonProjection::root_string(String::from_utf16_lossy(&[0xD800]), vec![0xD800])
                .expect("source error is a valid root string");
        assert_eq!(ToolError::Io(source.clone()).model_facing_message(), source);
        assert_eq!(
            ToolError::LspFailure(source.clone()).model_facing_message(),
            source
        );

        let detail =
            Utf16JsonProjection::root_string(String::from_utf16_lossy(&[0xDFFF]), vec![0xDFFF])
                .expect("source MCP detail is a valid root string");
        let message = ToolError::McpFailure {
            server: "srv".into(),
            tool: "read".into(),
            detail,
        }
        .model_facing_message();
        assert_eq!(message.string_units(""), Some(vec![0xDFFF]));
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
        schema: Utf16JsonProjection,
        has_output_schema: bool,
    }

    #[async_trait]
    impl Tool for BareTool {
        fn name(&self) -> &str {
            "Bare"
        }
        fn input_schema(&self) -> &Utf16JsonProjection {
            &self.schema
        }
        fn output_schema(&self) -> Option<&Utf16JsonProjection> {
            self.has_output_schema.then_some(&self.schema)
        }
        fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
            ToolInputRepairMetadata::Known {
                schema: ToolInputRepairSchema::Advertised,
                schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
                exempt_top_level_keys: None,
            }
        }
        fn is_enabled(&self, _: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            0
        }
        fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
            true
        }
        fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
            true
        }
        async fn check_permissions(
            &self,
            _: &Utf16JsonProjection,
            _: &ToolUseContext,
        ) -> PermissionResult {
            unimplemented!("not exercised")
        }
        async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
            String::new()
        }
        async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
            Utf16JsonProjection::from("")
        }
        async fn call(
            &self,
            _: Utf16JsonProjection,
            _: ToolUseContext,
            _: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            unimplemented!("not exercised")
        }
    }

    #[test]
    fn user_facing_name_methods_default_to_none() {
        let t = BareTool {
            schema: Utf16JsonProjection::parse(r#"{"type":"object"}"#).unwrap(),
            has_output_schema: false,
        };
        let input = Utf16JsonProjection::parse("{}").unwrap();
        assert_eq!(t.user_facing_name_for_input(&input), None);
        assert_eq!(t.user_facing_name_background_color(&input), None);
    }

    /// BASH-18: the new `coerce_input` seam is OPT-IN — a tool that does not
    /// override it returns `None`, which the dispatcher reads as "use the raw
    /// input" (oracle `e.coerceInput?.(t) ?? null`).
    #[test]
    fn coerce_input_defaults_to_none() {
        let t = BareTool {
            schema: Utf16JsonProjection::parse(r#"{"type":"object"}"#).unwrap(),
            has_output_schema: false,
        };
        let input = Utf16JsonProjection::parse(r#"{"timeout_ms":5000}"#).unwrap();
        assert!(t.coerce_input(&input).is_none());
    }

    #[tokio::test]
    async fn output_parser_is_identity_without_schema_and_fails_closed_with_unmapped_schema() {
        let input = Utf16JsonProjection::parse(r#"{"value":"\ud800"}"#).unwrap();
        let bare = BareTool {
            schema: Utf16JsonProjection::parse(r#"{"type":"object"}"#).unwrap(),
            has_output_schema: false,
        };
        assert!(bare
            .parse_output(&input)
            .await
            .unwrap()
            .semantically_eq(&input));

        let schema_bearing = BareTool {
            schema: Utf16JsonProjection::parse(r#"{"type":"object"}"#).unwrap(),
            has_output_schema: true,
        };
        let error = schema_bearing.parse_output(&input).await.unwrap_err();
        let ToolError::InvalidInput(message) = error else {
            panic!("expected an explicit missing-parser error")
        };
        assert_eq!(
            message.string_at(""),
            Some("Native output parser is not mapped for schema-bearing tool 'Bare'")
        );
    }

    macro_rules! source_json {
        ($($tokens:tt)*) => {
            Utf16JsonProjection::from_serde_value(&serde_json::json!($($tokens)*)).unwrap()
        };
    }

    #[test]
    fn tool_result_turn_end_matches_tool_mcp_and_error_precedence() {
        let detected = tool_result_turn_end(
            false,
            false,
            Some(&source_json!({
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
                Some(&source_json!({ "_meta": { "claude/endTurn": true } })),
            ),
            Some(ToolResultTurnEnd {
                source: ToolResultTurnEndSource::Tool,
            })
        );
        assert!(tool_result_turn_end(
            false,
            false,
            Some(&source_json!({
                "_meta": { "claude/endTurn": false }
            }))
        )
        .is_none());
        assert!(tool_result_turn_end(
            false,
            false,
            Some(&source_json!({
                "_meta": { "vendor/endTurn": true }
            }))
        )
        .is_none());
        assert!(tool_result_turn_end(true, true, None).is_none());
        assert!(tool_result_turn_end(
            false,
            true,
            Some(&source_json!({ "_meta": { "claude/endTurn": true } })),
        )
        .is_none());
    }
}
