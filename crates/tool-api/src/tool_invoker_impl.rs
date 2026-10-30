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
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::sync::Arc;

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
            is_scheduled_turn: false,
            available_tool_names: None,
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
            session_origin: None,
            agent_spawn_provenance: Default::default(),
            native_query_depth: None,
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
            host_tool_cancellation: crate::context::HostToolCancellation::Ordinary,
            fork_parent_system_prompt: None,
            cwd: None,
            depth: 0,
            observer: None,
            observer_pairings: None,
            file_history: None,
        };
        tool.cleanup_computer_inputs(&ctx)
            .await
            .map_err(map_tool_error)
    }

    fn permission_mode(&self) -> Option<String> {
        RegistryToolInvoker::permission_mode(self)
    }

    fn map_result_text(
        &self,
        name: &str,
        result: &Utf16JsonProjection,
    ) -> Option<Utf16JsonProjection> {
        self.registry
            .find_by_name(name)
            .and_then(|tool| tool.map_result_text(result))
    }

    fn map_result_is_error(&self, name: &str, result: &Utf16JsonProjection) -> Option<bool> {
        self.registry
            .find_by_name(name)
            .and_then(|tool| tool.map_result_is_error(result))
    }

    async fn parse_output(
        &self,
        name: &str,
        output: &Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ToolInvokerError> {
        let tool = self
            .registry
            .find_by_name(name)
            .ok_or_else(|| ToolInvokerError::NotFound(name.to_owned()))?;
        tool.parse_output(output)
            .await
            .map_err(|error| match error {
                crate::tool_trait::ToolError::InvalidInput(detail) => {
                    ToolInvokerError::Validation(detail)
                }
                other => map_tool_error(other),
            })
    }

    fn tool_is_concurrency_safe(&self, name: &str, input: &Utf16JsonProjection) -> Option<bool> {
        self.registry
            .find_by_name(name)
            .map(|tool| tool.is_concurrency_safe(input))
    }

    async fn invoke(
        &self,
        name: &str,
        input: Utf16JsonProjection,
        ctx: SubagentInvocationContext,
    ) -> Result<Utf16JsonProjection, ToolInvokerError> {
        self.invoke_detailed(name, input, ctx)
            .await
            .map(|result| result.data)
    }

    async fn invoke_detailed(
        &self,
        name: &str,
        input: Utf16JsonProjection,
        ctx: SubagentInvocationContext,
    ) -> Result<lingxi_core::host::tool_invoker::ToolInvocationResult, ToolInvokerError> {
        self.invoke_registered_or_supplied(name, input, ctx, None)
            .await
    }

    async fn invoke_supplied_detailed(
        &self,
        name: &str,
        input: Utf16JsonProjection,
        ctx: SubagentInvocationContext,
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
        self.invoke_registered_or_supplied(name, input, ctx, Some(tool))
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
        mut input: Utf16JsonProjection,
        ctx: SubagentInvocationContext,
        supplied: Option<Arc<dyn crate::Tool>>,
    ) -> Result<lingxi_core::host::tool_invoker::ToolInvocationResult, ToolInvokerError> {
        let tool = supplied
            .or_else(|| self.registry.find_by_name(name))
            .ok_or_else(|| ToolInvokerError::NotFound(name.to_string()))?;
        self.registry
            .check_session_tool_access(tool.as_ref())
            .map_err(|error| ToolInvokerError::Internal(error.model_facing_message()))?;

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
        }

        if tool.native_input_validation() {
            match tool.parse_native_input(&input) {
                Some(Ok(parsed)) => {
                    input = parsed;
                }
                Some(Err(error)) => {
                    let mut message_units =
                        "InputValidationError: ".encode_utf16().collect::<Vec<_>>();
                    message_units.extend(
                        error
                            .display
                            .string_units("")
                            .expect("native schema error display is a root string"),
                    );
                    return Err(ToolInvokerError::Validation(
                        crate::tool_trait::source_text_projection_units(&message_units),
                    ));
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
                    ToolInvokerError::Internal(
                        format!("Nested tool context state is invalid: {error}").into(),
                    )
                })?
                .as_ref()
                .clone(),
            None => crate::context::ToolUseContext {
                is_scheduled_turn: false,
                available_tool_names: ctx.available_tool_names.clone(),
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
                native_query_depth: ctx.native_query_depth,
                nested_memory_triggers: Arc::default(),
                agent_name: None,
                team_name: None,
                origin_session_id: None,
                session_origin: None,
                instruction_context: None,
                tool_execution_policy: ctx.tool_execution_policy,
                trusted_effective_permission_mode: None,
                classifier_only_review: None,
                content_replacement_state: None,
                session: None,
                subagent_registry: None,
                cancel: Some(ctx.cancellation_token.clone()),
                host_tool_cancellation: crate::context::HostToolCancellation::Ordinary,
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
        tool_use_ctx.available_tool_names = ctx.available_tool_names.clone();
        tool_use_ctx.assistant_message = ctx.assistant_message.clone();
        tool_use_ctx.same_turn_tool_uses = ctx.same_turn_tool_uses.clone();
        tool_use_ctx.tool_use_id = ctx
            .tool_use_id
            .clone()
            .map(lingxi_core::types::ToolUseId::from);
        tool_use_ctx.assistant_message_id = ctx.assistant_message_id;
        tool_use_ctx.agent_id = ctx.parent_agent_id;
        tool_use_ctx.agent_spawn_provenance = ctx.agent_spawn_provenance.clone();
        tool_use_ctx.native_query_depth = ctx.native_query_depth;
        tool_use_ctx.agent_name = ctx.agent_name.clone();
        tool_use_ctx.team_name = ctx.team_name.clone();
        tool_use_ctx.origin_session_id = ctx.origin_session_id;
        tool_use_ctx.session_origin = ctx.session_origin.clone();
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
                    .unwrap_or_else(|| format!("Permission to use {name} has been denied.").into()),
            ));
        }
        // A protected ASK needs a transport to obtain a human decision.  No
        // gate or a headless owner must fail closed rather than silently run
        // the tool-local operation.
        if tool_ask_is_protected && (self.gate.is_none() || ctx.is_non_interactive_session) {
            return Err(ToolInvokerError::Internal(
                format!("Permission to use {name} has been denied.").into(),
            ));
        }

        // The trusted requester owns one file, not a directory-wide grant.
        // A per-call allow layer only removes the plan-mode mutation ask;
        // explicit deny/ask rules and all earlier safety checks still run.
        let own_plan_path =
            if ctx.mode_override.as_deref() == Some("plan") && matches!(name, "Write" | "Edit") {
                input.string_at("/file_path").and_then(|path| {
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
                // Keep fork-time rules separate from ordinary permission
                // layers. Native `fLn` merges this source array before the
                // live Command bucket and then applies exact first-occurrence
                // Set semantics; a post-base `disallowed_tools` layer would
                // reverse same-source negation order.
                frozen_command_denies: ctx.frozen_command_denies.clone(),
                permission_layers: own_plan_path
                    .is_some()
                    .then(|| permission::PermissionLayer::AllowedTools(vec![name.into()]).to_wire())
                    .into_iter()
                    .collect(),
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
                        input = u;
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
                        input = u;
                    }
                }
                lingxi_core::host::permission_gate::PermissionOutcome::Deny { reason }
                | lingxi_core::host::permission_gate::PermissionOutcome::TransportClosed {
                    reason,
                } => {
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
                .string_at("/file_path")
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

        self.registry
            .check_session_tool_access(tool.as_ref())
            .map_err(|error| ToolInvokerError::Internal(error.model_facing_message()))?;
        let result = tool
            .call(input, tool_use_ctx, progress_tx)
            .await
            .map_err(map_tool_error)?;

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
            context: Utf16JsonProjection::parse("[]").expect("empty JSON array is valid"),
            context_state: Some(context_state),
        })
    }
}

fn map_tool_error(error: crate::tool_trait::ToolError) -> ToolInvokerError {
    use crate::tool_trait::ToolError;
    use lingxi_core::host::tool_invoker::CleanupUnverifiedReason;

    match error {
        ToolError::InvalidInput(message) => ToolInvokerError::InvalidInput(message),
        ToolError::CleanupUnverified(message) => {
            ToolInvokerError::CleanupUnverified(CleanupUnverifiedReason::Message(message))
        }
        ToolError::CleanupUnverifiedOutput(output) => {
            ToolInvokerError::CleanupUnverified(output.into())
        }
        ToolError::CleanupUnverifiedBatch(reasons) => {
            ToolInvokerError::CleanupUnverified(CleanupUnverifiedReason::Multiple(reasons))
        }
        other => ToolInvokerError::Internal(other.model_facing_message()),
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
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;

    fn source_fixture(value: Value) -> Utf16JsonProjection {
        Utf16JsonProjection::from_serde_value(&value).expect("bounded JSON test fixture")
    }

    fn source_schema_fixture(value: Value) -> Utf16JsonProjection {
        crate::tool_trait::generated_builtin_schema(&value)
    }

    // Test fixtures only: make the explicit serde Value-to-Source boundary
    // visible at each literal JSON construction site in this test module.
    macro_rules! json {
        ($($tokens:tt)*) => {
            source_fixture(serde_json::json!($($tokens)*))
        };
    }

    fn builtin_input_repair_metadata() -> crate::tool_trait::ToolInputRepairMetadata {
        use crate::tool_trait::{ToolInputRepairSchema, ToolInputSchemaProvenance};
        crate::tool_trait::ToolInputRepairMetadata::Known {
            schema: ToolInputRepairSchema::Advertised,
            schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }

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

    static ECHO_INPUT_SCHEMA: Lazy<Utf16JsonProjection> = Lazy::new(|| {
        source_schema_fixture(serde_json::json!({ "type": "object", "additionalProperties": true }))
    });
    static ECHO_OUTPUT_SCHEMA: Lazy<Utf16JsonProjection> = Lazy::new(|| {
        source_schema_fixture(
            serde_json::json!({"type":"object","required":["echo"],"properties":{"echo":{"type":"object"}}}),
        )
    });

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
        fn input_schema(&self) -> &Utf16JsonProjection {
            &ECHO_INPUT_SCHEMA
        }
        fn input_repair_metadata(&self) -> crate::tool_trait::ToolInputRepairMetadata {
            builtin_input_repair_metadata()
        }
        fn output_schema(&self) -> Option<&Utf16JsonProjection> {
            Some(&ECHO_OUTPUT_SCHEMA)
        }
        fn map_result_text(&self, result: &Utf16JsonProjection) -> Option<Utf16JsonProjection> {
            let mut units = "echo:".encode_utf16().collect::<Vec<_>>();
            units.extend(result.string_units("/echo/message")?);
            Some(crate::tool_trait::source_text_projection_units(&units))
        }
        async fn parse_output(
            &self,
            output: &Utf16JsonProjection,
        ) -> Result<Utf16JsonProjection, ToolError> {
            let Some(echo) = output.at_pointer("/echo") else {
                return Err(ToolError::InvalidInput("missing echo object".into()));
            };
            if echo.object_entries().is_none() {
                return Err(ToolError::InvalidInput("echo must be an object".into()));
            }
            let echo = output
                .subprojection("/echo")
                .map_err(|error| ToolError::Internal(error.to_string()))?;
            Utf16JsonProjection::object(vec![("echo".into(), echo)])
                .map_err(|error| ToolError::Internal(error.to_string()))
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
        fn is_concurrency_safe(&self, input: &Utf16JsonProjection) -> bool {
            !input
                .get_property("force_serial")
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
        }
        fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
            true
        }
        async fn validate_input(
            &self,
            _input: &Utf16JsonProjection,
            _ctx: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _input: &Utf16JsonProjection,
            _ctx: &ToolUseContext,
        ) -> PermissionResult {
            allow_for_tests()
        }
        async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
            "echo".into()
        }
        async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
            "echo tool".into()
        }
        async fn call(
            &self,
            input: Utf16JsonProjection,
            _ctx: ToolUseContext,
            _progress_tx: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            Ok(ToolCallResult {
                is_error: input
                    .get_property("fail")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false),
                data: Utf16JsonProjection::object(vec![("echo".into(), input)])
                    .expect("test echo object is valid"),
                model_content: None,
                new_messages: vec![],
                context_modifier: None,
                mcp_meta: None,
                subagent_hooks_fired: false,
            })
        }
        fn interrupt_behavior(&self, _input: &Utf16JsonProjection) -> InterruptBehavior {
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
        fn input_schema(&self) -> &Utf16JsonProjection {
            &ECHO_INPUT_SCHEMA
        }
        fn input_repair_metadata(&self) -> crate::tool_trait::ToolInputRepairMetadata {
            builtin_input_repair_metadata()
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
        fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
            true
        }
        fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
            false
        }
        async fn validate_input(
            &self,
            _: &Utf16JsonProjection,
            _: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _: &Utf16JsonProjection,
            _: &ToolUseContext,
        ) -> PermissionResult {
            self.result.clone()
        }
        async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
            "permission probe".into()
        }
        async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
            "".into()
        }
        async fn call(
            &self,
            _: Utf16JsonProjection,
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
                subagent_hooks_fired: false,
            })
        }
    }

    static RECORDING_INPUT_SCHEMA: Lazy<Utf16JsonProjection> =
        Lazy::new(|| source_schema_fixture(serde_json::json!({ "type": "object" })));

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
        fn input_schema(&self) -> &Utf16JsonProjection {
            &RECORDING_INPUT_SCHEMA
        }
        fn input_repair_metadata(&self) -> crate::tool_trait::ToolInputRepairMetadata {
            builtin_input_repair_metadata()
        }
        fn is_enabled(&self, _: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024
        }
        fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
            true
        }
        fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
            true
        }
        async fn validate_input(
            &self,
            _: &Utf16JsonProjection,
            _: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _: &Utf16JsonProjection,
            _: &ToolUseContext,
        ) -> PermissionResult {
            allow_for_tests()
        }
        async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
            "rec".into()
        }
        async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
            "rec".into()
        }
        async fn call(
            &self,
            _: Utf16JsonProjection,
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
                subagent_hooks_fired: false,
            })
        }
        fn interrupt_behavior(&self, _input: &Utf16JsonProjection) -> InterruptBehavior {
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

    #[tokio::test]
    async fn registry_invoker_validates_mod_replacement_without_running_the_tool() {
        let invoker = RegistryToolInvoker::new(registry_with_echo());
        assert!(invoker
            .parse_output("TestEcho", &json!({"echo":{}}))
            .await
            .is_ok());
        assert!(invoker
            .parse_output("TestEcho", &json!({"echo":42}))
            .await
            .is_err());
        assert_eq!(
            invoker
                .map_result_text("TestEcho", &json!({"echo":{"message":"hello"}}))
                .and_then(|text| text.string_at("").map(str::to_owned)),
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
                .invoke_detailed("TestEcho", json!({"fail": failed}), no_ctx())
                .await
                .expect("a tool-reported failure is still a completed invocation");
            assert_eq!(result.is_error, failed);
            assert_eq!(result.data, json!({"echo": {"fail": failed}}));
        }
    }

    struct IncompleteProcessTool;

    #[async_trait]
    impl Tool for IncompleteProcessTool {
        fn name(&self) -> &str {
            "IncompleteProcess"
        }
        fn input_schema(&self) -> &Utf16JsonProjection {
            &ECHO_INPUT_SCHEMA
        }
        fn input_repair_metadata(&self) -> crate::tool_trait::ToolInputRepairMetadata {
            builtin_input_repair_metadata()
        }
        fn is_enabled(&self, _: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024
        }
        fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
            false
        }
        fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
            false
        }
        async fn check_permissions(
            &self,
            _: &Utf16JsonProjection,
            _: &ToolUseContext,
        ) -> PermissionResult {
            allow_for_tests()
        }
        async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
            String::new()
        }
        async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
            "".into()
        }
        async fn call(
            &self,
            _: Utf16JsonProjection,
            _: ToolUseContext,
            _: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            use crate::tool_trait::{
                ProcessTreeStatus, UnverifiedCleanupOutput, UnverifiedCleanupOutputParts,
            };
            Err(ToolError::CleanupUnverifiedOutput(Box::new(
                UnverifiedCleanupOutput::new(UnverifiedCleanupOutputParts {
                    session_id: Some("session-child".into()),
                    stdout: "partial stdout".into(),
                    stderr: "partial stderr".into(),
                    stdout_truncated: false,
                    stderr_truncated: true,
                    stdout_eof: false,
                    stderr_eof: true,
                    exit_code: None,
                    timed_out: true,
                    termination_requested: Some(true),
                    tree_status: ProcessTreeStatus::Unverified,
                    cleanup_detail: "tree exit unknown".into(),
                    original_error: Some("original read failure".into()),
                }),
            )))
        }
    }

    #[tokio::test]
    async fn registry_invoker_retains_incomplete_process_evidence() {
        use lingxi_core::host::tool_invoker::{CleanupUnverifiedReason, ProcessTreeStatus};

        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(IncompleteProcessTool));
        let invoker = RegistryToolInvoker::new(Arc::new(registry));
        let error = invoker
            .invoke_detailed("IncompleteProcess", json!({}), no_ctx())
            .await
            .expect_err("an incomplete process is not a successful tool result");
        let ToolInvokerError::CleanupUnverified(CleanupUnverifiedReason::Output(output)) = error
        else {
            panic!("the structured cleanup error must survive registry dispatch");
        };
        let parts = output.parts();
        assert_eq!(parts.session_id.as_deref(), Some("session-child"));
        assert_eq!(parts.stdout, "partial stdout");
        assert_eq!(parts.stderr, "partial stderr");
        assert!(!parts.stdout_eof && parts.stderr_eof && parts.stderr_truncated);
        assert_eq!(parts.exit_code, None);
        assert!(parts.timed_out);
        assert_eq!(parts.termination_requested, Some(true));
        assert_eq!(parts.tree_status, ProcessTreeStatus::Unverified);
        assert_eq!(parts.cleanup_detail, "tree exit unknown");
        assert_eq!(
            parts.original_error.as_deref(),
            Some("original read failure")
        );
    }

    /// Trusted outcome markers are configured by the host-side fixture; model
    /// input is only echoed into ordinary result data and model-facing text.
    struct TurnEndProbe {
        tool_marker: bool,
        is_error: bool,
        mcp_meta: Option<Utf16JsonProjection>,
    }

    #[async_trait]
    impl Tool for TurnEndProbe {
        fn name(&self) -> &str {
            "TurnEndProbe"
        }
        fn input_schema(&self) -> &Utf16JsonProjection {
            &ECHO_INPUT_SCHEMA
        }
        fn input_repair_metadata(&self) -> crate::tool_trait::ToolInputRepairMetadata {
            builtin_input_repair_metadata()
        }
        fn is_enabled(&self, _: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024
        }
        fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
            true
        }
        fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
            true
        }
        fn result_ends_turn(&self, result: &ToolCallResult) -> bool {
            assert_eq!(result.is_error, self.is_error);
            assert_eq!(result.mcp_meta, self.mcp_meta);
            assert_eq!(
                result.model_content,
                Some(crate::tool_trait::source_text_projection(
                    &result
                        .data
                        .to_json_string()
                        .expect("test result serializes")
                ))
            );
            self.tool_marker
        }
        async fn validate_input(
            &self,
            _: &Utf16JsonProjection,
            _: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _: &Utf16JsonProjection,
            _: &ToolUseContext,
        ) -> PermissionResult {
            allow_for_tests()
        }
        async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
            "trusted result marker probe".into()
        }
        async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
            "".into()
        }
        async fn call(
            &self,
            input: Utf16JsonProjection,
            _: ToolUseContext,
            _: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            Ok(ToolCallResult {
                model_content: Some(crate::tool_trait::source_text_projection(
                    &input.to_json_string().expect("test input serializes"),
                )),
                data: input,
                new_messages: vec![],
                context_modifier: None,
                is_error: self.is_error,
                mcp_meta: self.mcp_meta.clone(),
                subagent_hooks_fired: false,
            })
        }
    }

    async fn invoke_turn_end_probe(
        tool_marker: bool,
        is_error: bool,
        mcp_meta: Option<Utf16JsonProjection>,
        input: Utf16JsonProjection,
    ) -> lingxi_core::host::tool_invoker::ToolInvocationResult {
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(TurnEndProbe {
            tool_marker,
            is_error,
            mcp_meta,
        }));
        let invoker = RegistryToolInvoker::new(Arc::new(registry));
        let result = invoker
            .invoke_detailed("TurnEndProbe", input.clone(), no_ctx())
            .await
            .expect("probe invocation completes");
        assert_eq!(result.data, input);
        assert_eq!(
            result.model_content,
            Some(crate::tool_trait::source_text_projection(
                &input.to_json_string().expect("test input serializes")
            ))
        );
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
            json!(null),
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
                "invalid MCP marker accepted: {}",
                mcp_meta.to_json_string().expect("test metadata serializes")
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
                "untrusted echoed input ended turn: {input:?}"
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
    async fn per_call_cancellation_reaches_tool_contexts_without_leaking_a_prior_scope() {
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

        for with_prior_snapshot in [false, true] {
            *captured.lock().unwrap() = None;
            let cancellation_token = tokio_util::sync::CancellationToken::new();
            let mut context = no_ctx();
            context.cancellation_token = cancellation_token.clone();
            let stale_token = if with_prior_snapshot {
                let stale_token = tokio_util::sync::CancellationToken::new();
                let mut stale_context = ToolUseContext::model_seed("prior-call".into());
                stale_context.cancel = Some(stale_token.clone());
                context.tool_context_state =
                    Some(ToolInvocationContextState::new(Arc::new(stale_context)));
                Some(stale_token)
            } else {
                None
            };

            invoker
                .invoke("RecordingTool", json!({}), context)
                .await
                .expect("tool call succeeds");

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
                    Option<Utf16JsonProjection>,
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
        fn input_schema(&self) -> &Utf16JsonProjection {
            &RECORDING_INPUT_SCHEMA
        }
        fn input_repair_metadata(&self) -> crate::tool_trait::ToolInputRepairMetadata {
            builtin_input_repair_metadata()
        }
        fn is_enabled(&self, _: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024
        }
        fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
            true
        }
        fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
            true
        }
        async fn validate_input(
            &self,
            _: &Utf16JsonProjection,
            _: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _: &Utf16JsonProjection,
            _: &ToolUseContext,
        ) -> PermissionResult {
            allow_for_tests()
        }
        async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
            "rec".into()
        }
        async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
            "rec".into()
        }
        async fn call(
            &self,
            _: Utf16JsonProjection,
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
                subagent_hooks_fired: false,
            })
        }
        fn interrupt_behavior(&self, _input: &Utf16JsonProjection) -> InterruptBehavior {
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
            per_turn_effort: None,
            id: lingxi_core::types::MessageId::new(),
            content: vec![current_tool_use],
            stop_reason: Some("tool_use".into()),

            resumable: None,
        };
        let prior_sibling_tool_use = lingxi_core::types::ContentBlock::ToolUse {
            id: lingxi_core::types::ToolUseId::new(),
            name: "Read".into(),
            input: json!({"file_path":"prior.txt"}),
            provider_id: Some("provider-prior".into()),
        };
        let mut stale_context = crate::context::ToolUseContext::model_seed("stale".into());
        stale_context.assistant_message =
            Some(lingxi_core::types::ConversationMessage::Assistant {
                per_turn_effort: None,
                id: lingxi_core::types::MessageId::new(),
                content: vec![lingxi_core::types::ContentBlock::Text {
                    text: "stale assistant row".into(),
                    citations: None,
                }],
                stop_reason: None,

                resumable: None,
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
            session_origin: None,
            available_tool_names: None,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            permission_pause_observer: None,
            parent_agent_id: None,
            native_query_depth: None,
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
            captured_system_prompt
                .as_ref()
                .and_then(|value| value.string_at("")),
            Some("exact parent system")
        );
        assert_eq!(captured_instructions.as_ref(), Some(&instruction_context));
        drop(captured_guard);

        let next_policy = lingxi_core::types::ConversationMessage::user_meta(
            lingxi_core::types::MessageId::new(),
            "next query history".into(),
        );
        let next_assistant_row = lingxi_core::types::ConversationMessage::Assistant {
            per_turn_effort: None,
            id: lingxi_core::types::MessageId::new(),
            content: vec![lingxi_core::types::ContentBlock::ToolUse {
                id: lingxi_core::types::ToolUseId::new(),
                name: "Agent".into(),
                input: json!({"description":"next current row"}),
                provider_id: None,
            }],
            stop_reason: Some("tool_use".into()),

            resumable: None,
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
    async fn dispatch_preserves_trusted_execution_policy() {
        let captured = Arc::new(StdMutex::new(None));
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(NameRecordingTool {
            captured: captured.clone(),
        }));
        let invoker = RegistryToolInvoker::new(Arc::new(registry));
        invoker
            .invoke(
                "NameRecordingTool",
                json!({}),
                SubagentInvocationContext {
                    session_origin: None,
                    available_tool_names: None,
                    cancellation_token: tokio_util::sync::CancellationToken::new(),
                    permission_pause_observer: None,
                    parent_agent_id: None,
                    native_query_depth: None,
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
            )
            .await
            .expect("dispatch succeeds");
        let captured = captured.lock().unwrap();
        let captured = captured.as_ref().expect("tool call ran");
        assert_eq!(
            captured.4,
            lingxi_core::host::tool_invoker::ToolExecutionPolicy::FusionPanel,
            "dispatch must not drop the trusted Fusion policy"
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
        fn input_schema(&self) -> &Utf16JsonProjection {
            &RECORDING_INPUT_SCHEMA
        }
        fn input_repair_metadata(&self) -> crate::tool_trait::ToolInputRepairMetadata {
            builtin_input_repair_metadata()
        }
        fn is_enabled(&self, _: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024
        }
        fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
            true
        }
        fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
            true
        }
        async fn validate_input(
            &self,
            _: &Utf16JsonProjection,
            _: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _: &Utf16JsonProjection,
            _: &ToolUseContext,
        ) -> PermissionResult {
            allow_for_tests()
        }
        async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
            "rec".into()
        }
        async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
            "rec".into()
        }
        async fn call(
            &self,
            _: Utf16JsonProjection,
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
                subagent_hooks_fired: false,
            })
        }
        fn interrupt_behavior(&self, _input: &Utf16JsonProjection) -> InterruptBehavior {
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
        async fn check(&self, name: &str, _input: &Utf16JsonProjection) -> GateDecision {
            self.seen.lock().unwrap().push(name.to_string());
            self.decision.clone()
        }
    }

    struct RevokingGate(crate::SessionToolGrantLease);
    #[async_trait]
    impl Gate for RevokingGate {
        async fn check(&self, _name: &str, _input: &Utf16JsonProjection) -> GateDecision {
            // Simulate an approval completing after its app was revoked.
            self.0.revoke();
            GateDecision::Allow
        }
    }

    #[tokio::test]
    async fn app_tool_grant_revoked_during_approval_never_enters_body() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(ToolPermissionProbe {
            result: allow_for_tests(),
            is_mcp: false,
            calls: calls.clone(),
        }));
        let lease = crate::SessionToolGrantLease::new(crate::SessionToolPolicy {
            builtin_tools: std::collections::HashSet::from(["ToolPermissionProbe".into()]),
            mcp_tools: Default::default(),
        });
        registry.set_session_tool_policy(lease.clone());
        let invoker =
            RegistryToolInvoker::new(Arc::new(registry)).with_gate(Arc::new(RevokingGate(lease)));
        let result = invoker
            .invoke("ToolPermissionProbe", json!({}), no_ctx())
            .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    fn no_ctx() -> SubagentInvocationContext {
        SubagentInvocationContext {
            session_origin: None,
            available_tool_names: None,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            permission_pause_observer: None,
            parent_agent_id: None,
            native_query_depth: None,
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
        fn input_schema(&self) -> &Utf16JsonProjection {
            &RECORDING_INPUT_SCHEMA
        }
        fn input_repair_metadata(&self) -> crate::tool_trait::ToolInputRepairMetadata {
            builtin_input_repair_metadata()
        }
        fn is_enabled(&self, _: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024
        }
        fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
            true
        }
        fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
            true
        }
        async fn validate_input(
            &self,
            _: &Utf16JsonProjection,
            _: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _: &Utf16JsonProjection,
            _: &ToolUseContext,
        ) -> PermissionResult {
            allow_for_tests()
        }
        async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
            "record session mode".into()
        }
        async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
            "record session mode".into()
        }
        async fn call(
            &self,
            _: Utf16JsonProjection,
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
                subagent_hooks_fired: false,
            })
        }
        fn interrupt_behavior(&self, _input: &Utf16JsonProjection) -> InterruptBehavior {
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
                    reason
                        .string_at("")
                        .unwrap()
                        .contains("denied by permission rule Bash"),
                    "got {reason:?}"
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
        async fn check(&self, _name: &str, _input: &Utf16JsonProjection) -> GateDecision {
            // `check_with_worker`'s default would land here; record None so a
            // missing override is visible. The real override is below.
            *self.seen.lock().unwrap() = Some(None);
            GateDecision::Allow
        }
        async fn check_with_worker(
            &self,
            _name: &str,
            _input: &Utf16JsonProjection,
            worker: Option<lingxi_core::host::permission_gate::PromptWorker>,
        ) -> GateDecision {
            *self.seen.lock().unwrap() = Some(worker);
            GateDecision::Allow
        }
    }

    fn named_ctx(can_show: bool) -> SubagentInvocationContext {
        SubagentInvocationContext {
            session_origin: None,
            available_tool_names: None,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            permission_pause_observer: None,
            parent_agent_id: None,
            native_query_depth: None,
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
        async fn check(&self, _name: &str, _input: &Utf16JsonProjection) -> GateDecision {
            // Must not be reached — the override below intercepts the dispatch.
            GateDecision::Allow
        }
        async fn check_with_context(
            &self,
            _name: &str,
            _input: &Utf16JsonProjection,
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
        async fn check(&self, _name: &str, _input: &Utf16JsonProjection) -> GateDecision {
            panic!("abort-aware dispatch must not fall back to the legacy gate method")
        }

        async fn check_with_context_or_abort(
            &self,
            _name: &str,
            _input: &Utf16JsonProjection,
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
        async fn check(&self, _name: &str, _input: &Utf16JsonProjection) -> GateDecision {
            panic!("AllowAuto test must use the rich context path")
        }

        async fn check_with_context(
            &self,
            _name: &str,
            _input: &Utf16JsonProjection,
            _ctx: &lingxi_core::host::permission_gate::PermissionCheckContext,
        ) -> lingxi_core::host::permission_gate::PermissionOutcome {
            lingxi_core::host::permission_gate::PermissionOutcome::AllowAuto {
                updated_input: Some(json!({ "rewritten": "auto" })),
            }
        }

        async fn set_permission_mode(&self, _mode: &str) -> Result<(), String> {
            panic!("subagent tool dispatch must not attempt a session auto-mode transition")
        }
    }

    fn ctx_with_tool_use_id(id: &str) -> SubagentInvocationContext {
        SubagentInvocationContext {
            session_origin: None,
            available_tool_names: None,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            permission_pause_observer: None,
            parent_agent_id: None,
            native_query_depth: None,
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
    async fn dispatch_forwards_frozen_command_denies_as_a_separate_source_array() {
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
        ctx.frozen_command_denies = vec!["Bash(rm:*)".into()];
        invoker
            .invoke("TestEcho", json!({ "a": 1 }), ctx)
            .await
            .expect("allow dispatches");
        let seen = seen
            .lock()
            .unwrap()
            .clone()
            .expect("context gate consulted");
        assert_eq!(seen.frozen_command_denies, vec!["Bash(rm:*)".into()]);
        assert!(
            seen.permission_layers.is_empty(),
            "frozen command rules must not be replayed as post-base layers"
        );
    }

    #[tokio::test]
    async fn frozen_lone_surrogate_deny_survives_sidecar_round_trip_and_dispatch_context() {
        let sidecar_source = r#"{"skillName":"review","attributionName":"review","frozenCommandDenies":["Bash(echo \ud800)","Bash(echo �)","Bash(echo \ud800)"]}"#;
        let written: session::forked_skill::ForkedSkillScoping =
            serde_json::from_str(sidecar_source).expect("Native-shaped sidecar strings parse");
        assert!(written.is_valid());
        let serialized =
            serde_json::to_string(&written).expect("sidecar writes source-preserving JSON");
        assert!(
            serialized.contains(
                r#""frozenCommandDenies":["Bash(echo \ud800)","Bash(echo �)","Bash(echo \ud800)"]"#
            ),
            "{serialized}"
        );
        let restored: session::forked_skill::ForkedSkillScoping =
            serde_json::from_str(&serialized).expect("source-preserving sidecar cold-read");
        let frozen = restored
            .frozen_command_denies
            .expect("frozen denies persist");

        let mut ctx = ctx_with_tool_use_id("toolu_fork");
        ctx.frozen_command_denies = frozen.clone();
        let seen = Arc::new(StdMutex::new(None));
        let gate = Arc::new(ContextRecordingGate {
            seen: seen.clone(),
            outcome: lingxi_core::host::permission_gate::PermissionOutcome::Allow {
                updated_input: None,
                permission_updates: Vec::new(),
                decision_classification: None,
            },
        });
        RegistryToolInvoker::new(registry_with_echo())
            .with_gate(gate)
            .invoke("TestEcho", json!({ "a": 1 }), ctx)
            .await
            .expect("allow dispatches");
        let seen = seen
            .lock()
            .unwrap()
            .clone()
            .expect("context gate consulted");
        assert_eq!(seen.frozen_command_denies, frozen);
        assert_eq!(seen.frozen_command_denies.len(), 3);
        assert_eq!(
            seen.frozen_command_denies[0].string_units(""),
            Some(
                "Bash(echo "
                    .encode_utf16()
                    .chain([0xd800])
                    .chain(")".encode_utf16())
                    .collect()
            ),
            "the context keeps the exact isolated source unit"
        );
        assert_eq!(
            seen.frozen_command_denies[1].string_units(""),
            Some("Bash(echo �)".encode_utf16().collect()),
            "U+FFFD remains a distinct exact rule"
        );
        assert_eq!(
            seen.frozen_command_denies[2].string_units(""),
            seen.frozen_command_denies[0].string_units(""),
            "the source array's duplicate and first-seen order are retained at dispatch"
        );
        assert!(seen.permission_layers.is_empty());
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
                updated_input: Some(json!({ "rewritten": true })),
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
        let invocation = ctx_with_tool_use_id("toolu_exact");
        let result = invoker
            .invoke_detailed("TestEcho", original.clone(), invocation)
            .await
            .unwrap();
        assert!(seen.lock().unwrap().is_some());
        assert_eq!(result.data.subprojection("/echo").unwrap(), updated);
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
            .invoke_detailed("TestEcho", json!({}), no_ctx())
            .await
            .unwrap();
        assert_eq!(result.data.subprojection("/echo").unwrap(), updated);
    }

    #[tokio::test]
    async fn dispatch_does_not_reuse_a_previous_calls_utf16_projection() {
        let exact = Utf16JsonProjection::parse(r#"{"text":"\ud800"}"#).unwrap();
        let current = Utf16JsonProjection::parse(r#"{"text":"\ufffd"}"#).unwrap();
        let invoker = RegistryToolInvoker::new(registry_with_echo());
        let first = invoker
            .invoke_detailed("TestEcho", exact, no_ctx())
            .await
            .unwrap();
        let mut next = no_ctx();
        next.tool_context_state = first.context_state;
        let result = invoker
            .invoke_detailed("TestEcho", current.clone(), next)
            .await
            .unwrap();
        assert_eq!(result.data.subprojection("/echo").unwrap(), current);
    }

    #[tokio::test]
    async fn dispatch_keeps_current_argument_identity_with_reused_context() {
        let exact = Utf16JsonProjection::parse(r#"{"text":"\ud800","old":true}"#).unwrap();
        let current = Utf16JsonProjection::parse(r#"{"text":"different","new":true}"#).unwrap();
        let invoker = RegistryToolInvoker::new(registry_with_echo());
        let first = invoker
            .invoke_detailed("TestEcho", exact, no_ctx())
            .await
            .unwrap();
        let mut next = no_ctx();
        next.tool_context_state = first.context_state;
        let result = invoker
            .invoke_detailed("TestEcho", current.clone(), next)
            .await
            .unwrap();
        assert_eq!(result.data.subprojection("/echo").unwrap(), current);
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
                assert!(
                    reason
                        .string_at("")
                        .unwrap()
                        .contains("denied via context gate"),
                    "got {reason:?}"
                );
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
            matches!(error, ToolInvokerError::Internal(ref reason) if reason.string_at("") == Some("tool-local deny"))
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
        async fn submit(
            &self,
            _: Utf16JsonProjection,
        ) -> Result<Utf16JsonProjection, Utf16JsonProjection> {
            unreachable!()
        }
    }
    struct PlanWriteProbe;
    #[async_trait]
    impl Tool for PlanWriteProbe {
        fn name(&self) -> &str {
            "Write"
        }
        fn input_schema(&self) -> &Utf16JsonProjection {
            &ECHO_INPUT_SCHEMA
        }
        fn input_repair_metadata(&self) -> crate::tool_trait::ToolInputRepairMetadata {
            builtin_input_repair_metadata()
        }
        fn is_enabled(&self, _: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024
        }
        fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
            false
        }
        fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
            false
        }
        async fn check_permissions(
            &self,
            _: &Utf16JsonProjection,
            _: &ToolUseContext,
        ) -> PermissionResult {
            allow_for_tests()
        }
        async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
            String::new()
        }
        async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
            "".into()
        }
        async fn call(
            &self,
            input: Utf16JsonProjection,
            _: ToolUseContext,
            _: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            std::fs::write(
                input
                    .string_at("/file_path")
                    .expect("test path is a string"),
                input
                    .string_at("/content")
                    .expect("test content is a string"),
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
            assert!(invoker
                .invoke("Write", input.clone(), plan_probe_context(id))
                .await
                .is_err());
            assert!(!path.exists());
        }
        let invoker =
            RegistryToolInvoker::new(plan_probe_registry()).with_gate(plan_policy_gate(None));
        assert!(invoker
            .invoke(
                "Write",
                json!({"file_path":sibling,"content":"no"}),
                plan_probe_context(id)
            )
            .await
            .is_err());
        let mut frozen = plan_probe_context(id);
        frozen.frozen_command_denies.push("Write".into());
        assert!(invoker
            .invoke("Write", input.clone(), frozen)
            .await
            .is_err());
        let result = invoker
            .invoke_detailed("Write", input.clone(), plan_probe_context(id))
            .await
            .unwrap();
        assert_eq!(result.data, json!({"written":true}));
        assert_eq!(
            result
                .model_content
                .as_ref()
                .and_then(|text| text.string_at("")),
            Some("Plan file written")
        );
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
                updated_input: Some(json!({"file_path":other,"content":"no"})),
                permission_updates: vec![],
                decision_classification: None,
            },
        });
        let invoker = RegistryToolInvoker::new(plan_probe_registry()).with_gate(gate);
        assert!(invoker
            .invoke(
                "Write",
                json!({"file_path":path,"content":"plan"}),
                plan_probe_context(id)
            )
            .await
            .is_err());
        assert!(!other.exists());
        assert!(!path.exists());
    }
}
