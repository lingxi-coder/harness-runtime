//! `ToolInvoker` impl wrapping a `ToolRegistry`.
//!
//! `RegistryToolInvoker` is the production adapter `AgentTool` uses to hand
//! a tool-dispatch surface to the spawner. The wrapper stores
//! `Arc<ToolRegistry>` so the recursion-lock invariant (parent + child
//! subagent share the *same* `Arc`) survives across the spawn boundary —
//! a property asserted via `Arc::ptr_eq` in M4-05 wiring tests.

use crate::registry::ToolRegistry;
use async_trait::async_trait;
use lingxi_core::host::permission_gate::PermissionGate;
use lingxi_core::host::tool_invoker::{
    SubagentInvocationContext, ToolInvocationContextModifier, ToolInvocationContextState,
    ToolInvoker, ToolInvokerError,
};
use serde_json::Value;
use std::sync::Arc;

/// Turn fork-time frozen command denies into the `disallowed_tools` permission
/// layer the fold understands (claude `freezeCommandDenies` → the
/// `case"disallowed_tools"` arm of `gn(toolUseContext)`).
///
/// Returns an EMPTY vec when `frozen` is empty or nothing survives, which
/// leaves [`lingxi_core::host::permission_gate::PermissionCheckContext::permission_layers`]
/// empty and the fold byte-identical to a spawn that froze nothing.
///
/// ## Entries are CANONICALIZED, not round-trip-tested
///
/// The gate re-parses whatever this emits — `PolicyPermissionGate::
/// extend_command_rules` feeds every layer string back through
/// [`permission::PermissionRuleValue::from_rule_string`]. So emitting a string
/// the parser reads differently than intended is the one failure that matters,
/// and it fails OPEN: a padded `" Bash(rm:*) "` parses to a BARE rule for a tool
/// literally named `" Bash(rm:*) "`, which matches nothing, so the deny silently
/// evaporates.
///
/// Rejecting anything that does not round-trip byte-for-byte fails open in the
/// other direction: `from_rule_string` deliberately normalizes (`Bash(*)` and
/// `Bash()` collapse to the tool-wide form, content parens are re-escaped,
/// legacy tool names are rewritten), so a round-trip test discards legitimate —
/// and in the `Bash(*)` case, maximally strict — rules as "corrupt". Upstream
/// `jLa` validates nothing at all; it spreads the strings straight into
/// `alwaysDenyRules.command`.
///
/// Emitting `parsed.to_rule_string()` gets both: every rule the parser
/// understood survives, in exactly the spelling the gate will re-parse.
///
/// Non-`Bash` rules are SKIPPED and logged rather than failing the resume:
/// refusing to resume would turn a damaged sidecar into an unrecoverable agent,
/// and the surviving entries still tighten the policy.
///
/// KNOWN DIVERGENCE — do not "fix" one half of this alone. Claude reads
/// `alwaysDenyRules.command`, which is keyed by rule SOURCE, not by tool: the
/// `command` bucket legitimately holds `Read(...)` / `WebFetch(...)` / `Edit(...)`
/// rules (claude unions a skill's frontmatter `disallowed-tools` straight into
/// it). This port instead freezes only tool-`Bash` rules, in BOTH halves — the
/// producer (`tools/skill/src/skill.rs::frozen_command_denies`) and the filter
/// below. Correcting the producer to read `PermissionRuleSource::Command`
/// WITHOUT also dropping this filter would make every non-`Bash` rule it newly
/// captures get silently discarded here and logged as a corrupt record.
#[must_use]
fn frozen_command_deny_layers(frozen: &[String]) -> Vec<Value> {
    let kept: Vec<String> = frozen
        .iter()
        .filter_map(|raw| {
            let rule = raw.trim();
            if rule.is_empty() {
                tracing::warn!(
                    target: "permission",
                    "frozen_command_denies: dropping an empty rule from the scoping record"
                );
                return None;
            }
            // `from_rule_string` is INFALLIBLE — a mangled string degrades to a
            // bare tool name rather than erroring — so malformed input has to be
            // caught by inspecting what it parsed INTO.
            //
            // The check is no longer "is this Bash?": the frozen set is the
            // `command` SOURCE bucket, which legitimately holds rules for any
            // tool. What a real tool name can never contain is a paren, so an
            // unbalanced rule like `"Bash(unbalanced"` — which parses as a tool
            // NAMED `"Bash(unbalanced"` — is still rejected here, while
            // `"Read(/etc/passwd)"` and the tool-wide `"Bash(*)"` both survive.
            let parsed = permission::PermissionRuleValue::from_rule_string(rule);
            if parsed.tool_name.contains('(') || parsed.tool_name.contains(')') {
                tracing::warn!(
                    target: "permission",
                    "frozen_command_denies: dropping {rule:?} — parsed tool name {:?} contains a \
                     paren, so the rule was malformed (scoping record corrupt or written by \
                     another producer)",
                    parsed.tool_name
                );
                return None;
            }
            Some(parsed.to_rule_string())
        })
        .collect();
    if kept.is_empty() {
        return Vec::new();
    }
    // Built through the layer type that OWNS this wire spelling, so the
    // `disallowed_tools` / `disallowedTools` keys cannot drift from the
    // `PermissionLayer::from_wire` that reads them back.
    vec![permission::PermissionLayer::DisallowedTools(kept).to_wire()]
}

/// Identify a tool-owned ASK without guessing from a wire name.  MCP tools
/// expose their clamp through `is_mcp`, while Workflow's nested Read check
/// carries a structured blocked path. `PermissionPromptTool` is the explicit
/// requires-user-interaction reason. Ordinary Bash sandbox asks intentionally
/// do not use this protected path and retain their existing carve-outs.
fn tool_permission_ask_is_protected(
    tool: &dyn crate::tool_trait::Tool,
    result: &permission::PermissionResult,
) -> bool {
    let permission::PermissionResult::Ask {
        reason, metadata, ..
    } = result
    else {
        return false;
    };
    tool.is_mcp()
        || metadata.blocked_path.is_some()
        || matches!(
            reason,
            permission::PermissionDecisionReason::PermissionPromptTool { .. }
        )
}

/// Private child-tool carrier accepted only by the enforcing registry adapter.
/// The child runtime mints it; serialized model data cannot select its body.
pub struct SuppliedTool(pub Arc<dyn crate::Tool>);

/// Wraps an `Arc<ToolRegistry>` as a `dyn ToolInvoker`.
///
/// Cheap to construct; clones share the same registry `Arc`.
pub struct RegistryToolInvoker {
    registry: Arc<ToolRegistry>,
    /// Permission gate consulted before every dispatch (enforcement 3b).
    /// `None` (the default) preserves the legacy always-dispatch behavior for
    /// tests / hosts without enforcement wired. When `Some`, this is the SAME
    /// gate the main loop consults — so subagent and teammate tool calls are
    /// now subject to the same allow/deny rules, closing the bypass.
    gate: Option<Arc<dyn PermissionGate>>,
    /// (Finding 22) Marks every dispatch through this invoker as owned by a
    /// background task with no interactive turn — see
    /// [`lingxi_core::host::permission_gate::PermissionCheckContext::background_owned`].
    /// Default `false`; set only via [`Self::with_background_owned`].
    background_owned: bool,
}

impl RegistryToolInvoker {
    /// Construct an invoker bound to `registry`. The `Arc` is stored
    /// verbatim — `Arc::ptr_eq` between this invoker's clone and the
    /// parent's clone returns `true`.
    #[must_use]
    pub fn new(registry: Arc<ToolRegistry>) -> Self {
        Self {
            registry,
            gate: None,
            background_owned: false,
        }
    }

    /// Attach the permission gate consulted before each dispatch (3b). Without
    /// it, dispatch is unconditional (legacy behavior).
    #[must_use]
    pub fn with_gate(mut self, gate: Arc<dyn PermissionGate>) -> Self {
        self.gate = Some(gate);
        self
    }

    /// (Finding 22) Mark every dispatch through this invoker as
    /// background-owned (no interactive turn owns it) — see
    /// [`lingxi_core::host::permission_gate::PermissionCheckContext::background_owned`].
    /// The composition root calls this ONLY on the invoker instance wired
    /// exclusively to a background task's spawner (as of writing, the
    /// `/fusion` background task's `fusion_invoker`); an invoker that a
    /// foreground direct tool call can also reach must never set this.
    #[must_use]
    pub fn with_background_owned(mut self, background_owned: bool) -> Self {
        self.background_owned = background_owned;
        self
    }

    /// Borrow the underlying registry `Arc`.
    #[must_use]
    pub fn registry_arc(&self) -> &Arc<ToolRegistry> {
        &self.registry
    }

    /// Current enforcing mode, for host-owned spawn decisions only.
    pub fn permission_mode(&self) -> Option<String> {
        self.gate.as_ref().and_then(|gate| gate.permission_mode())
    }
}

#[async_trait]
impl ToolInvoker for RegistryToolInvoker {
    async fn cleanup_computer_inputs(
        &self,
        agent_id: lingxi_core::types::AgentId,
        origin_session_id: Option<lingxi_core::types::SessionId>,
    ) -> Result<(), ToolInvokerError> {
        let Some(tool) = self
            .registry
            .find_registered("computer")
            .filter(|tool| !tool.is_mcp())
        else {
            return Ok(());
        };
        let ctx = crate::context::ToolUseContext {
            input_projection: None,
            options: crate::context::ToolUseOptions {
                debug: false,
                verbose: false,
                main_loop_model: "subagent".into(),
                model_profile: None,
                max_budget_nano_usd: None,
                mcp_clients: vec![],
                is_non_interactive_session: true,
                custom_system_prompt: None,
                append_system_prompt: None,
            },
            messages: vec![],
            tool_use_id: None,
            assistant_message_id: None,
            assistant_message: None,
            same_turn_tool_uses: vec![],
            agent_id: Some(agent_id),
            origin_session_id,
            agent_spawn_provenance: Default::default(),
            nested_memory_triggers: Arc::default(),
            agent_name: None,
            team_name: None,
            instruction_context: None,
            tool_execution_policy: Default::default(),
            trusted_effective_permission_mode: None,
            classifier_only_review: None,
            content_replacement_state: None,
            session: None,
            subagent_registry: None,
            cancel: None,
            fork_parent_system_prompt: None,
            cwd: None,
            depth: 0,
            observer: None,
            observer_pairings: None,
            file_history: None,
        };
        tool.cleanup_computer_inputs(&ctx)
            .await
            .map_err(|error| ToolInvokerError::Internal(error.to_string()))
    }

    fn permission_mode(&self) -> Option<String> {
        RegistryToolInvoker::permission_mode(self)
    }

    fn map_result_text(&self, name: &str, result: &Value) -> Option<String> {
        self.registry
            .find_by_name(name)
            .and_then(|tool| tool.map_result_text(result))
    }

    fn map_result_is_error(&self, name: &str, result: &Value) -> Option<bool> {
        self.registry
            .find_by_name(name)
            .and_then(|tool| tool.map_result_is_error(result))
    }

    fn validate_output(&self, name: &str, output: &Value) -> Result<(), String> {
        self.registry
            .find_by_name(name)
            .and_then(|tool| tool.output_schema().cloned())
            .map_or(Ok(()), |schema| {
                crate::output_schema::validate(&schema, output)
            })
    }

    fn tool_is_concurrency_safe(&self, name: &str, input: &Value) -> Option<bool> {
        self.registry
            .find_by_name(name)
            .map(|tool| tool.is_concurrency_safe(input))
    }

    async fn invoke(
        &self,
        name: &str,
        input: Value,
        ctx: SubagentInvocationContext,
    ) -> Result<Value, ToolInvokerError> {
        self.invoke_with_workspace_lease(name, input, ctx, None)
            .await
    }

    async fn invoke_with_workspace_lease(
        &self,
        name: &str,
        input: Value,
        ctx: SubagentInvocationContext,
        workspace_lease_token: Option<u64>,
    ) -> Result<Value, ToolInvokerError> {
        self.invoke_detailed(name, input, ctx, workspace_lease_token)
            .await
            .map(|result| result.data)
    }

    async fn invoke_detailed(
        &self,
        name: &str,
        input: Value,
        ctx: SubagentInvocationContext,
        workspace_lease_token: Option<u64>,
    ) -> Result<lingxi_core::host::tool_invoker::ToolInvocationResult, ToolInvokerError> {
        self.invoke_registered_or_supplied(name, input, ctx, workspace_lease_token, None)
            .await
    }

    async fn invoke_supplied_detailed(
        &self,
        name: &str,
        input: Value,
        ctx: SubagentInvocationContext,
        workspace_lease_token: Option<u64>,
        supplied: Arc<dyn std::any::Any + Send + Sync>,
    ) -> Result<lingxi_core::host::tool_invoker::ToolInvocationResult, ToolInvokerError> {
        if self.gate.is_none() {
            return Err(ToolInvokerError::Internal(
                "Host-supplied tools require a bound permission gate".into(),
            ));
        }
        let tool = supplied
            .downcast_ref::<SuppliedTool>()
            .ok_or_else(|| ToolInvokerError::Internal("Unknown host-supplied tool carrier".into()))?
            .0
            .clone();
        if tool.name() != name && !tool.aliases().contains(&name) {
            return Err(ToolInvokerError::NotFound(name.into()));
        }
        self.invoke_registered_or_supplied(name, input, ctx, workspace_lease_token, Some(tool))
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl RegistryToolInvoker {
    async fn invoke_registered_or_supplied(
        &self,
        name: &str,
        mut input: Value,
        ctx: SubagentInvocationContext,
        workspace_lease_token: Option<u64>,
        supplied: Option<Arc<dyn crate::Tool>>,
    ) -> Result<lingxi_core::host::tool_invoker::ToolInvocationResult, ToolInvokerError> {
        let tool = supplied
            .or_else(|| self.registry.find_by_name(name))
            .ok_or_else(|| ToolInvokerError::NotFound(name.to_string()))?;

        let mut input_projection = ctx.input_projection.clone().unwrap_or_else(|| {
            lingxi_core::types::utf16_json::Utf16JsonProjection::plain(input.clone())
        });
        if input_projection.value != input {
            return Err(ToolInvokerError::InvalidInput(
                "tool input projection belongs to different input".into(),
            ));
        }
        input_projection
            .validate()
            .map_err(|error| ToolInvokerError::InvalidInput(error.to_string()))?;

        // BASH-18 `coerceInput` (claude-code 2.1.238 BIN off **294282716**): the
        // tool's own pre-validation normalization of the model's raw arguments.
        // The oracle applies it once, at the TOP of `checkPermissionsAndCallTool`,
        // and the rewritten value is what the permission check and `call` both
        // see — so the subagent dispatch surface must coerce here too, BEFORE the
        // gate below, or a subagent's `Bash{timeout_ms}` would silently lose its
        // timeout where the main loop honours it. `None` for every tool but
        // `Bash` ⇒ strict no-op.
        //
        // Native-schema opt-in tools run their shared parse and validation
        // gates below; other tools retain their existing dispatch contract.
        if let Some(coerced) = tool.coerce_input(&input) {
            input = coerced.input;
            input_projection
                .rebase_display_value(input.clone())
                .map_err(|error| ToolInvokerError::InvalidInput(error.to_string()))?;
        }

        if tool.native_input_validation() {
            match tool.parse_native_input(&input) {
                Some(Ok(parsed)) => {
                    input = parsed;
                    input_projection
                        .rebase_display_value(input.clone())
                        .map_err(|error| ToolInvokerError::InvalidInput(error.to_string()))?;
                }
                Some(Err(error)) => {
                    return Err(ToolInvokerError::Validation(format!(
                        "InputValidationError: {}",
                        error.display
                    )));
                }
                // `None` means "schema vocabulary outside the flat collector",
                // not "invalid input". The main turn treats it that way —
                // `orchestrator::schema_validation::validate_tool_schema_detailed`
                // falls through to the generic collector — so hard-failing here
                // made nested/subagent dispatch the only surface that breaks
                // when an opt-in tool's schema grows an `enum`, an array, or a
                // nested object. Leave the input as-is and let the generic gate
                // downstream judge it, exactly like the main loop.
                None => {}
            }
        }

        // Tool-owned checks (MCP clamps and Workflow's nested Read gate) run
        // before either policy outcome can reach the tool body.
        let mut tool_use_ctx = match ctx.tool_context_state.as_ref() {
            Some(state) => state
                .downcast_arc::<crate::context::ToolUseContext>()
                .map_err(|error| {
                    ToolInvokerError::Internal(format!(
                        "Nested tool context state is invalid: {error}"
                    ))
                })?
                .as_ref()
                .clone(),
            None => crate::context::ToolUseContext {
                input_projection: None,
                options: crate::context::ToolUseOptions {
                    debug: false,
                    verbose: false,
                    main_loop_model: ctx
                        .parent_model
                        .clone()
                        .unwrap_or_else(|| "subagent".into()),
                    model_profile: ctx.parent_model_profile.clone(),
                    max_budget_nano_usd: None,
                    mcp_clients: vec![],
                    is_non_interactive_session: ctx.is_non_interactive_session,
                    custom_system_prompt: None,
                    append_system_prompt: None,
                },
                messages: ctx.current_history.clone(),
                tool_use_id: None,
                assistant_message_id: None,
                assistant_message: ctx.assistant_message.clone(),
                same_turn_tool_uses: ctx.same_turn_tool_uses.clone(),
                agent_id: None,
                agent_spawn_provenance: ctx.agent_spawn_provenance.clone(),
                nested_memory_triggers: Arc::default(),
                agent_name: None,
                team_name: None,
                origin_session_id: None,
                instruction_context: None,
                tool_execution_policy: ctx.tool_execution_policy,
                trusted_effective_permission_mode: None,
                classifier_only_review: None,
                content_replacement_state: None,
                session: None,
                subagent_registry: None,
                cancel: Some(ctx.cancellation_token.clone()),
                fork_parent_system_prompt: None,
                cwd: None,
                depth: 0,
                observer: None,
                observer_pairings: None,
                file_history: None,
            },
        };

        // Dynamic and trusted host facts always win over a previous call's
        // ToolUseContext snapshot. Other fields retain prior, tool-owned
        // context mutations so a nested agent sees the full modifier result.
        tool_use_ctx.options.main_loop_model = ctx
            .parent_model
            .clone()
            .unwrap_or_else(|| "subagent".into());
        tool_use_ctx.options.model_profile = ctx.parent_model_profile.clone();
        tool_use_ctx.options.is_non_interactive_session = ctx.is_non_interactive_session;
        tool_use_ctx.messages = ctx.current_history.clone();
        tool_use_ctx.assistant_message = ctx.assistant_message.clone();
        tool_use_ctx.same_turn_tool_uses = ctx.same_turn_tool_uses.clone();
        tool_use_ctx.tool_use_id = ctx
            .tool_use_id
            .clone()
            .map(lingxi_core::types::ToolUseId::from);
        tool_use_ctx.assistant_message_id = ctx.assistant_message_id;
        tool_use_ctx.agent_id = ctx.parent_agent_id;
        tool_use_ctx.agent_spawn_provenance = ctx.agent_spawn_provenance.clone();
        tool_use_ctx.agent_name = ctx.agent_name.clone();
        tool_use_ctx.team_name = ctx.team_name.clone();
        tool_use_ctx.origin_session_id = ctx.origin_session_id;
        tool_use_ctx.instruction_context = ctx.instruction_context.clone();
        tool_use_ctx.tool_execution_policy = ctx.tool_execution_policy;
        tool_use_ctx.trusted_effective_permission_mode =
            ctx.mode_override.clone().or_else(|| self.permission_mode());
        tool_use_ctx.subagent_registry = Some(self.registry.clone());
        // The current physical tool use owns this token. Do not carry a stale
        // token forward from the previous invocation's context snapshot.
        tool_use_ctx.cancel = Some(ctx.cancellation_token.clone());
        tool_use_ctx.fork_parent_system_prompt = ctx
            .fork_context
            .as_ref()
            .and_then(|fork| fork.system_prompt.clone());
        tool_use_ctx.cwd = ctx.cwd.clone();
        // Every invocation replaces a retained context snapshot's old carrier.
        tool_use_ctx.input_projection = Some(input_projection);
        tool_use_ctx.depth = ctx.depth;
        tool_use_ctx.observer = ctx.observer.clone();
        tool_use_ctx.observer_pairings = None;
        tool_use_ctx.file_history = None;
        if tool.native_input_validation() {
            if let Err(crate::ValidationError(message)) =
                tool.validate_input(&input, &tool_use_ctx).await
            {
                return Err(ToolInvokerError::Validation(message));
            }
        }
        let tool_permission_result = tool.check_permissions(&input, &tool_use_ctx).await;
        let tool_ask_is_protected =
            tool_permission_ask_is_protected(tool.as_ref(), &tool_permission_result);

        // A tool-local DENY is authoritative even when no outer gate is
        // installed (and therefore also over every outer Allow/Auto/Bypass).
        if let permission::PermissionResult::Deny { explanation, .. } = &tool_permission_result {
            return Err(ToolInvokerError::Internal(
                explanation
                    .clone()
                    .unwrap_or_else(|| format!("Permission to use {name} has been denied.")),
            ));
        }
        // A protected ASK needs a transport to obtain a human decision.  No
        // gate or a headless owner must fail closed rather than silently run
        // the tool-local operation.
        if tool_ask_is_protected && (self.gate.is_none() || ctx.is_non_interactive_session) {
            return Err(ToolInvokerError::Internal(format!(
                "Permission to use {name} has been denied."
            )));
        }

        // The trusted requester owns one file, not a directory-wide grant.
        // A per-call allow layer only removes the plan-mode mutation ask;
        // explicit deny/ask rules and all earlier safety checks still run.
        let own_plan_path =
            if ctx.mode_override.as_deref() == Some("plan") && matches!(name, "Write" | "Edit") {
                input
                    .get("file_path")
                    .and_then(Value::as_str)
                    .and_then(|path| {
                        lingxi_core::host::teammate_plan::own_plan_file_root(
                            ctx.parent_agent_id.as_ref(),
                            std::path::Path::new(path),
                        )
                        .map(|_| std::path::PathBuf::from(path))
                    })
            } else {
                None
            };

        // Permission gate (enforcement 3b). The subagent/teammate dispatch
        // surface now consults the same gate as the main loop — previously it
        // dispatched any registered tool unconditionally (the bypass). A `Deny`
        // is surfaced as `Internal`, while a headless auto-mode breaker trip is
        // preserved as `Abort` so the subagent loop terminates instead of
        // rendering a recoverable `tool_result`. Checked AFTER `find_by_name`
        // so an unknown tool stays `NotFound`, and BEFORE the borrow of `input`
        // is moved into `call`.
        if let Some(gate) = &self.gate {
            // Attribute the prompt to the originating worker (claude-code 2.1.186:
            // a background subagent's permission prompt surfaces in the main
            // session with a `● @name` badge). Attach the worker identity only for
            // a NAMED worker that may surface prompts (`can_show_permission_prompts`)
            // — a one-shot subagent (no display name) carries `None`, leaving the
            // prompt unattributed exactly as before.
            let worker = (ctx.can_show_permission_prompts)
                .then(|| ctx.agent_name.clone())
                .flatten()
                .map(|name| lingxi_core::host::permission_gate::PromptWorker {
                    name,
                    team: ctx.team_name.clone(),
                    is_async: ctx.is_async,
                });
            // Use the richer `check_with_context` (not `check_with_worker`) so the
            // subagent path is byte-faithful to the main loop's: it carries the
            // REAL `tool_use_id` of the dispatching call (so a stdio
            // `can_use_tool` request is correlatable) AND applies the host/policy
            // `updatedInput` rewrite to the input the tool actually runs with —
            // mirroring the turn-loop Ask arm. The default `check_with_context`
            // delegates to `check_with_worker` and maps `Allow`→`Allow{None}`, so
            // a gate that only overrides `check_with_worker` (or `check`) is
            // unchanged. Behavior is identical when `updated_input` is `None`.
            let check_ctx = lingxi_core::host::permission_gate::PermissionCheckContext {
                input_projection: Some(
                    tool_use_ctx
                        .projected_input(&input)
                        .map_err(|error| ToolInvokerError::InvalidInput(error.to_string()))?,
                ),
                pause_observer: ctx.permission_pause_observer.clone(),
                worker,
                tool_use_id: ctx.tool_use_id.clone(),
                requires_user_interaction: tool.requires_user_interaction(),
                // A tool-owned interaction is always per-call. Carry the
                // persistence guard alongside the transport metadata so TUI,
                // adapter, and stdio hosts cannot offer or honor a stale
                // AllowAlways response for subagent dispatches.
                suppress_always_allow_rule: tool.requires_user_interaction(),
                request_source: ctx.request_source,
                // Per-call permission-mode override (claude-code 2.1.207 Agent
                // `mode` → the child's `toolPermissionContext.mode`): a
                // `mode:"plan"` subagent's dispatch authorizes under Plan so
                // mutations are gated while reads stay frictionless. `None` for the
                // main thread / a spawn with no override.
                mode_override: ctx.mode_override.clone(),
                is_non_interactive_session: ctx.is_non_interactive_session,
                workspace_lease_token,
                // Replay the fork-time command denies as a `disallowed_tools`
                // LAYER. The fold applies layers ON TOP of the base policy, so a
                // frozen deny wins over a live rule that would now allow the same
                // command — which is the entire point: a settings edit made while
                // a fork was parked must not WIDEN what it may run on resume.
                permission_layers: {
                    let mut layers = frozen_command_deny_layers(&ctx.frozen_command_denies);
                    if own_plan_path.is_some() {
                        layers.push(
                            serde_json::json!({"kind":"allowed_tools","allowedTools":[name]}),
                        );
                    }
                    layers
                },
                // Finding 22: carry THIS invoker's background-owned marker
                // (set only on the dedicated background-task invoker, see
                // `with_background_owned`) onto every check so a transport
                // that serializes interactive prompts can tell a background
                // run's ask apart from the interactive turn's own.
                background_owned: self.background_owned,
                ..Default::default()
            };
            // Tool-local DENY has priority over every policy result.  For a
            // protected tool ASK, resolve the policy without its transport
            // first, then send the single ASK through that same worker-aware
            // transport; re-running the policy would swallow MCP ceilings or
            // Workflow's nested Read requirement on an outer Allow.
            let outcome = if let Some(policy) = tool.classifier_only() {
                let reviewed = gate
                    .check_classifier_only_with_context_or_abort(
                        name,
                        &input,
                        &check_ctx,
                        policy,
                        &lingxi_core::host::permission_gate::ClassifierOnlyReviewRequest {
                            transcript: tool_use_ctx.messages.clone(),
                            action: tool.classifier_only_action(&input),
                        },
                    )
                    .await
                    .map_err(|abort| ToolInvokerError::Abort(abort.message))?;
                tool_use_ctx.classifier_only_review = reviewed.review;
                reviewed.permission
            } else if tool_ask_is_protected {
                let resolution = gate
                    .resolve_detailed_or_abort(name, &input, &check_ctx)
                    .await
                    .map_err(|abort| ToolInvokerError::Abort(abort.message))?;
                match resolution {
                    lingxi_core::host::permission_gate::PermissionResolution::Deny {
                        reason,
                        ..
                    } => {
                        return Err(ToolInvokerError::Internal(reason));
                    }
                    lingxi_core::host::permission_gate::PermissionResolution::Allow { .. }
                    | lingxi_core::host::permission_gate::PermissionResolution::Ask
                    | lingxi_core::host::permission_gate::PermissionResolution::AskWithContext {
                        ..
                    } => gate.ask_via_transport(name, &input, &check_ctx).await,
                }
            } else {
                gate.check_with_context_or_abort(name, &input, &check_ctx)
                    .await
                    .map_err(|abort| ToolInvokerError::Abort(abort.message))?
            };
            match outcome {
                lingxi_core::host::permission_gate::PermissionOutcome::Allow {
                    updated_input,
                    ..
                } => {
                    if let Some(u) = updated_input {
                        input = tool_use_ctx
                            .replace_input(u)
                            .map_err(|error| ToolInvokerError::InvalidInput(error.to_string()))?;
                    }
                }
                lingxi_core::host::permission_gate::PermissionOutcome::AllowAuto {
                    updated_input,
                } => {
                    // Only the session-owning main turn can atomically switch the
                    // live permission mode. Subagent dispatch has no such seam,
                    // so `AllowAuto` degrades to a one-shot allow for THIS call
                    // while still honoring any host/policy input rewrite.
                    if let Some(u) = updated_input {
                        input = tool_use_ctx
                            .replace_input(u)
                            .map_err(|error| ToolInvokerError::InvalidInput(error.to_string()))?;
                    }
                }
                lingxi_core::host::permission_gate::PermissionOutcome::Deny { reason } => {
                    return Err(ToolInvokerError::Internal(reason));
                }
            }
        }

        if tool.classifier_only().is_some() && self.gate.is_none() {
            return Err(ToolInvokerError::Internal(
                "Classifier-only tools require a bound permission gate".into(),
            ));
        }

        // A transport may rewrite input, but the one-file allowance must not
        // follow that rewrite onto another path or a changed filesystem link.
        if let Some(approved_path) = own_plan_path {
            let unchanged = input
                .get("file_path")
                .and_then(Value::as_str)
                .is_some_and(|path| std::path::Path::new(path) == approved_path)
                && lingxi_core::host::teammate_plan::own_plan_file_root(
                    ctx.parent_agent_id.as_ref(),
                    &approved_path,
                )
                .is_some();
            if !unchanged {
                return Err(ToolInvokerError::InvalidInput(
                    "The teammate plan-file target changed after permission was checked.".into(),
                ));
            }
        }

        // Drop the progress receiver immediately — production tools tolerate
        // a closed progress channel, and we don't surface progress here.
        let (progress_tx, _progress_rx) =
            tokio::sync::mpsc::channel::<crate::progress::ToolProgress>(8);

        // Preserve the concrete per-agent context that the tool actually
        // received. The Agent runner applies selected one-shot context
        // modifiers to this snapshot only after the tool-result batch settles.
        let context_state = ToolInvocationContextState::new(Arc::new(tool_use_ctx.clone()));

        let result = tool
            .call(input, tool_use_ctx, progress_tx)
            .await
            .map_err(|e| match e {
                crate::tool_trait::ToolError::InvalidInput(s) => ToolInvokerError::InvalidInput(s),
                other => ToolInvokerError::Internal(format!("{other}")),
            })?;

        let turn_end = crate::tool_trait::tool_result_turn_end(
            tool.result_ends_turn(&result),
            result.is_error,
            result.mcp_meta.as_ref(),
        );
        Ok(lingxi_core::host::tool_invoker::ToolInvocationResult {
            is_error: result.is_error,
            data: result.data,
            model_content: result.model_content,
            new_messages: result.new_messages,
            context_modifier: result.context_modifier.map(|modifier| {
                ToolInvocationContextModifier::new::<crate::context::ToolUseContext, _>(modifier)
            }),
            mcp_meta: result.mcp_meta,
            turn_end,
            context: lingxi_core::types::utf16_json::Utf16JsonProjection::plain(Value::Array(
                Vec::new(),
            )),
            context_state: Some(context_state),
        })
    }
}

#[cfg(test)]
#[allow(clippy::option_option)]
mod tests {
    use super::*;
    use crate::context::ToolUseContext;
    use crate::progress::ToolProgressSender;
    use crate::tool_trait::{
        DescriptionOptions, InterruptBehavior, PromptOptions, Tool, ToolCallResult, ToolError,
        ToolStaticContext, ValidationError,
    };
    use async_trait::async_trait;
    use once_cell::sync::Lazy;
    use permission::result::PermissionMetadata;
    use permission::{PermissionDecisionReason, PermissionResult};
    use serde_json::json;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn allow_for_tests() -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "test".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    static ECHO_INPUT_SCHEMA: Lazy<serde_json::Value> =
        Lazy::new(|| json!({ "type": "object", "additionalProperties": true }));
    static ECHO_OUTPUT_SCHEMA: Lazy<serde_json::Value> = Lazy::new(
        || json!({"type":"object","required":["echo"],"properties":{"echo":{"type":"object"}}}),
    );

    /// Minimal Tool impl returning input under {"echo": <input>}.
    /// Lives entirely under `#[cfg(test)]`.
    struct TestEchoTool {
        requires_user_interaction: bool,
    }

    #[async_trait]
    impl Tool for TestEchoTool {
        fn name(&self) -> &str {
            "TestEcho"
        }
        fn input_schema(&self) -> &serde_json::Value {
            &ECHO_INPUT_SCHEMA
        }
        fn output_schema(&self) -> Option<&serde_json::Value> {
            Some(&ECHO_OUTPUT_SCHEMA)
        }
        fn map_result_text(&self, result: &Value) -> Option<String> {
            result
                .get("echo")
                .and_then(Value::as_object)
                .and_then(|echo| echo.get("message"))
                .and_then(Value::as_str)
                .map(|message| format!("echo:{message}"))
        }
        fn is_enabled(&self, _: &ToolStaticContext) -> bool {
            true
        }
        fn requires_user_interaction(&self) -> bool {
            self.requires_user_interaction
        }
        fn max_result_size_chars(&self) -> usize {
            1024
        }
        fn is_concurrency_safe(&self, input: &serde_json::Value) -> bool {
            !input
                .get("force_serial")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        }
        fn is_read_only(&self, _: &serde_json::Value) -> bool {
            true
        }
        async fn validate_input(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
        ) -> PermissionResult {
            allow_for_tests()
        }
        async fn description(&self, _: &serde_json::Value, _: &DescriptionOptions) -> String {
            "echo".into()
        }
        async fn prompt(&self, _: &PromptOptions) -> String {
            "echo tool".into()
        }
        async fn call(
            &self,
            input: serde_json::Value,
            ctx: ToolUseContext,
            _progress_tx: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            ctx.projected_input(&input)
                .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
            Ok(ToolCallResult {
                is_error: input.get("fail").and_then(Value::as_bool).unwrap_or(false),
                data: json!({ "echo": input }),
                model_content: None,
                new_messages: vec![],
                context_modifier: None,
                mcp_meta: None,
            })
        }
        fn interrupt_behavior(&self, _input: &serde_json::Value) -> InterruptBehavior {
            InterruptBehavior::Cancel
        }
    }

    /// Probe used to lock the subagent tool-local permission chokepoint.  It
    /// can model MCP/Workflow-style local DENY/ASK results while recording
    /// whether the tool body was reached.
    struct ToolPermissionProbe {
        result: PermissionResult,
        is_mcp: bool,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for ToolPermissionProbe {
        fn name(&self) -> &str {
            "ToolPermissionProbe"
        }
        fn input_schema(&self) -> &serde_json::Value {
            &ECHO_INPUT_SCHEMA
        }
        fn is_enabled(&self, _: &ToolStaticContext) -> bool {
            true
        }
        fn is_mcp(&self) -> bool {
            self.is_mcp
        }
        fn max_result_size_chars(&self) -> usize {
            1024
        }
        fn is_concurrency_safe(&self, _: &serde_json::Value) -> bool {
            true
        }
        fn is_read_only(&self, _: &serde_json::Value) -> bool {
            false
        }
        async fn validate_input(
            &self,
            _: &serde_json::Value,
            _: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _: &serde_json::Value,
            _: &ToolUseContext,
        ) -> PermissionResult {
            self.result.clone()
        }
        async fn description(&self, _: &serde_json::Value, _: &DescriptionOptions) -> String {
            "permission probe".into()
        }
        async fn prompt(&self, _: &PromptOptions) -> String {
            String::new()
        }
        async fn call(
            &self,
            _: serde_json::Value,
            _: ToolUseContext,
            _: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ToolCallResult {
                data: json!({ "called": true }),
                model_content: None,
                new_messages: vec![],
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
            })
        }
    }

    static RECORDING_INPUT_SCHEMA: Lazy<serde_json::Value> =
        Lazy::new(|| json!({ "type": "object" }));

    /// Tool fixture that records the ToolUseContext.subagent_registry it
    /// receives so tests can introspect Arc identity.
    struct RecordingTool {
        captured: Arc<
            StdMutex<
                Option<(
                    Option<Arc<ToolRegistry>>,
                    Option<tokio_util::sync::CancellationToken>,
                )>,
            >,
        >,
    }

    #[async_trait]
    impl Tool for RecordingTool {
        fn name(&self) -> &str {
            "RecordingTool"
        }
        fn input_schema(&self) -> &serde_json::Value {
            &RECORDING_INPUT_SCHEMA
        }
        fn is_enabled(&self, _: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024
        }
        fn is_concurrency_safe(&self, _: &serde_json::Value) -> bool {
            true
        }
        fn is_read_only(&self, _: &serde_json::Value) -> bool {
            true
        }
        async fn validate_input(
            &self,
            _: &serde_json::Value,
            _: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _: &serde_json::Value,
            _: &ToolUseContext,
        ) -> PermissionResult {
            allow_for_tests()
        }
        async fn description(&self, _: &serde_json::Value, _: &DescriptionOptions) -> String {
            "rec".into()
        }
        async fn prompt(&self, _: &PromptOptions) -> String {
            "rec".into()
        }
        async fn call(
            &self,
            _: serde_json::Value,
            ctx: ToolUseContext,
            _: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            *self.captured.lock().unwrap() =
                Some((ctx.subagent_registry.clone(), ctx.cancel.clone()));
            Ok(ToolCallResult {
                data: json!({}),
                model_content: None,
                new_messages: vec![],
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
            })
        }
        fn interrupt_behavior(&self, _input: &serde_json::Value) -> InterruptBehavior {
            InterruptBehavior::Cancel
        }
    }

    fn registry_with_echo() -> Arc<ToolRegistry> {
        let mut r = ToolRegistry::new();
        r.register_builtin(Arc::new(TestEchoTool {
            requires_user_interaction: false,
        }));
        Arc::new(r)
    }

    #[test]
    fn registry_invoker_validates_mod_replacement_without_running_the_tool() {
        let invoker = RegistryToolInvoker::new(registry_with_echo());
        assert!(
            invoker
                .validate_output("TestEcho", &json!({"echo":{}}))
                .is_ok()
        );
        assert!(
            invoker
                .validate_output("TestEcho", &json!({"echo":42}))
                .is_err()
        );
        assert_eq!(
            invoker.map_result_text("TestEcho", &json!({"echo":{"message":"hello"}})),
            Some("echo:hello".into())
        );
    }

    fn registry_with_interactive_echo() -> Arc<ToolRegistry> {
        let mut r = ToolRegistry::new();
        r.register_builtin(Arc::new(TestEchoTool {
            requires_user_interaction: true,
        }));
        Arc::new(r)
    }

    // ──── existing M4-05 test (preserved) ──────────────
    #[test]
    fn registry_invoker_preserves_arc_identity() {
        let r = Arc::new(ToolRegistry::new());
        let inv = RegistryToolInvoker::new(r.clone());
        assert!(Arc::ptr_eq(&r, inv.registry_arc()));
    }

    // ──── Task 9: dispatch returns tool's data ──────────────
    #[tokio::test]
    async fn registry_invoker_routes_to_tool_call_and_returns_data() {
        let registry = registry_with_echo();
        let invoker = RegistryToolInvoker::new(registry.clone());

        let input = json!({ "hello": "world", "n": 42 });
        let ctx = no_ctx();

        let result = invoker
            .invoke("TestEcho", input.clone(), ctx)
            .await
            .expect("dispatch succeeds");

        assert_eq!(
            result,
            json!({ "echo": { "hello": "world", "n": 42 } }),
            "invoker returns the tool's data verbatim"
        );
    }

    #[tokio::test]
    async fn detailed_invocation_preserves_tool_reported_error() {
        let invoker = RegistryToolInvoker::new(registry_with_echo());
        for failed in [false, true] {
            let result = invoker
                .invoke_detailed("TestEcho", json!({"fail": failed}), no_ctx(), None)
                .await
                .expect("a tool-reported failure is still a completed invocation");
            assert_eq!(result.is_error, failed);
            assert_eq!(result.data, json!({"echo": {"fail": failed}}));
        }
    }

    /// Trusted outcome markers are configured by the host-side fixture; model
    /// input is only echoed into ordinary result data and model-facing text.
    struct TurnEndProbe {
        tool_marker: bool,
        is_error: bool,
        mcp_meta: Option<Value>,
    }

    #[async_trait]
    impl Tool for TurnEndProbe {
        fn name(&self) -> &str {
            "TurnEndProbe"
        }
        fn input_schema(&self) -> &Value {
            &ECHO_INPUT_SCHEMA
        }
        fn is_enabled(&self, _: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024
        }
        fn is_concurrency_safe(&self, _: &Value) -> bool {
            true
        }
        fn is_read_only(&self, _: &Value) -> bool {
            true
        }
        fn result_ends_turn(&self, result: &ToolCallResult) -> bool {
            assert_eq!(result.is_error, self.is_error);
            assert_eq!(result.mcp_meta, self.mcp_meta);
            assert_eq!(result.model_content, Some(result.data.to_string()));
            self.tool_marker
        }
        async fn validate_input(
            &self,
            _: &Value,
            _: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
            allow_for_tests()
        }
        async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
            "trusted result marker probe".into()
        }
        async fn prompt(&self, _: &PromptOptions) -> String {
            String::new()
        }
        async fn call(
            &self,
            input: Value,
            _: ToolUseContext,
            _: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            Ok(ToolCallResult {
                model_content: Some(input.to_string()),
                data: input,
                new_messages: vec![],
                context_modifier: None,
                is_error: self.is_error,
                mcp_meta: self.mcp_meta.clone(),
            })
        }
    }

    async fn invoke_turn_end_probe(
        tool_marker: bool,
        is_error: bool,
        mcp_meta: Option<Value>,
        input: Value,
    ) -> lingxi_core::host::tool_invoker::ToolInvocationResult {
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(TurnEndProbe {
            tool_marker,
            is_error,
            mcp_meta,
        }));
        let invoker = RegistryToolInvoker::new(Arc::new(registry));
        let result = invoker
            .invoke_detailed("TurnEndProbe", input.clone(), no_ctx(), None)
            .await
            .expect("probe invocation completes");
        assert_eq!(result.data, input);
        assert_eq!(result.model_content, Some(input.to_string()));
        assert_eq!(result.is_error, is_error);
        result
    }

    #[tokio::test]
    async fn detailed_invocation_carries_trusted_turn_end_with_native_precedence() {
        use crate::tool_trait::ToolResultTurnEndSource;
        for (tool_marker, mcp_meta, expected) in [
            (true, None, Some(ToolResultTurnEndSource::Tool)),
            (
                true,
                Some(json!({"_meta":{"claude/endTurn":true}})),
                Some(ToolResultTurnEndSource::Tool),
            ),
            (
                false,
                Some(json!({"_meta":{"claude/endTurn":true}})),
                Some(ToolResultTurnEndSource::McpMeta),
            ),
            (false, None, None),
        ] {
            let result =
                invoke_turn_end_probe(tool_marker, false, mcp_meta, json!({"payload":"visible"}))
                    .await;
            assert_eq!(result.turn_end.map(|marker| marker.source), expected);
        }
    }

    #[tokio::test]
    async fn detailed_error_result_never_ends_turn() {
        for tool_marker in [false, true] {
            for mcp_meta in [None, Some(json!({"_meta":{"claude/endTurn":true}}))] {
                let result =
                    invoke_turn_end_probe(tool_marker, true, mcp_meta, json!({"endsTurn":true}))
                        .await;
                assert!(result.turn_end.is_none());
            }
        }
    }

    #[tokio::test]
    async fn detailed_mcp_turn_end_requires_nested_strict_boolean() {
        for mcp_meta in [
            Value::Null,
            json!(true),
            json!({"claude/endTurn":true}),
            json!({"_meta":true}),
            json!({"_meta":{"claude/endTurn":false}}),
            json!({"_meta":{"claude/endTurn":"true"}}),
            json!({"_meta":{"claude/endTurn":1}}),
            json!({"_meta":{"claude/endTurn":null}}),
            json!({"_meta":{"claude/endTurn":[true]}}),
        ] {
            let result =
                invoke_turn_end_probe(false, false, Some(mcp_meta.clone()), json!({})).await;
            assert!(
                result.turn_end.is_none(),
                "invalid MCP marker accepted: {mcp_meta}"
            );
        }
    }

    #[tokio::test]
    async fn detailed_invocation_does_not_promote_model_input_or_result_data_to_turn_end() {
        for input in [
            json!({"endsTurn":true}),
            json!({"_meta":{"claude/endTurn":true}}),
            json!({"mcp_meta":{"_meta":{"claude/endTurn":true}}}),
            json!({"turn_end":{"source":"Tool"}}),
            json!({"data":{"endsTurn":true}}),
        ] {
            let result = invoke_turn_end_probe(false, false, None, input.clone()).await;
            assert!(
                result.turn_end.is_none(),
                "untrusted echoed input ended turn: {input}"
            );
        }
    }

    #[tokio::test]
    async fn registry_invoker_unknown_tool_surfaces_not_found() {
        let registry = registry_with_echo();
        let invoker = RegistryToolInvoker::new(registry);
        let result = invoker.invoke("NotARealTool", json!({}), no_ctx()).await;
        match result {
            Err(ToolInvokerError::NotFound(name)) => assert_eq!(name, "NotARealTool"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn concurrency_safety_comes_from_the_resolved_registry_tool() {
        let invoker = RegistryToolInvoker::new(registry_with_echo());
        assert_eq!(
            ToolInvoker::tool_is_concurrency_safe(&invoker, "TestEcho", &json!({})),
            Some(true)
        );
        assert_eq!(
            ToolInvoker::tool_is_concurrency_safe(
                &invoker,
                "TestEcho",
                &json!({"force_serial": true})
            ),
            Some(false)
        );
        assert_eq!(
            ToolInvoker::tool_is_concurrency_safe(&invoker, "NotARealTool", &json!({})),
            None
        );
    }

    #[tokio::test]
    async fn session_allowlist_blocks_final_dispatch() {
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(TestEchoTool {
            requires_user_interaction: false,
        }));
        registry.set_session_tool_allowlist(&["Read".to_string()]);
        let invoker = RegistryToolInvoker::new(Arc::new(registry));

        let result = invoker.invoke("TestEcho", json!({}), no_ctx()).await;
        assert!(
            matches!(result, Err(ToolInvokerError::NotFound(ref name)) if name == "TestEcho"),
            "a tool hidden by the session capability profile must not dispatch: {result:?}"
        );
    }

    // ──── Task 10: dispatch preserves subagent_registry Arc identity ──────────────
    #[tokio::test]
    async fn registry_invoker_preserves_subagent_registry_arc_into_tool_use_ctx() {
        let captured: Arc<
            StdMutex<
                Option<(
                    Option<Arc<ToolRegistry>>,
                    Option<tokio_util::sync::CancellationToken>,
                )>,
            >,
        > = Arc::new(StdMutex::new(None));

        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(RecordingTool {
            captured: captured.clone(),
        }));
        let parent_registry = Arc::new(registry);

        let invoker = RegistryToolInvoker::new(parent_registry.clone());
        invoker
            .invoke("RecordingTool", json!({}), no_ctx())
            .await
            .expect("dispatch ok");

        let captured = captured.lock().unwrap();
        let (inner, _) = captured.as_ref().expect("RecordingTool::call ran");
        let registry_in_ctx = inner.as_ref().expect("subagent_registry was Some");
        assert!(
            Arc::ptr_eq(&parent_registry, registry_in_ctx),
            "RegistryToolInvoker must thread the parent Arc<ToolRegistry> into ToolUseContext.subagent_registry verbatim — this preserves the M4-05 recursion-lock contract across the dispatch boundary"
        );
    }

    #[tokio::test]
    async fn per_call_cancellation_reaches_normal_and_workspace_lease_tool_contexts() {
        let captured: Arc<
            StdMutex<
                Option<(
                    Option<Arc<ToolRegistry>>,
                    Option<tokio_util::sync::CancellationToken>,
                )>,
            >,
        > = Arc::new(StdMutex::new(None));
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(RecordingTool {
            captured: captured.clone(),
        }));
        let invoker = RegistryToolInvoker::new(Arc::new(registry));

        for workspace_lease_token in [None, Some(41)] {
            *captured.lock().unwrap() = None;
            let cancellation_token = tokio_util::sync::CancellationToken::new();
            let mut context = no_ctx();
            context.cancellation_token = cancellation_token.clone();
            let stale_token = if workspace_lease_token.is_some() {
                let stale_token = tokio_util::sync::CancellationToken::new();
                let mut stale_context = ToolUseContext::model_seed("prior-call".into(), None);
                stale_context.cancel = Some(stale_token.clone());
                context.tool_context_state =
                    Some(ToolInvocationContextState::new(Arc::new(stale_context)));
                Some(stale_token)
            } else {
                None
            };

            if let Some(token) = workspace_lease_token {
                invoker
                    .invoke_with_workspace_lease("RecordingTool", json!({}), context, Some(token))
                    .await
                    .expect("workspace-leased tool call succeeds");
            } else {
                invoker
                    .invoke("RecordingTool", json!({}), context)
                    .await
                    .expect("normal tool call succeeds");
            }

            let observed_token = captured
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|(_, token)| token.clone())
                .expect("tool receives the per-call cancellation token");
            assert!(!observed_token.is_cancelled());
            cancellation_token.cancel();
            assert!(
                observed_token.is_cancelled(),
                "the tool context token must share the caller's cancellation scope"
            );
            if let Some(stale_token) = stale_token {
                assert!(
                    !stale_token.is_cancelled(),
                    "a prior ToolUseContext snapshot must not leak its old cancellation scope"
                );
            }
        }
    }

    // ──── swarm identity: SubagentInvocationContext name/team → ToolUseContext ────

    /// Tool fixture that records the identity the dispatched `ToolUseContext`
    /// carries, so a test can assert swarm and originating-session identity
    /// flow through the invoker.
    struct NameRecordingTool {
        captured: Arc<
            StdMutex<
                Option<(
                    Option<String>,
                    Option<String>,
                    Option<lingxi_core::types::MessageId>,
                    Option<lingxi_core::types::SessionId>,
                    lingxi_core::host::tool_invoker::ToolExecutionPolicy,
                    Vec<lingxi_core::types::ConversationMessage>,
                    Option<String>,
                    Option<lingxi_core::host::instructions::InstructionContext>,
                    Option<lingxi_core::types::ConversationMessage>,
                    Vec<lingxi_core::types::ContentBlock>,
                )>,
            >,
        >,
    }
    #[async_trait]
    impl Tool for NameRecordingTool {
        fn name(&self) -> &str {
            "NameRecordingTool"
        }
        fn input_schema(&self) -> &serde_json::Value {
            &RECORDING_INPUT_SCHEMA
        }
        fn is_enabled(&self, _: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024
        }
        fn is_concurrency_safe(&self, _: &serde_json::Value) -> bool {
            true
        }
        fn is_read_only(&self, _: &serde_json::Value) -> bool {
            true
        }
        async fn validate_input(
            &self,
            _: &serde_json::Value,
            _: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _: &serde_json::Value,
            _: &ToolUseContext,
        ) -> PermissionResult {
            allow_for_tests()
        }
        async fn description(&self, _: &serde_json::Value, _: &DescriptionOptions) -> String {
            "rec".into()
        }
        async fn prompt(&self, _: &PromptOptions) -> String {
            "rec".into()
        }
        async fn call(
            &self,
            _: serde_json::Value,
            ctx: ToolUseContext,
            _: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            *self.captured.lock().unwrap() = Some((
                ctx.agent_name.clone(),
                ctx.team_name.clone(),
                ctx.assistant_message_id,
                ctx.origin_session_id,
                ctx.tool_execution_policy,
                ctx.messages,
                ctx.fork_parent_system_prompt,
                ctx.instruction_context,
                ctx.assistant_message,
                ctx.same_turn_tool_uses,
            ));
            Ok(ToolCallResult {
                data: json!({}),
                model_content: None,
                new_messages: vec![],
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
            })
        }
        fn interrupt_behavior(&self, _input: &serde_json::Value) -> InterruptBehavior {
            InterruptBehavior::Cancel
        }
    }

    /// The `agent_name` / `team_name` carried by the `SubagentInvocationContext`
    /// must reach the dispatched tool's `ToolUseContext` verbatim — this is what
    /// makes the merged swarm `TaskUpdate` auto-owner / `getTaskListId` logic
    /// actually fire for an in-process teammate (it keys on these).
    #[tokio::test]
    async fn registry_invoker_threads_swarm_identity_into_tool_use_ctx() {
        let captured = Arc::new(StdMutex::new(None));
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(NameRecordingTool {
            captured: captured.clone(),
        }));
        let invoker = RegistryToolInvoker::new(Arc::new(registry));
        let assistant_message_id = lingxi_core::types::MessageId::new();
        let origin_session_id = lingxi_core::types::SessionId::new();
        let policy = lingxi_core::types::ConversationMessage::user_meta(
            lingxi_core::types::MessageId::new(),
            "nested instruction body".into(),
        );
        let current_tool_use = lingxi_core::types::ContentBlock::ToolUse {
            id: lingxi_core::types::ToolUseId::new(),
            name: "Agent".into(),
            input: json!({"description":"current row"}),
            provider_id: Some("provider-current".into()),
        };
        let current_assistant_row = lingxi_core::types::ConversationMessage::Assistant {
            id: lingxi_core::types::MessageId::new(),
            content: vec![current_tool_use],
            stop_reason: Some("tool_use".into()),
        };
        let prior_sibling_tool_use = lingxi_core::types::ContentBlock::ToolUse {
            id: lingxi_core::types::ToolUseId::new(),
            name: "Read".into(),
            input: json!({"file_path":"prior.txt"}),
            provider_id: Some("provider-prior".into()),
        };
        let mut stale_context = crate::context::ToolUseContext::model_seed("stale".into(), None);
        stale_context.assistant_message =
            Some(lingxi_core::types::ConversationMessage::Assistant {
                id: lingxi_core::types::MessageId::new(),
                content: vec![lingxi_core::types::ContentBlock::Text {
                    text: "stale assistant row".into(),
                    citations: None,
                }],
                stop_reason: None,
            });
        stale_context.same_turn_tool_uses = vec![lingxi_core::types::ContentBlock::Text {
            text: "stale sibling".into(),
            citations: None,
        }];
        let mut instruction_context =
            lingxi_core::host::instructions::InstructionContext::default();
        instruction_context
            .sent_paths
            .insert("/project/pkg/AGENTS.md".into());

        let mut invocation = SubagentInvocationContext {
            input_projection: None,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            permission_pause_observer: None,
            parent_agent_id: None,
            origin_session_id: Some(origin_session_id),
            instruction_context: Some(instruction_context.clone()),
            fork_context: Some(lingxi_core::host::tool_invoker::SubagentForkContext {
                messages: vec![policy.clone()],
                system_prompt: Some("exact parent system".into()),
            }),
            tool_execution_policy: lingxi_core::host::tool_invoker::ToolExecutionPolicy::Ordinary,
            agent_name: Some("researcher".to_string()),
            team_name: Some("alpha".to_string()),
            is_async: false,
            is_non_interactive_session: false,
            can_show_permission_prompts: true,
            cwd: None,
            tool_use_id: None,
            assistant_message_id: Some(assistant_message_id),
            depth: 0,
            observer: None,
            parent_model: None,
            parent_model_profile: None,
            agent_spawn_provenance: Default::default(),
            tool_context_state: Some(ToolInvocationContextState::new(Arc::new(stale_context))),
            assistant_message: Some(current_assistant_row.clone()),
            same_turn_tool_uses: vec![prior_sibling_tool_use.clone()],
            current_history: vec![policy.clone()],
            mode_override: None,
            request_source: None,
            frozen_command_denies: Vec::new(),
        };
        invoker
            .invoke("NameRecordingTool", json!({}), invocation.clone())
            .await
            .expect("dispatch ok");

        let captured_guard = captured.lock().unwrap();
        let (
            agent_name,
            team_name,
            captured_message_id,
            captured_origin_session_id,
            captured_policy,
            captured_messages,
            captured_system_prompt,
            captured_instructions,
            captured_assistant_message,
            captured_same_turn_tool_uses,
        ) = captured_guard
            .as_ref()
            .expect("NameRecordingTool::call ran");
        assert_eq!(
            agent_name.as_deref(),
            Some("researcher"),
            "the teammate DISPLAY name reaches ToolUseContext.agent_name (getAgentName())"
        );
        assert_eq!(
            team_name.as_deref(),
            Some("alpha"),
            "the team name reaches ToolUseContext.team_name (getTeammateContext()?.teamName)"
        );
        assert_eq!(
            *captured_message_id,
            Some(assistant_message_id),
            "the current assistant message id reaches ToolUseContext unchanged"
        );
        assert_eq!(
            *captured_origin_session_id,
            Some(origin_session_id),
            "the trusted originating session reaches ToolUseContext unchanged"
        );
        assert_eq!(
            *captured_policy,
            lingxi_core::host::tool_invoker::ToolExecutionPolicy::Ordinary,
            "the trusted execution policy reaches ToolUseContext unchanged"
        );
        assert_eq!(captured_messages, &vec![policy]);
        assert_eq!(
            captured_assistant_message,
            &Some(current_assistant_row),
            "the current raw assistant row stays separate from the base history"
        );
        assert_eq!(
            captured_same_turn_tool_uses,
            &vec![prior_sibling_tool_use],
            "only earlier siblings are refreshed, excluding stale snapshot and current row"
        );
        assert_eq!(
            captured_system_prompt.as_deref(),
            Some("exact parent system")
        );
        assert_eq!(captured_instructions.as_ref(), Some(&instruction_context));
        drop(captured_guard);

        let next_policy = lingxi_core::types::ConversationMessage::user_meta(
            lingxi_core::types::MessageId::new(),
            "next query history".into(),
        );
        let next_assistant_row = lingxi_core::types::ConversationMessage::Assistant {
            id: lingxi_core::types::MessageId::new(),
            content: vec![lingxi_core::types::ContentBlock::ToolUse {
                id: lingxi_core::types::ToolUseId::new(),
                name: "Agent".into(),
                input: json!({"description":"next current row"}),
                provider_id: None,
            }],
            stop_reason: Some("tool_use".into()),
        };
        let next_sibling_tool_use = lingxi_core::types::ContentBlock::ToolUse {
            id: lingxi_core::types::ToolUseId::new(),
            name: "Read".into(),
            input: json!({"file_path":"next-prior.txt"}),
            provider_id: None,
        };
        invocation.current_history = vec![next_policy.clone()];
        invocation.assistant_message = Some(next_assistant_row.clone());
        invocation.same_turn_tool_uses = vec![next_sibling_tool_use.clone()];
        invoker
            .invoke("NameRecordingTool", json!({}), invocation)
            .await
            .expect("second dispatch ok");

        let captured = captured.lock().unwrap();
        let captured = captured.as_ref().expect("second tool call ran");
        assert_eq!(captured.5, vec![next_policy]);
        assert_eq!(captured.8, Some(next_assistant_row));
        assert_eq!(captured.9, vec![next_sibling_tool_use]);
    }

    #[tokio::test]
    async fn workspace_lease_dispatch_preserves_trusted_execution_policy() {
        let captured = Arc::new(StdMutex::new(None));
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(NameRecordingTool {
            captured: captured.clone(),
        }));
        let invoker = RegistryToolInvoker::new(Arc::new(registry));
        invoker
            .invoke_with_workspace_lease(
                "NameRecordingTool",
                json!({}),
                SubagentInvocationContext {
                    input_projection: None,
                    cancellation_token: tokio_util::sync::CancellationToken::new(),
                    permission_pause_observer: None,
                    parent_agent_id: None,
                    origin_session_id: None,
                    instruction_context: None,
                    fork_context: None,
                    tool_execution_policy:
                        lingxi_core::host::tool_invoker::ToolExecutionPolicy::FusionPanel,
                    agent_name: None,
                    team_name: None,
                    is_async: false,
                    is_non_interactive_session: false,
                    can_show_permission_prompts: false,
                    cwd: None,
                    tool_use_id: None,
                    assistant_message_id: None,
                    depth: 0,
                    observer: None,
                    parent_model: None,
                    parent_model_profile: None,
                    agent_spawn_provenance: Default::default(),
                    tool_context_state: None,
                    assistant_message: None,
                    same_turn_tool_uses: Vec::new(),
                    current_history: Vec::new(),
                    mode_override: None,
                    request_source: None,
                    frozen_command_denies: Vec::new(),
                },
                Some(77),
            )
            .await
            .expect("lease-aware dispatch succeeds");
        let captured = captured.lock().unwrap();
        let captured = captured.as_ref().expect("tool call ran");
        assert_eq!(
            captured.4,
            lingxi_core::host::tool_invoker::ToolExecutionPolicy::FusionPanel,
            "workspace lease path must not drop the trusted Fusion policy"
        );
    }

    /// Tool fixture that records the `main_loop_model` the dispatched
    /// `ToolUseContext` carries, so a test can assert the DISPATCHING subagent's
    /// resolved model reaches a recursive tool call (claude-code
    /// `AgentTool.tsx:418` `toolUseContext.options.mainLoopModel`).
    struct ModelRecordingTool {
        captured: Arc<StdMutex<Option<(String, Option<String>)>>>,
    }
    #[async_trait]
    impl Tool for ModelRecordingTool {
        fn name(&self) -> &str {
            "ModelRecordingTool"
        }
        fn input_schema(&self) -> &serde_json::Value {
            &RECORDING_INPUT_SCHEMA
        }
        fn is_enabled(&self, _: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024
        }
        fn is_concurrency_safe(&self, _: &serde_json::Value) -> bool {
            true
        }
        fn is_read_only(&self, _: &serde_json::Value) -> bool {
            true
        }
        async fn validate_input(
            &self,
            _: &serde_json::Value,
            _: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _: &serde_json::Value,
            _: &ToolUseContext,
        ) -> PermissionResult {
            allow_for_tests()
        }
        async fn description(&self, _: &serde_json::Value, _: &DescriptionOptions) -> String {
            "rec".into()
        }
        async fn prompt(&self, _: &PromptOptions) -> String {
            "rec".into()
        }
        async fn call(
            &self,
            _: serde_json::Value,
            ctx: ToolUseContext,
            _: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            *self.captured.lock().unwrap() = Some((
                ctx.options.main_loop_model.clone(),
                ctx.options.model_profile.clone(),
            ));
            Ok(ToolCallResult {
                data: json!({}),
                model_content: None,
                new_messages: vec![],
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
            })
        }
        fn interrupt_behavior(&self, _input: &serde_json::Value) -> InterruptBehavior {
            InterruptBehavior::Cancel
        }
    }

    /// The DISPATCHING subagent's own resolved model
    /// ([`SubagentInvocationContext::parent_model`]) must reach the dispatched
    /// tool's `ToolUseContext.options.main_loop_model` — this is what makes a
    /// NESTED `Agent` tool call resolve its child's model against the IMMEDIATE
    /// parent's model (claude-code `runAgent.ts:678` → `AgentTool.tsx:418`). When
    /// unset it falls back to the legacy `"subagent"` placeholder.
    #[tokio::test]
    async fn registry_invoker_threads_parent_model_into_main_loop_model() {
        let captured: Arc<StdMutex<Option<(String, Option<String>)>>> =
            Arc::new(StdMutex::new(None));
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(ModelRecordingTool {
            captured: captured.clone(),
        }));
        let invoker = RegistryToolInvoker::new(Arc::new(registry));

        let mut ctx = no_ctx();
        ctx.parent_model = Some("claude-sonnet-5".to_string());
        ctx.parent_model_profile = Some("anthropic".to_string());
        invoker
            .invoke("ModelRecordingTool", json!({}), ctx)
            .await
            .expect("dispatch ok");
        assert_eq!(
            captured.lock().unwrap().as_ref(),
            Some(&("claude-sonnet-5".to_string(), Some("anthropic".to_string()))),
            "the dispatching subagent's model and provider reach ToolUseContext options"
        );

        // Unset parent_model ⇒ the legacy placeholder (byte-identical fallback).
        *captured.lock().unwrap() = None;
        invoker
            .invoke("ModelRecordingTool", json!({}), no_ctx())
            .await
            .expect("dispatch ok");
        assert_eq!(
            captured.lock().unwrap().as_ref(),
            Some(&("subagent".to_string(), None))
        );
    }

    // ──── enforcement 3b: permission gate before dispatch ──────────────
    use lingxi_core::host::permission_gate::{
        PermissionDecision as GateDecision, PermissionGate as Gate,
    };

    /// Gate that returns a fixed decision and records each tool name it sees.
    struct FixedGate {
        decision: GateDecision,
        seen: Arc<StdMutex<Vec<String>>>,
    }
    #[async_trait]
    impl Gate for FixedGate {
        async fn check(&self, name: &str, _input: &Value) -> GateDecision {
            self.seen.lock().unwrap().push(name.to_string());
            self.decision.clone()
        }
    }

    fn no_ctx() -> SubagentInvocationContext {
        SubagentInvocationContext {
            input_projection: None,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            permission_pause_observer: None,
            parent_agent_id: None,
            origin_session_id: None,
            instruction_context: None,
            fork_context: None,
            tool_execution_policy: lingxi_core::host::tool_invoker::ToolExecutionPolicy::Ordinary,
            agent_name: None,
            team_name: None,
            is_async: false,
            is_non_interactive_session:
                lingxi_core::host::session_flags::effective_non_interactive_session(),
            can_show_permission_prompts: false,
            cwd: None,
            tool_use_id: None,
            assistant_message_id: None,
            depth: 0,
            observer: None,
            parent_model: None,
            parent_model_profile: None,
            agent_spawn_provenance: Default::default(),
            tool_context_state: None,
            assistant_message: None,
            same_turn_tool_uses: Vec::new(),
            current_history: Vec::new(),
            mode_override: None,
            request_source: None,
            frozen_command_denies: Vec::new(),
        }
    }

    struct SessionModeRecordingTool {
        captured: Arc<StdMutex<Option<bool>>>,
    }

    #[async_trait]
    impl Tool for SessionModeRecordingTool {
        fn name(&self) -> &str {
            "SessionModeRecordingTool"
        }
        fn input_schema(&self) -> &serde_json::Value {
            &RECORDING_INPUT_SCHEMA
        }
        fn is_enabled(&self, _: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024
        }
        fn is_concurrency_safe(&self, _: &serde_json::Value) -> bool {
            true
        }
        fn is_read_only(&self, _: &serde_json::Value) -> bool {
            true
        }
        async fn validate_input(
            &self,
            _: &serde_json::Value,
            _: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _: &serde_json::Value,
            _: &ToolUseContext,
        ) -> PermissionResult {
            allow_for_tests()
        }
        async fn description(&self, _: &serde_json::Value, _: &DescriptionOptions) -> String {
            "record session mode".into()
        }
        async fn prompt(&self, _: &PromptOptions) -> String {
            "record session mode".into()
        }
        async fn call(
            &self,
            _: serde_json::Value,
            ctx: ToolUseContext,
            _: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            *self.captured.lock().unwrap() = Some(ctx.options.is_non_interactive_session);
            Ok(ToolCallResult {
                data: json!({}),
                model_content: None,
                new_messages: vec![],
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
            })
        }
        fn interrupt_behavior(&self, _input: &serde_json::Value) -> InterruptBehavior {
            InterruptBehavior::Cancel
        }
    }

    #[tokio::test]
    async fn synchronous_subagent_tools_inherit_scheduled_headless_session_mode() {
        let captured = Arc::new(StdMutex::new(None));
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(SessionModeRecordingTool {
            captured: captured.clone(),
        }));
        let invoker = RegistryToolInvoker::new(Arc::new(registry));

        lingxi_core::host::session_flags::scope_non_interactive_session(true, async {
            invoker
                .invoke("SessionModeRecordingTool", json!({}), no_ctx())
                .await
                .expect("dispatch ok");
        })
        .await;

        assert_eq!(
            *captured.lock().unwrap(),
            Some(true),
            "a synchronous child must inherit its scheduled-headless owner's mode"
        );
    }

    #[tokio::test]
    async fn gate_deny_blocks_dispatch_and_surfaces_reason() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let gate = Arc::new(FixedGate {
            decision: GateDecision::Deny {
                reason: "denied by permission rule Bash".into(),
            },
            seen: seen.clone(),
        });
        let invoker = RegistryToolInvoker::new(registry_with_echo()).with_gate(gate);
        let mut ctx = no_ctx();
        ctx.tool_execution_policy =
            lingxi_core::host::tool_invoker::ToolExecutionPolicy::FusionPanel;
        match invoker.invoke("TestEcho", json!({ "a": 1 }), ctx).await {
            Err(ToolInvokerError::Internal(reason)) => {
                assert!(
                    reason.contains("denied by permission rule Bash"),
                    "got {reason}"
                );
            }
            other => panic!("expected Internal(deny), got {other:?}"),
        }
        // Gate was consulted; the tool was NOT run (echo would have returned data).
        assert_eq!(seen.lock().unwrap().as_slice(), ["TestEcho"]);
    }

    #[tokio::test]
    async fn gate_allow_dispatches_to_tool() {
        let gate = Arc::new(FixedGate {
            decision: GateDecision::Allow,
            seen: Arc::new(StdMutex::new(Vec::new())),
        });
        let invoker = RegistryToolInvoker::new(registry_with_echo()).with_gate(gate);
        let out = invoker
            .invoke("TestEcho", json!({ "k": "v" }), no_ctx())
            .await
            .expect("allow dispatches");
        assert_eq!(out, json!({ "echo": { "k": "v" } }));
    }

    #[tokio::test]
    async fn no_gate_dispatches_unconditionally_legacy() {
        // Default `new` (no gate) preserves the pre-3b always-dispatch behavior.
        let invoker = RegistryToolInvoker::new(registry_with_echo());
        let out = invoker
            .invoke("TestEcho", json!({}), no_ctx())
            .await
            .expect("legacy dispatch");
        assert_eq!(out, json!({ "echo": {} }));
    }

    /// Gate that records the [`PromptWorker`] handed to `check_with_worker`.
    struct WorkerRecordingGate {
        seen: Arc<StdMutex<Option<Option<lingxi_core::host::permission_gate::PromptWorker>>>>,
    }
    #[async_trait]
    impl Gate for WorkerRecordingGate {
        async fn check(&self, _name: &str, _input: &Value) -> GateDecision {
            // `check_with_worker`'s default would land here; record None so a
            // missing override is visible. The real override is below.
            *self.seen.lock().unwrap() = Some(None);
            GateDecision::Allow
        }
        async fn check_with_worker(
            &self,
            _name: &str,
            _input: &Value,
            worker: Option<lingxi_core::host::permission_gate::PromptWorker>,
        ) -> GateDecision {
            *self.seen.lock().unwrap() = Some(worker);
            GateDecision::Allow
        }
    }

    fn named_ctx(can_show: bool) -> SubagentInvocationContext {
        SubagentInvocationContext {
            input_projection: None,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            permission_pause_observer: None,
            parent_agent_id: None,
            origin_session_id: None,
            instruction_context: None,
            fork_context: None,
            tool_execution_policy: lingxi_core::host::tool_invoker::ToolExecutionPolicy::Ordinary,
            agent_name: Some("researcher".to_string()),
            team_name: Some("alpha".to_string()),
            is_async: true,
            is_non_interactive_session: true,
            can_show_permission_prompts: can_show,
            cwd: None,
            tool_use_id: None,
            assistant_message_id: None,
            depth: 0,
            observer: None,
            parent_model: None,
            parent_model_profile: None,
            agent_spawn_provenance: Default::default(),
            tool_context_state: None,
            assistant_message: None,
            same_turn_tool_uses: Vec::new(),
            current_history: Vec::new(),
            mode_override: None,
            request_source: None,
            frozen_command_denies: Vec::new(),
        }
    }

    #[tokio::test]
    async fn worker_identity_threads_to_gate_when_named_and_eligible() {
        let seen = Arc::new(StdMutex::new(None));
        let gate = Arc::new(WorkerRecordingGate { seen: seen.clone() });
        let invoker = RegistryToolInvoker::new(registry_with_echo()).with_gate(gate);
        invoker
            .invoke("TestEcho", json!({}), named_ctx(true))
            .await
            .expect("allow dispatches");
        let worker = seen.lock().unwrap().clone().expect("gate consulted");
        let worker = worker.expect("a named, prompt-eligible worker is attributed");
        assert_eq!(worker.name, "researcher");
        assert_eq!(worker.team.as_deref(), Some("alpha"));
        assert!(worker.is_async);
    }

    #[tokio::test]
    async fn no_worker_attribution_for_unnamed_or_ineligible() {
        // Unnamed (one-shot subagent) → no attribution.
        let seen = Arc::new(StdMutex::new(None));
        let gate = Arc::new(WorkerRecordingGate { seen: seen.clone() });
        RegistryToolInvoker::new(registry_with_echo())
            .with_gate(gate)
            .invoke("TestEcho", json!({}), no_ctx())
            .await
            .expect("ok");
        assert_eq!(seen.lock().unwrap().clone().expect("consulted"), None);

        // Named but NOT prompt-eligible (`can_show_permission_prompts = false`)
        // → no worker chrome.
        let seen2 = Arc::new(StdMutex::new(None));
        let gate2 = Arc::new(WorkerRecordingGate {
            seen: seen2.clone(),
        });
        RegistryToolInvoker::new(registry_with_echo())
            .with_gate(gate2)
            .invoke("TestEcho", json!({}), named_ctx(false))
            .await
            .expect("ok");
        assert_eq!(seen2.lock().unwrap().clone().expect("consulted"), None);
    }

    /// Gate that overrides `check_with_context` to record the
    /// [`PermissionCheckContext`] it sees and return a fixed
    /// [`PermissionOutcome`] — so a test can assert the subagent dispatch path
    /// threads the REAL `tool_use_id` AND honors a host `updatedInput` rewrite
    /// (the parity gap the stdio `can_use_tool` flow needs), mirroring the main
    /// loop's Ask arm.
    struct ContextRecordingGate {
        seen: Arc<StdMutex<Option<lingxi_core::host::permission_gate::PermissionCheckContext>>>,
        outcome: lingxi_core::host::permission_gate::PermissionOutcome,
    }
    #[async_trait]
    impl Gate for ContextRecordingGate {
        async fn check(&self, _name: &str, _input: &Value) -> GateDecision {
            // Must not be reached — the override below intercepts the dispatch.
            GateDecision::Allow
        }
        async fn check_with_context(
            &self,
            _name: &str,
            _input: &Value,
            ctx: &lingxi_core::host::permission_gate::PermissionCheckContext,
        ) -> lingxi_core::host::permission_gate::PermissionOutcome {
            *self.seen.lock().unwrap() = Some(ctx.clone());
            self.outcome.clone()
        }
    }

    struct AbortGate {
        seen: Arc<StdMutex<Option<lingxi_core::host::permission_gate::PermissionCheckContext>>>,
    }

    #[async_trait]
    impl Gate for AbortGate {
        async fn check(&self, _name: &str, _input: &Value) -> GateDecision {
            panic!("abort-aware dispatch must not fall back to the legacy gate method")
        }

        async fn check_with_context_or_abort(
            &self,
            _name: &str,
            _input: &Value,
            ctx: &lingxi_core::host::permission_gate::PermissionCheckContext,
        ) -> Result<
            lingxi_core::host::permission_gate::PermissionOutcome,
            lingxi_core::host::permission_gate::PermissionAbort,
        > {
            *self.seen.lock().unwrap() = Some(ctx.clone());
            Err(lingxi_core::host::permission_gate::PermissionAbort {
                message: "Agent aborted: too many classifier denials in headless mode".into(),
            })
        }
    }

    struct AllowAutoNoTransitionGate;

    #[async_trait]
    impl Gate for AllowAutoNoTransitionGate {
        async fn check(&self, _name: &str, _input: &Value) -> GateDecision {
            panic!("AllowAuto test must use the rich context path")
        }

        async fn check_with_context(
            &self,
            _name: &str,
            _input: &Value,
            _ctx: &lingxi_core::host::permission_gate::PermissionCheckContext,
        ) -> lingxi_core::host::permission_gate::PermissionOutcome {
            lingxi_core::host::permission_gate::PermissionOutcome::AllowAuto {
                updated_input: Some(lingxi_core::types::utf16_json::Utf16JsonProjection::plain(
                    json!({ "rewritten": "auto" }),
                )),
            }
        }

        async fn set_permission_mode(&self, _mode: &str) -> Result<(), String> {
            panic!("subagent tool dispatch must not attempt a session auto-mode transition")
        }
    }

    fn ctx_with_tool_use_id(id: &str) -> SubagentInvocationContext {
        SubagentInvocationContext {
            input_projection: None,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            permission_pause_observer: None,
            parent_agent_id: None,
            origin_session_id: None,
            instruction_context: None,
            fork_context: None,
            tool_execution_policy: lingxi_core::host::tool_invoker::ToolExecutionPolicy::Ordinary,
            agent_name: Some("researcher".to_string()),
            team_name: Some("alpha".to_string()),
            is_async: true,
            is_non_interactive_session: true,
            can_show_permission_prompts: true,
            cwd: None,
            tool_use_id: Some(id.to_string()),
            assistant_message_id: None,
            depth: 0,
            observer: None,
            parent_model: None,
            parent_model_profile: None,
            agent_spawn_provenance: Default::default(),
            tool_context_state: None,
            assistant_message: None,
            same_turn_tool_uses: Vec::new(),
            current_history: Vec::new(),
            mode_override: None,
            request_source: None,
            frozen_command_denies: Vec::new(),
        }
    }

    #[tokio::test]
    async fn dispatch_threads_real_tool_use_id_and_worker_into_context_gate() {
        // The dispatching call's tool_use block id (and the worker attribution)
        // must reach the gate's PermissionCheckContext so a subagent's stdio
        // `can_use_tool` request is byte-faithful (claude-code
        // `createCanUseTool(toolUseID)`), not a freshly minted id.
        let seen = Arc::new(StdMutex::new(None));
        let gate = Arc::new(ContextRecordingGate {
            seen: seen.clone(),
            outcome: lingxi_core::host::permission_gate::PermissionOutcome::Allow {
                updated_input: None,
                permission_updates: Vec::new(),
                decision_classification: None,
            },
        });
        let invoker = RegistryToolInvoker::new(registry_with_echo()).with_gate(gate);
        let pause = lingxi_core::host::permission_gate::PermissionPauseObserver::new(|_| {});
        let mut invocation_context = ctx_with_tool_use_id("toolu_abc123");
        invocation_context.permission_pause_observer = Some(pause.clone());
        invoker
            .invoke("TestEcho", json!({ "a": 1 }), invocation_context)
            .await
            .expect("allow dispatches");
        let ctx = seen
            .lock()
            .unwrap()
            .clone()
            .expect("context gate consulted");
        assert_eq!(
            ctx.tool_use_id.as_deref(),
            Some("toolu_abc123"),
            "the dispatching call's real tool_use_id reaches PermissionCheckContext.tool_use_id"
        );
        assert_eq!(
            ctx.pause_observer,
            Some(pause),
            "real worker pause sink must reach the transport"
        );
        assert!(!ctx.requires_user_interaction);
        let worker = ctx
            .worker
            .expect("a named, prompt-eligible worker is attributed");
        assert_eq!(worker.name, "researcher");
        assert_eq!(worker.team.as_deref(), Some("alpha"));
        assert!(worker.is_async);
    }

    #[tokio::test]
    async fn dispatch_preserves_permission_abort_as_terminal_error() {
        let seen = Arc::new(StdMutex::new(None));
        let gate = Arc::new(AbortGate { seen: seen.clone() });
        let invoker = RegistryToolInvoker::new(registry_with_echo()).with_gate(gate);

        let error = invoker
            .invoke("TestEcho", json!({}), ctx_with_tool_use_id("toolu_abort"))
            .await
            .expect_err("a permission abort must stop before tool dispatch");

        assert!(matches!(
            error,
            ToolInvokerError::Abort(ref message)
                if message == "Agent aborted: too many classifier denials in headless mode"
        ));
        let ctx = seen.lock().unwrap().clone().expect("gate consulted");
        assert!(ctx.is_non_interactive_session);
        assert_eq!(ctx.tool_use_id.as_deref(), Some("toolu_abort"));
    }

    #[tokio::test]
    async fn dispatch_threads_tool_interaction_requirement_into_context_gate() {
        let seen = Arc::new(StdMutex::new(None));
        let gate = Arc::new(ContextRecordingGate {
            seen: seen.clone(),
            outcome: lingxi_core::host::permission_gate::PermissionOutcome::Allow {
                updated_input: None,
                permission_updates: Vec::new(),
                decision_classification: None,
            },
        });
        RegistryToolInvoker::new(registry_with_interactive_echo())
            .with_gate(gate)
            .invoke("TestEcho", json!({}), ctx_with_tool_use_id("toolu_ui"))
            .await
            .expect("allow dispatches");

        let ctx = seen
            .lock()
            .unwrap()
            .clone()
            .expect("context gate consulted");
        assert!(ctx.requires_user_interaction);
    }

    #[tokio::test]
    async fn dispatch_replays_frozen_command_denies_as_a_layer() {
        // The consumer `frozen_command_denies` never had: the fork-time snapshot
        // must reach the gate as a `disallowed_tools` LAYER, which the fold
        // applies on top of the base policy. `policy_gate_test.rs`'s
        // `a_disallowed_tools_layer_adds_a_command_source_deny_rule` proves such
        // a layer denies even under BypassPermissions — i.e. the frozen deny wins.
        let seen = Arc::new(StdMutex::new(None));
        let gate = Arc::new(ContextRecordingGate {
            seen: seen.clone(),
            outcome: lingxi_core::host::permission_gate::PermissionOutcome::Allow {
                updated_input: None,
                permission_updates: Vec::new(),
                decision_classification: None,
            },
        });
        let invoker = RegistryToolInvoker::new(registry_with_echo()).with_gate(gate);
        let mut ctx = ctx_with_tool_use_id("toolu_fork");
        ctx.frozen_command_denies = vec!["Bash(rm:*)".to_string()];
        invoker
            .invoke("TestEcho", json!({ "a": 1 }), ctx)
            .await
            .expect("allow dispatches");
        let seen = seen
            .lock()
            .unwrap()
            .clone()
            .expect("context gate consulted");
        assert_eq!(
            seen.permission_layers,
            vec![json!({
                "kind": "disallowed_tools",
                "disallowedTools": ["Bash(rm:*)"],
            })],
            "the frozen snapshot reaches the gate as a disallowed_tools layer"
        );
    }

    #[test]
    fn frozen_command_deny_layers_drops_entries_it_cannot_honour() {
        // `from_rule_string` is INFALLIBLE — a mangled rule degrades to a bare
        // tool name instead of erroring — so malformed input is caught by what
        // it parsed INTO: a real tool name can never contain a paren.
        //
        // The frozen set is the `command` SOURCE bucket, which legitimately
        // holds rules for ANY tool, so `Read(...)` is kept — it used to be
        // dropped by a `tool_name == "Bash"` filter that read the bucket as if
        // `command` named the Bash tool.
        let layers = frozen_command_deny_layers(&[
            "Bash(rm:*)".to_string(),
            "   ".to_string(),               // empty after trim → dropped
            "Bash(unbalanced".to_string(),   // unbalanced → tool name holds a paren → dropped
            "Read(/etc/passwd)".to_string(), // a non-Bash command-source rule → KEPT
            "Bash(*)".to_string(),           // tool-wide; canonicalizes to "Bash"
            "Bash(git push:*)".to_string(),
        ]);
        assert_eq!(
            layers,
            vec![json!({
                "kind": "disallowed_tools",
                "disallowedTools": [
                    "Bash(rm:*)",
                    "Read(/etc/passwd)",
                    "Bash",
                    "Bash(git push:*)",
                ],
            })],
            "every well-formed rule survives regardless of tool; only malformed ones drop"
        );
    }

    #[test]
    fn frozen_command_deny_layers_emits_the_canonical_spelling() {
        // The string this emits is re-parsed by `extend_command_rules`, so it
        // must be the spelling the parser round-trips — NOT the raw record text.
        //
        // Two ways the raw text betrays the deny, both fail-OPEN:
        //   * padding — `from_rule_string(" Bash(rm:*) ")` sees a trailing space
        //     after the `)`, so it degrades to a BARE rule for a tool literally
        //     named `" Bash(rm:*) "`, which matches nothing;
        //   * `Bash(*)` / `Bash()` — the parser collapses both to the tool-wide
        //     form, so a byte-for-byte round-trip test would discard the single
        //     strictest rule the record can carry.
        let layers = frozen_command_deny_layers(&[
            " Bash(rm:*) ".to_string(),
            "Bash(*)".to_string(),
            "\tBash(git push:*)\n".to_string(),
        ]);
        assert_eq!(
            layers,
            vec![json!({
                "kind": "disallowed_tools",
                "disallowedTools": ["Bash(rm:*)", "Bash", "Bash(git push:*)"],
            })],
            "every rule the parser understood survives, in canonical spelling"
        );
    }

    #[test]
    fn frozen_command_deny_layers_are_the_layer_type_the_fold_reads_back() {
        // Guard against the wire spelling drifting from `PermissionLayer`: the
        // producer and the fold must agree, and asserting a hand-written literal
        // on both sides cannot catch a rename. Parse + fold the emitted value
        // through the REAL consumer instead.
        let layers = frozen_command_deny_layers(&["Bash(rm:*)".to_string()]);
        let folded = permission::fold_permission_layers(
            &permission::parse_permission_layers(&layers),
            permission::LayerFoldInputs::default(),
        );
        assert_eq!(
            folded.deny_command_rules,
            vec!["Bash(rm:*)".to_string()],
            "the emitted layer must reach the fold's command-deny bucket"
        );
    }

    #[test]
    fn frozen_command_deny_layers_is_empty_when_nothing_survives() {
        // No layer at all — NOT an empty layer — so the fold is byte-identical
        // to a spawn that froze nothing.
        assert!(frozen_command_deny_layers(&[]).is_empty());
        // Only MALFORMED entries drop now. `Read(x)` would be kept (the frozen
        // set is the command SOURCE bucket, not a Bash-only one), so this uses
        // an unbalanced rule instead.
        assert!(frozen_command_deny_layers(&["Bash(oops".to_string()]).is_empty());
        assert!(frozen_command_deny_layers(&["  ".to_string()]).is_empty());
    }

    #[tokio::test]
    async fn dispatch_threads_mode_override_into_context_gate() {
        // A spawned subagent's effective permission mode (claude-code 2.1.207
        // Agent `mode` → the child's `toolPermissionContext.mode`) must reach the
        // gate's PermissionCheckContext so the dispatch authorizes under that mode.
        let seen = Arc::new(StdMutex::new(None));
        let gate = Arc::new(ContextRecordingGate {
            seen: seen.clone(),
            outcome: lingxi_core::host::permission_gate::PermissionOutcome::Allow {
                updated_input: None,
                permission_updates: Vec::new(),
                decision_classification: None,
            },
        });
        let invoker = RegistryToolInvoker::new(registry_with_echo()).with_gate(gate);
        let mut ctx = ctx_with_tool_use_id("toolu_plan");
        ctx.mode_override = Some("plan".to_string());
        invoker
            .invoke("TestEcho", json!({ "a": 1 }), ctx)
            .await
            .expect("allow dispatches");
        let seen = seen
            .lock()
            .unwrap()
            .clone()
            .expect("context gate consulted");
        assert_eq!(
            seen.mode_override.as_deref(),
            Some("plan"),
            "the child's spawn mode reaches PermissionCheckContext.mode_override"
        );
    }

    #[tokio::test]
    async fn dispatch_leaves_mode_override_none_by_default() {
        // A dispatch with no spawn-mode override leaves PermissionCheckContext
        // untagged, so the gate uses its live/boot mode (byte-identical to before).
        let seen = Arc::new(StdMutex::new(None));
        let gate = Arc::new(ContextRecordingGate {
            seen: seen.clone(),
            outcome: lingxi_core::host::permission_gate::PermissionOutcome::Allow {
                updated_input: None,
                permission_updates: Vec::new(),
                decision_classification: None,
            },
        });
        let invoker = RegistryToolInvoker::new(registry_with_echo()).with_gate(gate);
        invoker
            .invoke("TestEcho", json!({}), no_ctx())
            .await
            .expect("allow dispatches");
        let seen = seen
            .lock()
            .unwrap()
            .clone()
            .expect("context gate consulted");
        assert_eq!(seen.mode_override, None);
    }

    #[tokio::test]
    async fn dispatch_applies_updated_input_rewrite_to_tool() {
        // An Allow{updated_input: Some(..)} from the gate (the host's
        // `updatedInput` rewrite) must substitute the input the tool actually
        // runs with — mirroring turn_loop's Ask arm. The echo tool returns
        // whatever input it received, so we can observe the substitution.
        let gate = Arc::new(ContextRecordingGate {
            seen: Arc::new(StdMutex::new(None)),
            outcome: lingxi_core::host::permission_gate::PermissionOutcome::Allow {
                updated_input: Some(lingxi_core::types::utf16_json::Utf16JsonProjection::plain(
                    json!({ "rewritten": true }),
                )),
                permission_updates: Vec::new(),
                decision_classification: None,
            },
        });
        let invoker = RegistryToolInvoker::new(registry_with_echo()).with_gate(gate);
        let out = invoker
            .invoke(
                "TestEcho",
                json!({ "original": true }),
                ctx_with_tool_use_id("toolu_x"),
            )
            .await
            .expect("allow dispatches");
        assert_eq!(
            out,
            json!({ "echo": { "rewritten": true } }),
            "the gate-rewritten updatedInput, not the original, is what the tool runs with"
        );
    }

    #[tokio::test]
    async fn dispatch_preserves_original_and_approved_utf16_input_carriers() {
        use lingxi_core::types::utf16_json::Utf16JsonProjection;
        let original =
            Utf16JsonProjection::parse(r#"{"text":"\ud800","\ud801":"original"}"#).unwrap();
        let updated =
            Utf16JsonProjection::parse(r#"{"text":"\udfff","\ud901":"approved"}"#).unwrap();
        let seen = Arc::new(StdMutex::new(None));
        let gate = Arc::new(ContextRecordingGate {
            seen: seen.clone(),
            outcome: lingxi_core::host::permission_gate::PermissionOutcome::Allow {
                updated_input: Some(updated.clone()),
                permission_updates: Vec::new(),
                decision_classification: None,
            },
        });
        let invoker = RegistryToolInvoker::new(registry_with_echo()).with_gate(gate);
        let mut invocation = ctx_with_tool_use_id("toolu_exact");
        invocation.input_projection = Some(original.clone());
        let result = invoker
            .invoke_detailed("TestEcho", original.value.clone(), invocation, None)
            .await
            .unwrap();
        assert_eq!(
            seen.lock().unwrap().as_ref().unwrap().input_projection,
            Some(original)
        );
        let retained = result
            .context_state
            .unwrap()
            .downcast_arc::<ToolUseContext>()
            .unwrap();
        assert_eq!(retained.projected_input(&updated.value).unwrap(), updated);
    }

    #[tokio::test]
    async fn dispatch_allow_auto_preserves_utf16_rewrite() {
        use lingxi_core::types::utf16_json::Utf16JsonProjection;
        let updated =
            Utf16JsonProjection::parse(r#"{"text":"\udfff","\ud901":"approved"}"#).unwrap();
        let gate = Arc::new(ContextRecordingGate {
            seen: Arc::new(StdMutex::new(None)),
            outcome: lingxi_core::host::permission_gate::PermissionOutcome::AllowAuto {
                updated_input: Some(updated.clone()),
            },
        });
        let invoker = RegistryToolInvoker::new(registry_with_echo()).with_gate(gate);
        let result = invoker
            .invoke_detailed("TestEcho", json!({}), no_ctx(), None)
            .await
            .unwrap();
        let retained = result
            .context_state
            .unwrap()
            .downcast_arc::<ToolUseContext>()
            .unwrap();
        assert_eq!(retained.projected_input(&updated.value).unwrap(), updated);
    }

    #[tokio::test]
    async fn dispatch_does_not_reuse_a_previous_calls_utf16_projection() {
        use lingxi_core::types::utf16_json::Utf16JsonProjection;
        let exact = Utf16JsonProjection::parse(r#"{"text":"\ud800"}"#).unwrap();
        let invoker = RegistryToolInvoker::new(registry_with_echo());
        let mut first = no_ctx();
        first.input_projection = Some(exact.clone());
        let result = invoker
            .invoke_detailed("TestEcho", exact.value.clone(), first, None)
            .await
            .unwrap();
        let mut next = no_ctx();
        next.tool_context_state = result.context_state;
        // Equal display strings do not establish physical-call ownership.
        let result = invoker
            .invoke_detailed("TestEcho", exact.value.clone(), next, None)
            .await
            .unwrap();
        let retained = result
            .context_state
            .unwrap()
            .downcast_arc::<ToolUseContext>()
            .unwrap();
        assert!(
            retained
                .projected_input(&exact.value)
                .unwrap()
                .strings
                .is_empty()
        );
    }

    #[tokio::test]
    async fn dispatch_rejects_an_input_projection_for_different_arguments() {
        use lingxi_core::types::utf16_json::Utf16JsonProjection;
        let seen = Arc::new(StdMutex::new(None));
        let gate = Arc::new(ContextRecordingGate {
            seen: seen.clone(),
            outcome: lingxi_core::host::permission_gate::PermissionOutcome::Allow {
                updated_input: None,
                permission_updates: Vec::new(),
                decision_classification: None,
            },
        });
        let invoker = RegistryToolInvoker::new(registry_with_echo()).with_gate(gate);
        let mut invocation = no_ctx();
        invocation.input_projection =
            Some(Utf16JsonProjection::parse(r#"{"text":"\ud800"}"#).unwrap());
        let result = invoker
            .invoke_detailed("TestEcho", json!({"text":"different"}), invocation, None)
            .await;
        assert!(matches!(result, Err(ToolInvokerError::InvalidInput(_))));
        assert!(seen.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn dispatch_keeps_original_input_when_updated_input_none() {
        // Behavior identical when updated_input is None: the original input runs.
        let gate = Arc::new(ContextRecordingGate {
            seen: Arc::new(StdMutex::new(None)),
            outcome: lingxi_core::host::permission_gate::PermissionOutcome::Allow {
                updated_input: None,
                permission_updates: Vec::new(),
                decision_classification: None,
            },
        });
        let invoker = RegistryToolInvoker::new(registry_with_echo()).with_gate(gate);
        let out = invoker
            .invoke("TestEcho", json!({ "original": true }), no_ctx())
            .await
            .expect("allow dispatches");
        assert_eq!(out, json!({ "echo": { "original": true } }));
    }

    #[tokio::test]
    async fn dispatch_allow_auto_is_one_shot_and_applies_updated_input() {
        let invoker = RegistryToolInvoker::new(registry_with_echo())
            .with_gate(Arc::new(AllowAutoNoTransitionGate));
        let out = invoker
            .invoke(
                "TestEcho",
                json!({ "original": true }),
                ctx_with_tool_use_id("toolu_auto"),
            )
            .await
            .expect("AllowAuto still permits the current tool call");
        assert_eq!(
            out,
            json!({ "echo": { "rewritten": "auto" } }),
            "subagent dispatch must honor updated_input while degrading AllowAuto to this call only"
        );
    }

    #[tokio::test]
    async fn dispatch_context_deny_blocks_and_surfaces_reason() {
        // A Deny from the context gate is surfaced as Internal, exactly like the
        // legacy 2-valued path.
        let gate = Arc::new(ContextRecordingGate {
            seen: Arc::new(StdMutex::new(None)),
            outcome: lingxi_core::host::permission_gate::PermissionOutcome::Deny {
                reason: "denied via context gate".into(),
            },
        });
        let invoker = RegistryToolInvoker::new(registry_with_echo()).with_gate(gate);
        match invoker
            .invoke("TestEcho", json!({ "a": 1 }), no_ctx())
            .await
        {
            Err(ToolInvokerError::Internal(reason)) => {
                assert!(reason.contains("denied via context gate"), "got {reason}");
            }
            other => panic!("expected Internal(deny), got {other:?}"),
        }
    }

    fn protected_mcp_ask() -> PermissionResult {
        PermissionResult::Ask {
            reason: PermissionDecisionReason::PermissionPromptTool {
                tool_name: "mcp__srv__write".into(),
            },
            prompt: permission::result::PermissionPrompt {
                title: "Permission required".into(),
                message: "MCP tool requires approval".into(),
                options: vec!["Allow once".into(), "Deny".into()],
            },
            pending_classifier_check: None,
            metadata: PermissionMetadata::default(),
        }
    }

    fn probe_registry(
        result: PermissionResult,
        is_mcp: bool,
        calls: Arc<AtomicUsize>,
    ) -> Arc<ToolRegistry> {
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(ToolPermissionProbe {
            result,
            is_mcp,
            calls,
        }));
        Arc::new(registry)
    }

    #[tokio::test]
    async fn tool_local_deny_blocks_subagent_even_without_outer_gate() {
        let calls = Arc::new(AtomicUsize::new(0));
        let invoker = RegistryToolInvoker::new(probe_registry(
            PermissionResult::Deny {
                reason: PermissionDecisionReason::Other {
                    reason: "local deny".into(),
                },
                explanation: Some("tool-local deny".into()),
                metadata: PermissionMetadata::default(),
            },
            true,
            calls.clone(),
        ));
        let error = invoker
            .invoke("ToolPermissionProbe", json!({}), no_ctx())
            .await
            .expect_err("tool-local deny must bind without a policy gate");
        assert!(
            matches!(error, ToolInvokerError::Internal(ref reason) if reason == "tool-local deny")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn protected_mcp_ask_uses_worker_attributed_transport_over_allow() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(StdMutex::new(None));
        let gate = Arc::new(WorkerRecordingGate { seen: seen.clone() });
        let invoker =
            RegistryToolInvoker::new(probe_registry(protected_mcp_ask(), true, calls.clone()))
                .with_gate(gate);
        let mut ctx = ctx_with_tool_use_id("toolu_mcp_ask");
        ctx.is_non_interactive_session = false;
        invoker
            .invoke("ToolPermissionProbe", json!({}), ctx)
            .await
            .expect("interactive worker transport approves the protected ask");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let worker = seen.lock().unwrap().clone().expect("gate consulted");
        assert_eq!(worker.expect("worker attribution").name, "researcher");
    }

    #[tokio::test]
    async fn protected_mcp_ask_fails_closed_for_headless_subagent() {
        let calls = Arc::new(AtomicUsize::new(0));
        let invoker =
            RegistryToolInvoker::new(probe_registry(protected_mcp_ask(), true, calls.clone()))
                .with_gate(Arc::new(WorkerRecordingGate {
                    seen: Arc::new(StdMutex::new(None)),
                }));
        let mut ctx = ctx_with_tool_use_id("toolu_mcp_headless");
        ctx.is_non_interactive_session = true;
        let error = invoker
            .invoke("ToolPermissionProbe", json!({}), ctx)
            .await
            .expect_err("headless protected ask must fail closed");
        assert!(matches!(error, ToolInvokerError::Internal(_)));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn unknown_tool_is_not_found_before_gate_runs() {
        // `find_by_name` precedes the gate → an unknown tool is NotFound and
        // the gate is never consulted (no spurious deny on a non-existent tool).
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let gate = Arc::new(FixedGate {
            decision: GateDecision::Deny {
                reason: "should not run".into(),
            },
            seen: seen.clone(),
        });
        let invoker = RegistryToolInvoker::new(registry_with_echo()).with_gate(gate);
        let result = invoker.invoke("NotARealTool", json!({}), no_ctx()).await;
        assert!(matches!(result, Err(ToolInvokerError::NotFound(_))));
        assert!(
            seen.lock().unwrap().is_empty(),
            "gate not consulted for unknown tool"
        );
    }
    struct OwnPlanRequester(String);
    #[async_trait]
    impl lingxi_core::host::teammate_plan::TeammatePlanRequester for OwnPlanRequester {
        fn writable_plan_path(&self) -> Option<&str> {
            Some(&self.0)
        }
        async fn submit(&self, _: Value) -> Result<Value, String> {
            unreachable!()
        }
    }
    struct PlanWriteProbe;
    #[async_trait]
    impl Tool for PlanWriteProbe {
        fn name(&self) -> &str {
            "Write"
        }
        fn input_schema(&self) -> &Value {
            &ECHO_INPUT_SCHEMA
        }
        fn is_enabled(&self, _: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024
        }
        fn is_concurrency_safe(&self, _: &Value) -> bool {
            false
        }
        fn is_read_only(&self, _: &Value) -> bool {
            false
        }
        async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
            allow_for_tests()
        }
        async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
            String::new()
        }
        async fn prompt(&self, _: &PromptOptions) -> String {
            String::new()
        }
        async fn call(
            &self,
            input: Value,
            _: ToolUseContext,
            _: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            std::fs::write(
                input["file_path"].as_str().unwrap(),
                input["content"].as_str().unwrap(),
            )
            .unwrap();
            Ok(ToolCallResult {
                model_content: Some("Plan file written".into()),
                ..ToolCallResult::from_data(json!({"written":true}))
            })
        }
    }
    fn plan_probe_registry() -> Arc<ToolRegistry> {
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(PlanWriteProbe));
        Arc::new(registry)
    }
    fn plan_probe_context(id: lingxi_core::types::AgentId) -> SubagentInvocationContext {
        let mut ctx = no_ctx();
        ctx.parent_agent_id = Some(id);
        ctx.mode_override = Some("plan".into());
        ctx.is_non_interactive_session = true;
        ctx
    }
    fn plan_policy_gate(behavior: Option<permission::PermissionBehavior>) -> Arc<dyn Gate> {
        let mut policy = permission::PermissionPolicy::new(permission::PermissionMode::Plan);
        if let Some(behavior) = behavior {
            let rule = permission::PermissionRule {
                value: permission::PermissionRuleValue::from_rule_string("Write"),
                source: permission::PermissionRuleSource::Command,
                behavior,
            };
            match behavior {
                permission::PermissionBehavior::Deny => {
                    policy.deny_rules.insert(rule.source, vec![rule]);
                }
                permission::PermissionBehavior::Ask => {
                    policy.ask_rules.insert(rule.source, vec![rule]);
                }
                permission::PermissionBehavior::Allow => unreachable!(),
            }
        }
        Arc::new(permission::PolicyPermissionGate::new(
            Arc::new(policy),
            Arc::new(FixedGate {
                decision: GateDecision::Deny {
                    reason: "no unattended approval".into(),
                },
                seen: Arc::new(StdMutex::new(vec![])),
            }),
        ))
    }
    #[tokio::test]
    async fn own_plan_allowance_preserves_deny_ask_frozen_rules_and_structured_result() {
        use permission::PermissionBehavior::{Ask, Deny};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap().join("plan.md");
        let sibling = path.with_file_name("sibling.md");
        let id = lingxi_core::types::AgentId::new();
        let requester: Arc<dyn lingxi_core::host::teammate_plan::TeammatePlanRequester> =
            Arc::new(OwnPlanRequester(path.to_string_lossy().into_owned()));
        lingxi_core::host::teammate_plan::register(id, &requester);
        let input = json!({"file_path":path,"content":"Review then implement"});
        for rule in [Some(Deny), Some(Ask)] {
            let invoker =
                RegistryToolInvoker::new(plan_probe_registry()).with_gate(plan_policy_gate(rule));
            assert!(
                invoker
                    .invoke("Write", input.clone(), plan_probe_context(id))
                    .await
                    .is_err()
            );
            assert!(!path.exists());
        }
        let invoker =
            RegistryToolInvoker::new(plan_probe_registry()).with_gate(plan_policy_gate(None));
        assert!(
            invoker
                .invoke(
                    "Write",
                    json!({"file_path":sibling,"content":"no"}),
                    plan_probe_context(id)
                )
                .await
                .is_err()
        );
        let mut frozen = plan_probe_context(id);
        frozen.frozen_command_denies.push("Write".into());
        assert!(
            invoker
                .invoke("Write", input.clone(), frozen)
                .await
                .is_err()
        );
        let result = invoker
            .invoke_detailed("Write", input.clone(), plan_probe_context(id), None)
            .await
            .unwrap();
        assert_eq!(result.data, json!({"written":true}));
        assert_eq!(result.model_content.as_deref(), Some("Plan file written"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "Review then implement"
        );
        assert!(!sibling.exists());
    }
    #[tokio::test]
    async fn own_plan_allowance_rejects_permission_input_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap().join("plan.md");
        let other = path.with_file_name("other.md");
        let id = lingxi_core::types::AgentId::new();
        let requester: Arc<dyn lingxi_core::host::teammate_plan::TeammatePlanRequester> =
            Arc::new(OwnPlanRequester(path.to_string_lossy().into_owned()));
        lingxi_core::host::teammate_plan::register(id, &requester);
        let gate = Arc::new(ContextRecordingGate {
            seen: Arc::new(StdMutex::new(None)),
            outcome: lingxi_core::host::permission_gate::PermissionOutcome::Allow {
                updated_input: Some(lingxi_core::types::utf16_json::Utf16JsonProjection::plain(
                    json!({"file_path":other,"content":"no"}),
                )),
                permission_updates: vec![],
                decision_classification: None,
            },
        });
        let invoker = RegistryToolInvoker::new(plan_probe_registry()).with_gate(gate);
        assert!(
            invoker
                .invoke(
                    "Write",
                    json!({"file_path":path,"content":"plan"}),
                    plan_probe_context(id)
                )
                .await
                .is_err()
        );
        assert!(!other.exists());
        assert!(!path.exists());
    }
}
