use super::batch_hooks::run_post_tool_batch_hooks;
use super::tool_results::{
    apply_tool_result_persistence_with_process_output, bash_image_tool_result_blocks,
    image_tool_result_blocks, process_output_file_from_data, tool_result_to_model_text,
    tool_search_reference_blocks,
};
use super::{
    apply_terminal_sequence, AGENT_TOOL_NAME, CANCEL_MESSAGE, ENTER_WORKTREE_TOOL_NAME,
    LEGACY_AGENT_TOOL_NAME, PERMISSION_DENIED_RETRY_MESSAGE,
};
use crate::conversation::ConversationOrchestrator;
use crate::error::OrchestratorError;
use crate::test_support::{PermissionDecision, PermissionDecisionSource, PermissionResolution};
use hooks::events::HookEvent;
use hooks::registry::HookContext;
use hooks::response::HookDecision;
use protocol::{ContentBlock, ConversationMessage, MessageId, ToolUseId};
use std::path::{Component, Path, PathBuf};
use telemetry::tengu::orchestrator as orch_events;
use tool_api::context::{ToolUseContext, ToolUseOptions};
use tool_api::tool_trait::tool_result_turn_end;
use tool_api::ContextModifier;

pub(super) async fn forward_tool_progress(
    output: &dyn platform_api::OutputStream,
    parent_tool_use_id: &str,
    progress: tool_api::progress::ToolProgress,
) {
    if let Some(text) = progress
        .data
        .get("subagent_activity")
        .and_then(serde_json::Value::as_str)
    {
        output.emit_subagent_activity(text).await;
    } else if let Some(message) = progress.data.get("forward_subagent_message") {
        output
            .emit_forwarded_subagent_message(message, parent_tool_use_id)
            .await;
    }
}

/// Collapse `.` and `..` segments without touching the filesystem, mirroring
/// Node's `path.normalize`/`resolve` (used by `expandPath`). A `..` pops the
/// previous normal component; a leading `..` with nothing to pop is kept.
pub(crate) fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                // Pop the last NORMAL component; otherwise keep the `..`
                // (e.g. above the root prefix or a leading relative `..`).
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub(super) fn canonical_or_normalize(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| normalize_lexically(path))
}

pub(super) fn resolve_tool_file_path(
    tool_input: &serde_json::Value,
    cwd: &Path,
) -> Option<PathBuf> {
    let raw = tool_input
        .get("file_path")
        .or_else(|| tool_input.get("path"))
        .and_then(serde_json::Value::as_str)?;
    let path = Path::new(raw);
    Some(if path.is_absolute() {
        normalize_lexically(path)
    } else {
        normalize_lexically(&cwd.join(path))
    })
}

pub(super) fn memdir_index_notice_for_tool(
    orch: &ConversationOrchestrator,
    tool_name: &str,
    tool_input: &serde_json::Value,
    is_error: bool,
) -> Option<memory::MemoryIndexNotice> {
    if is_error || !matches!(tool_name, "Write" | "Edit" | "MultiEdit") {
        return None;
    }
    let memdir = orch
        .prompt_runtime
        .memory_prefetch
        .as_ref()?
        .user_memdir()?;
    let memory_index = canonical_or_normalize(&memdir.join("MEMORY.md"));
    let target = resolve_tool_file_path(tool_input, &orch.current_cwd())?;
    if canonical_or_normalize(&target) != memory_index {
        return None;
    }
    let content = std::fs::read_to_string(&memory_index).ok()?;
    memory::memory_index_cap_notice(&content)
}

/// Fold a tool result's `structuredPatch` (if any) into the session's
/// cumulative code-change counters (claude-code `Bhn(added, removed)`,
/// surfaced by `/usage`). Only file-edit tools (Edit/Write/MultiEdit) put a
/// `structuredPatch` array in their result data; every other tool's payload
/// lacks the key, so this is a no-op for them. Also a no-op when no
/// `cost_tracker` is wired (M6-06 default `None`).
pub(crate) async fn accumulate_code_change(
    emit_payload: &serde_json::Value,
    tracker: Option<&std::sync::Arc<cost::CostTracker>>,
) {
    let Some(sp) = emit_payload.get("structuredPatch") else {
        return;
    };
    let (added, removed) = crate::cost_lines::count_structured_patch_lines(sp);
    if added > 0 || removed > 0 {
        if let Some(tracker) = tracker {
            tracker.record_code_change(added, removed).await;
        }
    }
}

/// Dispatch each `tool_use` block through hooks -> permission -> registry ->
/// hooks. Returns a list of `ContentBlock::ToolResult` blocks for the
/// next user message.
///
/// Test-only thin wrapper over [`dispatch_tool_uses_tracked`] that returns just
/// the `ContentBlock` results (dropping the `prevent_continuation` and injected
/// `new_messages` tuple elements) for the in-file tests' convenience.
///
/// The production streaming path
/// ([`crate::streaming_executor::StreamingToolExecutor`]) calls
/// `dispatch_tool_uses_tracked` per tool, so it DOES replay tool-injected
/// `new_messages` (the Skill tool's expanded prompt) into history after the
/// `tool_result` — mirroring the batched [`execute_one_turn`] path (SKILLEXEC.3).
#[cfg(test)]
pub(crate) async fn dispatch_tool_uses(
    orch: &ConversationOrchestrator,
    tool_uses: &[(ToolUseId, String, serde_json::Value, Option<String>)],
) -> Result<Vec<ContentBlock>, OrchestratorError> {
    Ok(dispatch_tool_uses_tracked(orch, tool_uses, None).await?.0)
}

/// claude-code `ZX_(rule.source, behavior)`: the OTEL decision-source label a
/// matched permission RULE contributes.
///
/// ```text
/// session                     → allow ? "user_temporary" : "user_reject"
/// localSettings|userSettings  → allow ? "user_permanent" : "user_reject"
/// default                     → "config"
/// ```
///
/// The three user-OWNED `SettingSource`s are the only ones that read as a user
/// decision; `projectSettings`/`policySettings`/`flagSettings`/`cliArg`/… (and a
/// decision with no rule at all — mode, classifier, safety check) stay "config".
/// claude-code's `toolDenialKind` classifier — byte-locked to the 2.1.220
/// mapping at binary offset 235392096:
///
/// ```js
/// if(e.behavior==="ask") return "user-rejected";
/// let t=e.decisionReason;
/// if(t.type==="classifier" && t.classifier==="auto-mode"){
///   if(t.reason===TRt)              return "automode-unavailable";
///   if(t.reason.startsWith(p2s))    return "automode-parsing-error";
///   return "automode-blocked";
/// }
/// return "permission-rule";
/// ```
///
/// with `TRt = "Classifier unavailable"` (offset 226748755) and
/// `p2s = "Auto mode could not evaluate this action and is blocking it for
/// safety"` (offset 235392399). `TRt` is matched by EQUALITY and `p2s` by
/// PREFIX — the oracle appends detail after `p2s`, so a prefix test is required.
///
/// The `behavior === "ask"` test runs FIRST and short-circuits: an ask-behavior
/// denial is `user-rejected` even when it also carries a classifier reason.
///
/// Documented narrowing: the oracle additionally requires
/// `t.classifier === "auto-mode"`. `permission::policy_gate::decision_reason_type`
/// maps BOTH classifier variants (`ClassifierApproved` / `ClassifierRejected`)
/// to `"classifier"`, and auto-mode is the only classifier LingXi has, so
/// `type == "classifier"` implies `classifier == "auto-mode"` here. Should a
/// second classifier ever land, this must gain the extra discriminator or it
/// will mislabel that classifier's denials as `automode-*`.
///
/// REACHABILITY (do not read the branches as live): today only
/// `automode-blocked` can actually fire. `PermissionDecisionReason::ClassifierRejected`
/// carries just `{classifier, score}` and no reason text
/// (`permission/src/result.rs`), and `permission::policy_gate::sysmsg_decision_reason`
/// returns `None` for it, so every classifier denial arrives here as
/// `(behavior_ask=false, Some("classifier"), None)`. The `TRt` / `p2s` branches
/// become reachable only once the classifier's reason text is threaded onto the
/// decision. The current OUTPUT is correct — the port's only classifier denial
/// is a genuine block — but the two sibling branches are inert until then.
/// `behavior_ask` is likewise hard-coded `false` at both producers, so the `ask`
/// short-circuit is inert too (same reason the contentBlocks path is documented
/// as dormant at its own call site).
pub(crate) fn tool_denial_kind(
    behavior_ask: bool,
    decision_reason_type: Option<&str>,
    decision_reason: Option<&str>,
) -> &'static str {
    /// claude-code `TRt`.
    const CLASSIFIER_UNAVAILABLE: &str = "Classifier unavailable";
    /// claude-code `p2s`.
    const SAFETY_BLOCK_PREFIX: &str =
        "Auto mode could not evaluate this action and is blocking it for safety";

    if behavior_ask {
        return "user-rejected";
    }
    if decision_reason_type == Some("classifier") {
        let reason = decision_reason.unwrap_or_default();
        if reason == CLASSIFIER_UNAVAILABLE {
            return "automode-unavailable";
        }
        if reason.starts_with(SAFETY_BLOCK_PREFIX) {
            return "automode-parsing-error";
        }
        return "automode-blocked";
    }
    "permission-rule"
}

/// BASH-10 — the `(decision_reason_type, decision_reason)` pair a TOOL-originated
/// permission result contributes to the stdio `can_use_tool` request.
///
/// Mirrors claude-code's two serializers, which the permission crate already
/// implements for its OWN producers but keeps `pub(crate)`:
/// * the `.type` discriminant (`decisionReason?.type`), and
/// * `ZXn(decisionReason)` (2.1.238 BIN off **292948902**), which returns
///   `undefined` for `rule`/`mode`/`subcommandResults`/`permissionPromptTool`
///   and `e.reason` for `classifier`/`hook`/`asyncAgent`/`sandboxOverride`/
///   `workingDir`/`safetyCheck`/`other`.
///
/// `SandboxOverride` is the one that matters here: the oracle sends
/// `decision_reason_type:"sandboxOverride"` with
/// `decision_reason:"dangerouslyDisableSandbox"`. LingXi models that reason as an
/// ENUM, so the string is rendered from the variant.
pub(super) fn tool_ask_reason_context(
    reason: &permission::PermissionDecisionReason,
) -> (Option<String>, Option<String>) {
    use permission::result::SandboxOverrideReason;
    use permission::PermissionDecisionReason as R;
    match reason {
        R::MatchedRule { .. } => (Some("rule".into()), None),
        R::PermissionMode { .. } => (Some("mode".into()), None),
        R::SubcommandResults { .. } => (Some("subcommandResults".into()), None),
        R::PermissionPromptTool { .. } => (Some("permissionPromptTool".into()), None),
        R::ClassifierApproved { .. } => (Some("classifier".into()), None),
        R::ClassifierRejected { reason, .. } => (Some("classifier".into()), Some(reason.clone())),
        R::HookOverride { reason, .. } => (Some("hook".into()), reason.clone()),
        R::AsyncAgent { reason } => (Some("asyncAgent".into()), Some(reason.clone())),
        R::WorkingDirectory { reason } => (Some("workingDir".into()), Some(reason.clone())),
        R::SafetyCheck { reason, .. } => (Some("safetyCheck".into()), Some(reason.clone())),
        R::SandboxOverride { reason } => (
            Some("sandboxOverride".into()),
            Some(
                match reason {
                    SandboxOverrideReason::DangerouslyDisableSandbox => "dangerouslyDisableSandbox",
                    SandboxOverrideReason::ExcludedCommand => "excludedCommand",
                }
                .into(),
            ),
        ),
        R::Other { reason } => (Some("other".into()), Some(reason.clone())),
        // LingXi-internal reasons carry no claude-code `.type`.
        R::DenialLimitExceeded | R::AutoModeFallback | R::BypassPermissions => (None, None),
    }
}

pub(crate) fn rule_decision_otel_source(rule_source: Option<&str>, allow: bool) -> &'static str {
    match rule_source {
        Some("session") => {
            if allow {
                "user_temporary"
            } else {
                "user_reject"
            }
        }
        Some("localSettings" | "userSettings") => {
            if allow {
                "user_permanent"
            } else {
                "user_reject"
            }
        }
        _ => "config",
    }
}

/// A tool-owned ASK is part of the call's permission contract, rather than a
/// replacement for the policy gate's result.  MCP tools expose their clamp via
/// the `Tool` metadata and structured `PermissionDecisionReason`; workflow
/// tools expose the nested Read check through `blocked_path`.  Keep the
/// ordinary Bash sandbox ASK separate so its existing allow-rule/bypass
/// carve-outs remain unchanged.
pub(super) fn tool_permission_ask_is_protected(
    tool: &dyn tool_api::tool_trait::Tool,
    result: &permission::PermissionResult,
) -> bool {
    let permission::PermissionResult::Ask {
        reason, metadata, ..
    } = result
    else {
        return false;
    };
    tool.is_mcp()
        || tool.requires_user_interaction()
        || matches!(
            reason,
            permission::PermissionDecisionReason::PermissionPromptTool { .. }
        )
        || metadata.blocked_path.is_some()
}

/// Whether a tool-owned ASK must not be rescued by a `PermissionRequest`
/// hook.  MCP/org ceilings and explicit `requiresUserInteraction` contracts
/// are hard per-call boundaries.  A Workflow `scriptPath`, however, asks for
/// an ordinary nested `Read` and may be approved by the configured permission
/// handler; its `blocked_path` metadata is still forwarded to the transport,
/// but does not make the hook rescue unsafe.
pub(super) fn tool_permission_ask_blocks_hook_rescue(
    tool: &dyn tool_api::tool_trait::Tool,
    result: &permission::PermissionResult,
) -> bool {
    let permission::PermissionResult::Ask { reason, .. } = result else {
        return false;
    };
    tool.is_mcp()
        || tool.requires_user_interaction()
        || matches!(
            reason,
            permission::PermissionDecisionReason::PermissionPromptTool { .. }
        )
}

/// Preserve the tool's own structured deny provenance when it tightens a
/// policy Allow/Ask.  This is used at the dispatch boundary so a tool-local
/// deny cannot be hidden by an outer allow rule or mode.
pub(super) fn tool_permission_deny_resolution(
    name: &str,
    reason: &permission::PermissionDecisionReason,
    explanation: Option<&str>,
) -> PermissionResolution {
    let (decision_reason_type, decision_reason) = tool_ask_reason_context(reason);
    PermissionResolution::Deny {
        reason: explanation.map_or_else(
            || format!("Permission to use {name} has been denied."),
            str::to_string,
        ),
        source: PermissionDecisionSource::Unspecified,
        rule_source: None,
        decision_reason_type,
        decision_reason,
        behavior_ask: false,
        content_blocks: Vec::new(),
    }
}

/// Results from dispatching a tool batch before the once-per-batch hook runs.
///
/// The two conversation drivers persist the tool results first, then run the
/// deferred `PostToolBatch` event. This split is load-bearing: claude-code
/// observes end-turn metadata only after yielding the results and emits its
/// end-turn telemetry before `PostToolBatch`; the streaming executor also
/// dispatches tools one at a time, so firing the hook inside this function
/// would incorrectly produce one batch event per tool.
pub(crate) struct DeferredToolDispatch {
    pub(crate) results: Vec<ContentBlock>,
    /// Stop requested by a per-tool Pre/PostToolUse hook. This has precedence
    /// over a tool result's end-turn marker and suppresses `PostToolBatch`.
    pub(crate) prevent_continuation: bool,
    pub(crate) injected_messages: Vec<(ConversationMessage, ToolUseId)>,
    pub(crate) context_modifiers: Vec<ContextModifier>,
    pub(crate) post_tool_batch_calls: Vec<hooks::events::PostToolBatchCall>,
}

pub(crate) async fn dispatch_tool_uses_tracked_deferred(
    orch: &ConversationOrchestrator,
    tool_uses: &[(ToolUseId, String, serde_json::Value, Option<String>)],
    // PHASE-2 + DEFERRED-3: per-tool `CancellationToken` (a child of the streaming
    // executor's `tool_abort`) threaded into each tool's
    // `ToolUseContext::cancel`. It fires when the turn is discarded (streaming
    // fallback) OR — because `tool_abort` is parented to the turn's
    // user-interrupt token in `new_with_user_cancel` — when the USER interrupts
    // (DEFERRED-3, ESC / new message). A Cancel-behavior tool (e.g. an in-flight
    // Bash) observes it to return early / SIGKILL its subprocess; the executor
    // then substitutes the synthetic result. `None` for every non-streaming caller
    // (batched turn loop + tests) → no cancellation ever fires.
    cancel: Option<tokio_util::sync::CancellationToken>,
    assistant_message_id: Option<MessageId>,
) -> Result<DeferredToolDispatch, OrchestratorError> {
    let mut results = Vec::with_capacity(tool_uses.len());
    // HOOK.2: OR-fold each tool's PreToolUse `prevent_continuation` signal.
    let mut prevent_continuation = false;
    // SKILLEXEC.3 (Part A): conversation messages a tool wants injected AFTER
    // its tool_result (TS `ToolResult.newMessages`, e.g. the Skill tool's
    // expanded skill prompt). Accumulated in tool-dispatch order and returned to
    // the caller, which appends them to history right after this batch's
    // tool_result user message. Empty for every existing tool → no-op.
    //
    // Each injected message is paired with the `tool_use_id` of the tool that
    // injected it — the faithful port of TS `tagMessagesWithToolUseID`
    // (`tools/utils.ts:12-25`), which stamps every injected `UserMessage` with
    // the Skill tool's OWN `tool_use` block id (`sourceToolUseID`). The caller
    // records the pair into `SessionState::injected_message_sources` (an
    // in-memory side-table, never serialized to JSONL) when it appends the
    // message to history.
    let mut injected_messages: Vec<(ConversationMessage, ToolUseId)> = Vec::new();
    // SKILLEXEC.3 (model scope): one-shot `context_modifier`s a tool returns
    // (TS `ToolResult.contextModifier`, e.g. the Skill tool's `model:` override).
    // Collected in tool-dispatch order and folded POST-BATCH by the caller over a
    // seed context carrying the live `session.model` (see
    // [`apply_model_context_modifiers`]). Empty for every tool that returns
    // `context_modifier: None` (every existing tool + skills WITHOUT a `model:`
    // frontmatter) → the caller does NOTHING → byte-identical.
    let mut context_modifiers: Vec<ContextModifier> = Vec::new();
    // #39 PostToolBatch is assembled after dispatch from the complete assistant
    // tool-use batch. Calls without a yielded result retain
    // `tool_response: None`, matching the oracle's `toolUseBlocks.map(...)` +
    // response-map lookup.
    // FORK (codex #5 follow-up): the rendered system prompt this turn handed the
    // model, recorded by the turn driver after the successful API call. Threaded
    // onto each tool's `ToolUseContext::fork_parent_system_prompt` so a
    // fork-subagent spawn (`AgentTool` with no `subagent_type`) can run its child
    // with a byte-identical system prompt (cache-prefix parity, claude
    // `AgentTool.tsx:622-623`). `None` until the first successful turn / when the
    // turn ran with no system prompt — no non-fork tool reads this field.
    let fork_parent_system_prompt = orch.current_turn_system_prompt().await;
    for (tool_use_id, name, input, provider_id) in tool_uses {
        orch.output.emit_tool_call(tool_use_id, name, input).await;

        // claude-code order (`toolExecution.ts` runToolUse ~401 +
        // checkPermissionsAndCallTool ~683): the unknown-tool check and the
        // `validateInput` gate run at the TOP of `runToolUse` — BEFORE the
        // PreToolUse hooks (~800) and the permission gate. We mirror that here,
        // resolving the tool handle + synthesizing the per-call context first,
        // then running `validate_input`, and only after both clear do the
        // PreToolUse hook + permission gate run below.

        // Unknown-tool arm (claude-code `toolExecution.ts:401`). Runs BEFORE
        // any hook, so there is no pre-hook context to fold — emit the raw
        // wrapped literal verbatim (claude-code's unknown-tool has no hook
        // context).
        let Some(tool_handle) = orch.find_tool_for_dispatch(name) else {
            // Shared builder so this parity-critical string lives in one place
            // (also used by the streaming executor's add_tool).
            let suffix = crate::streaming_executor::unknown_tool_suffix_for(name, orch);
            let result_block = crate::streaming_executor::synthetic_unknown_tool(
                tool_use_id.clone(),
                name,
                provider_id.clone(),
                &suffix,
            );
            // Pass the SAME wrapped string the result_block carries as the
            // model text, so the SDK frame's `content` matches the model wire.
            let model_text = match &result_block {
                ContentBlock::ToolResult { content, .. } => content.clone(),
                _ => format!(
                    "<tool_use_error>Error: No such tool available: {name}{suffix}</tool_use_error>"
                ),
            };
            // O1: claude's unknown-tool arm stamps the persisted line with the
            // BARE string `` `Error: No such tool available: ${name}${suffix}` ``
            // — the unwrapped twin of the `<tool_use_error>` model text. `suffix`
            // is 2.1.263 `Ldt` (Glob/Grep-via-shell, MCP disconnect, …).
            orch.record_tool_use_result(
                tool_use_id,
                serde_json::Value::String(format!("Error: No such tool available: {name}{suffix}")),
            )
            .await;
            orch.emit_tool_result_frame(
                tool_use_id,
                name,
                &model_text,
                &serde_json::json!({ "error": format!("tool not found: {name}") }),
                None,
            )
            .await;
            results.push(result_block);
            continue;
        };

        // BASH-18 `coerceInput` seam (claude-code 2.1.238 BIN off **294282716**,
        // the top of `checkPermissionsAndCallTool`):
        //
        // ```js
        // let h=r,g=null;
        // if(e.coerceInput){ if(g=e.coerceInput(r), g!==null) h=g.input }
        // let y=e.inputSchema.safeParse(h);
        // ```
        //
        // A tool-supplied normalization of the model's raw arguments that runs
        // BEFORE schema validation and, when it fires, REPLACES the input for
        // everything downstream — the schema gate, `validate_input`, the
        // PreToolUse hooks (which read `effective_input`, seeded from `input`
        // below) and `call` — exactly as the oracle threads `y.data` onward.
        // `None` (the oracle's `null`) leaves the raw input untouched, which is
        // every tool but `Bash` today, so this is a strict no-op there.
        //
        // NOT emitted: the oracle's `tengu_tool_input_coerced` analytics event.
        // It is a pure telemetry dimension (`shapeClass` / `outcome`) with no
        // model-visible bytes, and registering a new tengu name would move the
        // parity telemetry-registry count. `CoercedInput::shape_class` carries
        // the value for a future wiring.
        let coerced_input = tool_handle.coerce_input(input);
        let input: &serde_json::Value = coerced_input.as_ref().map_or(input, |c| &c.input);
        let normalized_input = tool_handle.parse_native_input(input).and_then(Result::ok);
        let input = normalized_input.as_ref().unwrap_or(input);

        // JSON-schema input gate (claude-code `toolExecution.ts:615`
        // `inputSchema.safeParse`): runs on the RAW `input` (pre-hook), AFTER the
        // unknown-tool arm and BEFORE the `validate_input` gate — the exact order
        // of `checkPermissionsAndCallTool` (safeParse ~615 precedes validateInput
        // ~683). Native refinement issues accompany the exported JSON Schema
        // so the detail uses Claude's Zod `zue` grouping and JSON fallback. A
        // malformed tool schema is treated as PASS (logged) — see
        // [`crate::schema_validation::validate_tool_input_schema`].
        if let Err(schema_error) =
            crate::schema_validation::validate_tool_schema_detailed(tool_handle.as_ref(), input)
        {
            let detail = &schema_error.display;
            tool_handle
                .on_input_schema_rejected(
                    input,
                    Some(tool_use_id.as_str()),
                    assistant_message_id.as_ref(),
                )
                .await;
            let model_text =
                format!("<tool_use_error>InputValidationError: {detail}</tool_use_error>");
            let result_block = ContentBlock::ToolResult {
                tool_use_id: tool_use_id.clone(),
                content: model_text.clone(),
                is_error: true,
                provider_tool_use_id: provider_id.clone(),
                content_blocks: None,
            };
            // Native .270 persists raw ZodError.message, while only the model
            // block uses the enriched grouped diagnostic (Yge).
            orch.record_tool_use_result(
                tool_use_id,
                serde_json::Value::String(format!("InputValidationError: {}", schema_error.raw)),
            )
            .await;
            orch.emit_tool_result_frame(
                tool_use_id,
                name,
                &model_text,
                &serde_json::json!({ "error": detail }),
                None,
            )
            .await;
            results.push(result_block);
            continue;
        }

        // Synthesize a minimal ToolUseContext — needed by the validate_input
        // gate below and reused by the eventual `tool_handle.call()`.
        let (messages, model, model_profile) = {
            let s = orch.session.lock().await;
            (
                s.model_context_history(),
                s.model.clone(),
                s.model_profile.clone(),
            )
        };
        let ctx = ToolUseContext {
            options: ToolUseOptions {
                debug: false,
                verbose: false,
                // The LIVE session model (updated by `/model` switches / resume),
                // not `config.model` (frozen at launch). Tools gate model-facing
                // behavior on this — e.g. WebSearch's hosted-vs-client-side split
                // needs the current model, so a switched/resumed non-Claude model
                // resolves correctly.
                main_loop_model: model,
                model_profile,
                max_budget_nano_usd: None,
                mcp_clients: Vec::new(),
                // FIX 3: claude-code's main REPL builds `getToolUseContext` with
                // `isNonInteractiveSession: false` (REPL.tsx:2427). LingXi hardcoded
                // `true` here — the OPPOSITE — which would flip the model-facing
                // verification-nudge + fork-subagent paths in an interactive
                // session. Use the orchestrator's own print/headless signal
                // `!interactive_permissions` (the SAME signal the defer path uses at
                // turn_loop.rs ~1729/1754): `true` only in a non-interactive
                // (print/headless) session. Inert under today's default-off feature
                // flags, but removes the latent divergence.
                is_non_interactive_session: !orch.config.interactive_permissions,
                custom_system_prompt: orch.config.system_prompt_override.clone(),
                append_system_prompt: None,
            },
            messages,
            tool_use_id: Some(tool_use_id.clone()),
            assistant_message_id,
            agent_id: None,
            // Main / leader thread: no teammate identity (TS getAgentName() /
            // getTeammateContext() are undefined here).
            agent_name: None,
            team_name: None,
            origin_session_id: None,
            tool_execution_policy: platform_api::tool_invoker::ToolExecutionPolicy::Ordinary,
            content_replacement_state: None,
            session: Some(orch.session.clone()),
            observer_pairings: orch.model_runtime.observer_pairings.clone(),
            subagent_registry: Some(orch.tools.clone()),
            // PHASE-2: hand each tool a clone of the sibling cancel token (the
            // streaming executor passes a per-tool child; every other caller
            // passes `None`). Clone per-tool since this loop may dispatch a
            // batch (the streaming executor calls one-tool-at-a-time).
            cancel: cancel.clone(),
            // FORK-ONLY: the parent's rendered system prompt for THIS turn (the
            // bytes the model saw), recorded by the turn driver after the API
            // call. On the fork path `AgentTool` threads it onto the child's
            // `SubagentSpawnRequest.fork_parent_system_prompt` for a
            // byte-identical cache prefix. `None` until the first successful turn
            // / a turn with no system prompt; no non-fork tool reads it.
            fork_parent_system_prompt: fork_parent_system_prompt.clone(),
            // Main turn loop uses the shared session workspace (no per-agent
            // cwd override); only an isolated subagent sets this.
            cwd: None,
            depth: 0,
            observer: None,
            // (/rewind) Hand each write tool the file-history sink (a trait view
            // of the shared checkpoint store) so pre-edit content is backed up.
            file_history: orch
                .file_history
                .clone()
                .map(|fh| fh as std::sync::Arc<dyn platform_api::FileHistorySink>),
        };

        // validate_input gate (claude-code `toolExecution.ts:683-723`): a
        // `validateInput` failure wraps the message in `<tool_use_error>` and
        // short-circuits. Runs on the RAW `input` (pre-hook), BEFORE the
        // PreToolUse hooks/permission (claude-code order), so there is no
        // pre-hook context to fold.
        if let Err(tool_api::ValidationError(msg)) = tool_handle.validate_input(input, &ctx).await {
            let model_text = format!("<tool_use_error>{msg}</tool_use_error>");
            let result_block = ContentBlock::ToolResult {
                tool_use_id: tool_use_id.clone(),
                content: model_text.clone(),
                is_error: true,
                provider_tool_use_id: provider_id.clone(),
                content_blocks: None,
            };
            // O1: claude's validate_input arm (2.1.220 BIN off 235407190)
            // stamps `` toolUseResult: `Error: ${T.message}` `` — the unwrapped
            // twin of the `<tool_use_error>` model text.
            orch.record_tool_use_result(
                tool_use_id,
                serde_json::Value::String(format!("Error: {msg}")),
            )
            .await;
            orch.emit_tool_result_frame(
                tool_use_id,
                name,
                &model_text,
                &serde_json::json!({ "error": msg }),
                None,
            )
            .await;
            results.push(result_block);
            continue;
        }

        // claude-code `toolExecution.ts:413-453`: if the user-interrupt token
        // is already cancelled at the top of runToolUse (a pre-cancel — ESC
        // fired before this tool got CPU), emit the bare CANCEL_MESSAGE as an
        // is_error tool_result and skip execution. Mirrors the TS guard exactly:
        // the bare string (NOT `<tool_use_error>`-wrapped), `is_error: true`,
        // and `continue` without pushing to `post_tool_batch_calls` (tool
        // didn't run). A `None` cancel token → guard never fires.
        if cancel.as_ref().is_some_and(|t| t.is_cancelled()) {
            let result_block = ContentBlock::ToolResult {
                tool_use_id: tool_use_id.clone(),
                content: CANCEL_MESSAGE.to_string(),
                is_error: true,
                provider_tool_use_id: provider_id.clone(),
                content_blocks: None,
            };
            // Denial provenance: claude-code HARDCODES `toolDenialKind:"cancelled"`
            // at this site (binary offset 235399713) rather than routing through
            // its `YDd` abort-reason classifier, so this needs no abort-reason
            // plumbing to be faithful. `cancelled` is not one of the five kinds
            // the permission classifier emits, but it IS an ordinary
            // `toolDenialKind` value that produces a `tool_result_meta` entry.
            orch.record_tool_denial_kind(tool_use_id, "cancelled").await;
            // O1: the same site sets `toolUseResult: FK` (2.1.220 BIN off
            // 235398916), where `FK` (BIN off 229154836) IS `CANCEL_MESSAGE` —
            // the identical string this block's model content already carries.
            orch.record_tool_use_result(
                tool_use_id,
                serde_json::Value::String(CANCEL_MESSAGE.to_string()),
            )
            .await;
            orch.emit_tool_result_frame(
                tool_use_id,
                name,
                CANCEL_MESSAGE,
                &serde_json::json!({ "error": CANCEL_MESSAGE }),
                Some("cancelled"),
            )
            .await;
            results.push(result_block);
            continue;
        }

        // M5-06 Task 14: PreToolUse hook chain. Build the event + context,
        // call the executor, and either Block (turn the response into an
        // error ToolResult), apply modified_input, or continue.
        // FIX 2: populate `transcript_path` + `permission_mode` on the PreToolUse
        // context (and the PostToolUse fire below, which reuses this `hook_ctx`),
        // matching claude-code `createBaseHookInput` (always sets
        // `transcript_path: getTranscriptPathForSession(...)`, utils/hooks.ts:322)
        // plus PreToolUse/PostToolUse's `permission_mode =
        // appState.toolPermissionContext.mode` (toolHooks.ts:471). The hook reads
        // the enforcing gate's live wire mode; session `plan_mode` remains the
        // explicit override used while the plan workflow is active.
        //
        // FIX A: the transcript path is the live JSONL writer's path when one is
        // wired (preserves the writer-backed tests) ELSE the deterministically-
        // computed `<config_home>/projects/<sanitize(cwd)>/<uuid>.jsonl`. In
        // PRODUCTION no writer is wired, so the prior `unwrap_or_default()` made
        // EVERY PreToolUse/PostToolUse hook carry an empty `transcript_path`.
        let (session_id, plan_mode) = {
            let s = orch.session.lock().await;
            (s.session_id, s.plan_mode)
        };
        let transcript_path = orch
            .transcript
            .jsonl_writer
            .as_ref()
            .map(|w| w.path().to_path_buf())
            .unwrap_or_else(|| orch.computed_transcript_path(&session_id));
        let permission_mode = Some(if plan_mode {
            "plan".to_string()
        } else {
            orch.permission_mode()
                .unwrap_or_else(|| "default".to_string())
        });
        // `prompt_id` on the shared hook-input base (oracle `createBaseHookInput`
        // / minified `c_`: `prompt_id:Vut()??void 0`) — the process-wide current
        // prompt id, shared with the JSONL `user` lines and the OTel `prompt.id`
        // attribute, so a PreToolUse/PostToolUse hook's output joins to OTel
        // events at prompt grain.
        let prompt_id = orch.prompt_runtime.current_prompt_id.lock().await.clone();
        let hook_ctx = HookContext {
            prompt_transcript: Some(orch.prompt_hook_transcript().await),
            session_id,
            cwd: orch.current_cwd(),
            transcript_path,
            prompt_id,
            permission_mode,
            trace_context: telemetry::otel::capture_current_trace_context(),
            ..Default::default()
        };
        let pre_event = HookEvent::PreToolUse {
            tool_name: name.clone(),
            tool_input: input.clone(),
            tool_use_id: tool_use_id.clone(),
        };
        let pre_started = std::time::Instant::now();
        tracing::info!(
            event = orch_events::HOOK_PRE_STARTED,
            tool_name = %name,
        );
        telemetry::otel::emit_hook_lifecycle("pre", "started", name, None);
        let pre_agg = orch.hooks.execute(pre_event, hook_ctx.clone()).await;
        // hook duration bounded by tokio timeout — u128 ms cannot exceed u64::MAX
        #[allow(clippy::cast_possible_truncation)]
        let pre_dur_ms = pre_started.elapsed().as_millis() as u64;

        // HOOK.2: a PreToolUse hook's `continue:false` (preventContinuation)
        // signal — OR-folded so a later turn-step disposition ends the loop.
        // Captured BEFORE any early `continue` so a blocking hook that also
        // requested preventContinuation still stops the loop (TS yields
        // preventContinuation in the pre-hook phase regardless of the block).
        if pre_agg.prevent_continuation {
            prevent_continuation = true;
        }
        // #40 terminalSequence apply (claude-code `szn`, BIN off 205755390): a
        // hook may return a top-level `terminalSequence` for LingXi to emit
        // (OSC 9 / 777 desktop notification, etc.). Run the allowlist validator
        // (`NEo`) over the folded sequence: on REJECT, warn (the observable half,
        // byte-faithful to claude-code's reject message). On ACCEPT the
        // validated string is emitted through the output bridge to the active
        // TUI terminal (`BEo`). No-op when no hook set it.
        apply_terminal_sequence(orch, name, pre_agg.terminal_sequence.as_deref()).await;
        // HOOK.1: a PreToolUse hook's `additionalContext` (NOT `systemMessage`)
        // becomes its OWN meta message, not folded into the tool_result — claude
        // pushes it to `resultingMessages` (`toolExecution.ts:845`). Shape:
        // `<system-reminder>`-wrapped `PreToolUse:{tool} hook additional context:
        // {ctx}`, contexts joined by `\n` (`messages.ts:4117-4128`). `systemMessage`
        // is excluded — its `hook_system_message` `normalizeAttachmentForAPI`→`[]`
        // never reaches the model (`messages.ts:4258`). Built in the PRE-hook phase
        // (`toolExecution.ts:846`), so it surfaces on success/block/deny alike,
        // ordered after that arm's tool_result. Tagged with `tool_use_id` (TS
        // `toolUseID`); no-op when empty.
        let pre_hook_messages = pre_agg.additional_contexts.clone();
        // Build the standalone additionalContext message (HOOK.1) and queue it
        // on the `injected` channel, tagged with THIS tool's `tool_use_id` (TS
        // stamps `toolUseID` on the attachment). A strict no-op when the hook
        // emitted no context, so the locked turn-loop fixtures (noop hooks) are
        // unaffected. claude-code pushes `additionalContext` to
        // `resultingMessages` in the PRE-hook phase (`toolExecution.ts:846`),
        // BEFORE the permission/block check — so it surfaces even when the tool
        // is later BLOCKED (preventContinuation) or DENIED. We therefore emit it
        // on the SUCCESS, BLOCK, and DENY paths alike, in every case ordered
        // AFTER that path's tool_result (matching claude-code post-hoist).
        let pre_context_message: Option<ConversationMessage> = if pre_hook_messages.is_empty() {
            None
        } else {
            // O3: the PERSISTED record is a `hook_additional_context`
            // ATTACHMENT line (2.1.220 BIN off 234733097 for the PreToolUse
            // producer), queued here and flushed after this tool's tool_result.
            orch.queue_hook_attachment(
                tool_use_id,
                hooks::additional_context_attachment(
                    &format!("PreToolUse:{name}"),
                    tool_use_id.as_str(),
                    "PreToolUse",
                    &pre_hook_messages,
                ),
            )
            .await;
            let body = pre_hook_messages.join("\n");
            // O3: the model-facing rendering is `zr({content: Ww(…),
            // isMeta:true})` (renderer table BIN off 238107100) and is
            // EPHEMERAL — built from the attachment at API-normalization time
            // and never persisted. `user_meta` marks it so both drivers skip
            // persisting it; the attachment above IS the on-disk record.
            Some(ConversationMessage::user_meta(
                MessageId::new(),
                format!(
                    "<system-reminder>\nPreToolUse:{name} hook additional context: {body}\n</system-reminder>"
                ),
            ))
        };

        // FIX C (hook_stopped_continuation, PreToolUse twin): a PreToolUse hook's
        // `continue:false` (preventContinuation) becomes its OWN meta message —
        // claude yields it AFTER the tool_result on the SUCCESS path
        // (`toolExecution.ts:1571-1582`, inside the post-execution try-block),
        // using `stopReason || 'Execution stopped by hook'` and hookName
        // `PreToolUse:{tool}`. The tool STILL runs (the pre-hook only OR-folds the
        // end-turn signal at line ~2363); the message is emitted after that tool's
        // tool_result, tagged with this tool's `tool_use_id`. Built here alongside
        // `pre_context_message` but pushed ONLY on the success path below — a
        // Block/Defer never executes the tool, so claude's post-execution site
        // never fires there. `pre_agg.reason` carries the parsed `stopReason`
        // (`hook_payload.rs:1113`). `None` when the hook did not request
        // preventContinuation (the common case), a strict no-op.
        // O2: the message is paired with the `hook_stopped_continuation`
        // ATTACHMENT the oracle records beside it (BIN off 235403061). Both are
        // built here but published together on the success path below, so the
        // record cannot drift away from the prose it describes.
        let pre_prevent: Option<(ConversationMessage, serde_json::Value)> = if pre_agg
            .prevent_continuation
        {
            let reason = pre_agg
                .reason
                .clone()
                .unwrap_or_else(|| "Execution stopped by hook".to_string());
            let attachment = hooks::stopped_continuation_attachment(
                &hooks::HookAttachmentIdentity {
                    hook_name: format!("PreToolUse:{name}"),
                    hook_event: "PreToolUse".to_string(),
                    tool_use_id: tool_use_id.as_str().to_string(),
                },
                &reason,
            );
            Some((
                ConversationMessage::user_meta(
                    MessageId::new(),
                    format!(
                        "<system-reminder>\nPreToolUse:{name} hook stopped continuation: {reason}\n</system-reminder>"
                    ),
                ),
                attachment,
            ))
        } else {
            None
        };

        // #37 `permissionDecision:"defer"` (claude BIN off 202454844): a PreToolUse
        // hook defers a tool to a later interactive resume. Gated to (1) non-
        // interactive mode and (2) a SOLO batch; else warns and falls through.
        // On the gated path: emit `tengu_pre_tool_hook_deferred`, push a
        // `hook_deferred_tool` meta, TERMINATE (`tool_deferred`, tool not run).
        // `is_non_interactive_session = !interactive_permissions`; DORMANT on the
        // default REPL (interactive ignores defer).
        if matches!(pre_agg.decision, Some(HookDecision::Defer)) {
            // The deferred attachment uses the source of the hook that owns the
            // folded Defer decision. Fall back to the event-qualified name only
            // for legacy/custom executors that omitted provenance.
            let hook_name = pre_agg
                .hook_source
                .map(hooks::HookSource::deferred_label)
                .map(str::to_string)
                .unwrap_or_else(|| format!("PreToolUse:{name}"));
            let is_non_interactive = !orch.config.interactive_permissions;
            // batch size = the number of tool_use blocks this dispatch is
            // processing (claude-code counts `tool_use` blocks in the assistant
            // message via `Wn(s.message.content, te=>te.type==="tool_use")`).
            let batch_tool_count = tool_uses.len();
            if !is_non_interactive {
                tracing::warn!(
                    tool_name = %name,
                    "Hook {hook_name} returned permissionDecision=defer in interactive mode; ignoring (defer is print-mode only)"
                );
                // ignored → fall through to the normal gate by clearing Defer.
                // (handled below: the Defer decision is treated as no-decision)
            } else if batch_tool_count > 1 {
                tracing::warn!(
                    tool_name = %name,
                    "Hook {hook_name} returned permissionDecision=defer but {batch_tool_count} tool calls are in this batch; ignoring (defer is solo-only \u{2014} siblings would be orphaned on resume)"
                );
                // ignored → fall through to the normal gate.
            } else {
                // GATED path: honor the defer. Emit the analytic (inline event
                // name, NOT a locked const — same pattern as
                // `tengu_model_fallback_triggered`, so the 347 registry is
                // untouched), push the `hook_deferred_tool` meta message, and
                // terminate the turn (`tool_deferred` stop-reason — the tool is
                // not executed).
                // Persist the same live mode already supplied to the hook
                // payload, including acceptEdits/bypassPermissions/dontAsk/auto.
                let permission_mode = hook_ctx.permission_mode.as_deref().unwrap_or("default");
                tracing::info!(
                    event = "tengu_pre_tool_hook_deferred",
                    tool_name = %name,
                );
                tracing::info!(
                    event = orch_events::HOOK_PRE_COMPLETED,
                    tool_name = %name,
                    decision = "defer",
                    duration_ms = pre_dur_ms,
                );
                // O2: the `hook_deferred_tool` record is PERSISTED, never sent
                // to the model. Its renderer is `hook_deferred_tool:()=>[]`
                // (BIN off 238109388), and the record is FUNCTIONAL rather than
                // cosmetic — it is the resume protocol:
                //   * `QAs` (BIN off 237925753) scans the transcript's last
                //     1 MiB backwards for `'"hook_deferred_tool"'`, requiring
                //     `type:"attachment"` with that inner type, and rejects the
                //     deferral if a LATER line carries this `toolUseID`.
                //   * the stream-json engine (BIN off 240899919) rebuilds
                //     `{id, name, input}` from it with `stop_reason`
                //     `"tool_deferred"`.
                //
                // Previously the port pushed the raw JSON as a plain (non-meta)
                // user message: the model read a blob claude suppresses AND no
                // resume scanner could ever find the deferral. BEHAVIOR CHANGE:
                // the `-p`/print-mode defer path no longer sends that message.
                //
                // Persisted IMMEDIATELY rather than queued — the tool-keyed
                // queue is flushed after a `tool_result`, and a deferred tool
                // never produces one, so a queued record would strand.
                //
                // `toolInput` is the HOOK-UPDATED input (oracle `b`, set by the
                // `case"hookUpdatedInput"` arm before the defer yield at BIN
                // off 235409134) — NOT the raw model input. `effective_input`
                // is computed further down, after this block, so the same
                // `modified_input` fold is applied here.
                let deferred_input = pre_agg
                    .modified_input
                    .clone()
                    .unwrap_or_else(|| input.clone());
                orch.persist_hook_attachment_to_jsonl(hooks::deferred_tool_attachment(
                    tool_use_id.as_str(),
                    name,
                    &deferred_input,
                    &hook_name,
                    permission_mode,
                    hook_ctx
                        .trace_context
                        .as_ref()
                        .map(|context| context.traceparent.as_str()),
                ))
                .await;
                // HOOK.1: any PreToolUse additionalContext, ordered AFTER the
                // deferred-tool record (matching the Block arm). Its attachment
                // was queued above against this tool id; drain it now, since
                // the usual post-`tool_result` flush will never run here.
                orch.flush_hook_attachments(tool_use_id).await;
                if let Some(msg) = pre_context_message {
                    injected_messages.push((msg, tool_use_id.clone()));
                }
                // TERMINATE the turn — the deferred tool is NOT executed. The
                // `tool_deferred` stop-reason has no distinct LingXi turn-stop
                // variant; reuse the `prevent_continuation` end-of-turn signal so
                // the agent loop stops after this batch (the deferred tool's
                // result is intentionally absent). `continue` skips this tool's
                // execution entirely.
                prevent_continuation = true;
                continue;
            }
        }

        if matches!(pre_agg.decision, Some(HookDecision::Block)) {
            // claude-code maps a PreToolUse `decision:"block"` to
            // `permissionBehavior:"deny"` with `blockingError = reason ||
            // "Blocked by hook"`, then renders the model-facing deny message via
            // `aAs(hookName, blockingError)` = `` `${hookName} hook error:
            // ${blockingError}` ``. For PreToolUse the hook name is
            // `PreToolUse:${toolName}`, so the tool_result the model sees is
            // `"PreToolUse:<name> hook error: <reason>"` (fallback reason
            // "Blocked by hook", capital B).
            let reason = pre_agg
                .reason
                .clone()
                .unwrap_or_else(|| "Blocked by hook".into());
            tracing::info!(
                event = orch_events::HOOK_PRE_COMPLETED,
                tool_name = %name,
                decision = "block",
                duration_ms = pre_dur_ms,
            );
            let model_text = format!("PreToolUse:{name} hook error: {reason}");
            let result_block = ContentBlock::ToolResult {
                tool_use_id: tool_use_id.clone(),
                content: model_text.clone(),
                is_error: true,
                provider_tool_use_id: provider_id.clone(),
                content_blocks: None,
            };
            orch.emit_tool_result_frame(
                tool_use_id,
                name,
                &model_text,
                &serde_json::json!({ "error": model_text.clone() }),
                None,
            )
            .await;
            results.push(result_block);
            // HOOK.1: even on a BLOCK, the PreToolUse `additionalContext` was
            // pushed in claude-code's pre-hook phase (`toolExecution.ts:846`),
            // before the block check — so surface it here, ordered AFTER this
            // path's error tool_result. No-op when the hook emitted no context.
            if let Some(msg) = pre_context_message {
                injected_messages.push((msg, tool_use_id.clone()));
            }
            continue;
        }

        // Apply modified_input if any hook mutated the tool input. Mutable so a
        // PermissionRequest hook 'allow' can further rewrite the input before the
        // tool runs (claude-code `updatedInput`).
        let mut effective_input = pre_agg
            .modified_input
            .clone()
            .unwrap_or_else(|| input.clone());
        tracing::info!(
            event = orch_events::HOOK_PRE_COMPLETED,
            tool_name = %name,
            decision = match pre_agg.decision {
                Some(HookDecision::Allow) => "allow",
                Some(HookDecision::Approve) => "approve",
                Some(HookDecision::Continue) => "continue",
                Some(HookDecision::Block) => "block",
                // #37: a Defer that reached here was IGNORED (interactive mode or
                // a multi-tool batch) — the gated path `continue`d above, so this
                // arm only fires for the ignored case, which proceeds to the
                // normal permission gate exactly like no decision.
                Some(HookDecision::Defer) => "defer-ignored",
                // R-D3: a `permissionDecision:"ask"` parses to `HookDecision::Ask`
                // and forces the interactive prompt even over a configured allow
                // rule — routed in the normal-gate branch below (an Allow
                // resolution is upgraded to Ask when `hook_ask`). Deny rules and
                // plan mode still bind (deny > ask > allow).
                Some(HookDecision::Ask) => "ask",
                None => "none",
            },
            duration_ms = pre_dur_ms,
        );
        telemetry::otel::emit_hook_lifecycle("pre", "completed", name, Some(pre_dur_ms));

        // HOOK.3: a PreToolUse hook's permissionDecision "allow" (legacy
        // `decision: "approve"`) bypasses the permission gate for this tool call
        // (TS `resolveHookPermissionDecision`: a hook 'allow' skips the
        // interactive prompt). Both wire forms parse to `HookDecision::Approve`.
        // A hook "deny"/"block" already short-circuited above (parsed to
        // `HookDecision::Block`); "ask" / no-decision leave `pre_agg.decision`
        // unset and fall through to the normal gate.
        //
        // HOOK.3 resolution: a hook 'allow' skips the interactive PROMPT but
        // STILL applies rule-based deny/ask (claude-code
        // `resolveHookPermissionDecision` + `checkRuleBasedPermissions`) — a hook
        // CANNOT override an explicit deny rule or the active mode's mutation
        // backstop. So we ALWAYS consult the gate: `check_after_hook_allow`
        // (deny rules + mode bind, the prompt is skipped) when a hook approved,
        // else the normal `check` (which may delegate an `Ask` to the prompt
        // transport). Uses the post-hook `effective_input` so a Pre hook can
        // rewrite a tool argument before the permission check sees it.
        let requires_user_interaction = tool_handle.requires_user_interaction();
        let restricted_protected_mutation = orch
            .perms
            .is_restricted_protected_mutation(name, &effective_input);
        let hook_allowed = matches!(
            pre_agg.decision,
            Some(HookDecision::Approve | HookDecision::Allow)
        ) && !requires_user_interaction;
        // R-D3: a PreToolUse hook `permissionDecision:"ask"` forces the interactive
        // prompt even over a configured allow rule (the resolution upgrade in the
        // normal-gate branch below). Mutually exclusive with `hook_allowed`.
        let hook_ask = matches!(pre_agg.decision, Some(HookDecision::Ask));
        // HOOK.4 — plan-mode dynamic gate (claude's live `mode='plan'`): authorize
        // under `PermissionMode::Plan` so the mutation backstop activates on a
        // runtime `EnterPlanMode` (`check_in_plan_mode`), binding OVER a hook 'allow'
        // (HOOK.3 issue 1). Lock read-and-dropped here. Deny-arm carry-overs from the
        // SOURCED resolution (`toolExecution.ts:1040`), since `Deny` carries only
        // `reason`: `reject_content_blocks` (top-level deny blocks, `ask` only) +
        // `deny_hook_says_retry` (classifier `{retry:true}`, `toolExecution.ts:1090`).
        // Both inert on normal denies.
        let mut reject_content_blocks: Vec<ContentBlock> = Vec::new();
        let mut deny_hook_says_retry = false;
        // Same carry-over pattern for the denial provenance: `PermissionDecision`
        // collapses to `Deny { reason }`, so the structured signals
        // `tool_denial_kind` needs (`behavior_ask` / `decision_reason_type` /
        // `decision_reason`) must be classified in the SOURCED arm and carried
        // out to the emit site. "permission-rule" is the oracle's own fallthrough
        // and stays correct for every arm that carries no classifier reason
        // (hook Block, plan-mode, unknown).
        let mut denial_kind: &'static str = "permission-rule";
        let plan_mode = orch.session.lock().await.plan_mode;
        // ORPHAN RECOVERY: a re-dispatched orphaned tool carries a forced
        // permission decision (its recovered `control_response`) that REPLACES the
        // interactive gate — twin of claude-code's forced `canUseTool` in
        // `handleOrphanedPermission` (queryHelpers.ts:278-284). Consumed (removed)
        // on read so it binds exactly this `tool_use` once. The map is empty on
        // every normal turn, so this is a strict no-op there (byte-locked
        // turn-loop fixtures unchanged). PreToolUse hooks above STILL ran (so do
        // claude-code's, via `runTools`); only the permission decision is forced.
        let forced_decision = orch
            .orphan_forced_decisions
            .lock()
            .await
            .remove(tool_use_id);
        // Every real dispatch performs the tool-owned permission check before
        // any policy outcome can reach `call`.  This is deliberately outside
        // the policy-resolution branches: an explicit allow rule, bypass mode,
        // hook allow, plan allow, or orphan recovery must not skip a tool-local
        // deny/ask (MCP ceilings, requiresUserInteraction, and Workflow's
        // nested Read check all live here).
        let tool_permission_result = tool_handle.check_permissions(&effective_input, &ctx).await;
        let tool_ask_is_protected =
            tool_permission_ask_is_protected(tool_handle.as_ref(), &tool_permission_result);
        let tool_ask_blocks_hook_rescue =
            tool_permission_ask_blocks_hook_rescue(tool_handle.as_ref(), &tool_permission_result);
        let non_normal_permission_path = forced_decision.is_some() || plan_mode || hook_allowed;
        // OTEL `code_edit_tool.decision` / `tool_decision` source label,
        // threaded out of the decision branches below.
        //
        // claude-code has two publishers: `qtd` (driven by the permission
        // checker's own `logDecision`, which also records
        // `toolDecisions[toolUseID]`) and the dispatch-site one, guarded on
        // `toolDecisions?.[t] === void 0`, which derives the label from the
        // structured decision reason with `eQ_`. The port has no `logDecision`
        // twin — no gate emits OTEL and nothing writes a `toolDecisions`
        // record — so this site, which is the dispatch-site publisher's twin,
        // always fires and `eQ_` is the whole taxonomy:
        //   rule                       → `ZX_` (see [`rule_decision_otel_source`])
        //   hook                       → "hook"
        //   permissionPromptTool       → the host's `decisionClassification`,
        //                                defaulting to user_temporary/user_reject
        //   other (request aborted)    → "user_abort"
        //   mode/classifier/…/no reason→ "config"
        let mut decision_otel_source: &'static str = "config";
        let decision = if let Some(forced) = forced_decision {
            // A recovered orphan's forced `control_response` — no structured
            // decision reason survives the recovery (CC default arm).
            decision_otel_source = "unknown";
            forced
        } else if hook_allowed && !plan_mode {
            // Carry the REAL tool_use_id so a hook-allow→ask-rule re-check emits a
            // byte-faithful stdio `can_use_tool` (correlatable id + decision_reason).
            let ctx = platform_api::permission_gate::PermissionCheckContext {
                tool_use_id: Some(tool_use_id.to_string()),
                requires_user_interaction,
                suppress_always_allow_rule: requires_user_interaction
                    || restricted_protected_mutation,
                ..Default::default()
            };
            let hook_outcome = orch
                .perms
                .check_after_hook_allow_outcome_ctx(name, &effective_input, &ctx)
                .await;
            let mut hook_decision_classification = None;
            let hook_decision = match hook_outcome {
                platform_api::permission_gate::PermissionOutcome::Allow {
                    updated_input,
                    decision_classification,
                    permission_updates: _,
                } => {
                    hook_decision_classification = decision_classification;
                    if let Some(updated) = updated_input {
                        effective_input = updated;
                    }
                    PermissionDecision::Allow
                }
                platform_api::permission_gate::PermissionOutcome::AllowAuto { updated_input } => {
                    if let Some(updated) = updated_input {
                        effective_input = updated;
                    }
                    if let Err(error) = orch.perms.set_permission_mode("auto").await {
                        // The current call was explicitly approved, but never
                        // claim Auto mode when the atomic live-mode write fails.
                        tracing::warn!(%error, "permission prompt approved Auto mode but mode switch failed");
                    } else {
                        // Upstream 2.1.270 flattens a decision source with
                        // `case "user": return e.permanent ? "user_permanent"
                        // : "user_temporary"`, so a user's allow-once is
                        // `user_temporary`, not `hook`. Set the CLASSIFICATION
                        // rather than the label: the assignment after this
                        // `match` is unconditional, so writing the label here
                        // could never be observed (it was, and was not).
                        hook_decision_classification = Some(
                            platform_api::permission_gate::ToolDecisionClassification::UserTemporary,
                        );
                    }
                    PermissionDecision::Allow
                }
                platform_api::permission_gate::PermissionOutcome::Deny { reason } => {
                    PermissionDecision::Deny { reason }
                }
            };
            // The hook only OWNS the label when its allow stands: `han` returns
            // the hook's own `{behavior:"allow"}` (decisionReason `hook`) there,
            // but when the re-check overrides it (`Hook returned '…' but deny
            // rule overrides`) the decision — and therefore the label — is the
            // RULE's, which `ZX_` renders as "config" for every non-user-owned
            // SettingSource. `PermissionDecision` is 2-valued, so the overriding
            // rule's own scope is not separable here.
            decision_otel_source = if matches!(hook_decision, PermissionDecision::Allow) {
                hook_decision_classification.map_or(
                    "hook",
                    platform_api::permission_gate::ToolDecisionClassification::as_str,
                )
            } else {
                "config"
            };
            hook_decision
        } else {
            // NORMAL permission path. Resolve the decision SOURCE first (without
            // delegating to the prompt transport) so the source-gated permission
            // hooks fire the way claude-code does.
            let resolution_ctx = platform_api::permission_gate::PermissionCheckContext {
                tool_use_id: Some(tool_use_id.to_string()),
                requires_user_interaction,
                suppress_always_allow_rule: requires_user_interaction
                    || restricted_protected_mutation,
                is_non_interactive_session: !orch.config.interactive_permissions,
                ..Default::default()
            };
            let resolution = if plan_mode {
                orch.perms
                    .resolve_detailed_in_plan_mode_or_abort(name, &effective_input, &resolution_ctx)
                    .await
            } else {
                orch.perms
                    .resolve_detailed_or_abort(name, &effective_input, &resolution_ctx)
                    .await
            }
            .map_err(|abort| OrchestratorError::PermissionAbort {
                message: abort.message,
            })?;
            // R-D3: a PreToolUse hook `permissionBehavior:"ask"` (HookDecision::Ask)
            // forces the interactive prompt even over a configured ALLOW rule, but a
            // DENY rule still overrides the hook. This is 1:1 with claude-code's
            // `applyHookPermissionResult` (`JWn`): on a hook `ask`/`allow` it RE-RUNS
            // the rule resolution (`EPe`) and `if (p?.behavior === "deny") return …
            // "deny rule overrides"`, so the deny rule wins; only when no deny rule
            // matches does the hook `ask` fall through to the full permission pipeline
            // (the interactive prompt). Here `resolve_detailed` has already applied
            // that rule precedence, so upgrading ONLY the resolved `Allow` to `Ask`
            // reproduces it exactly: a resolved `Deny` keeps binding (the deny rule
            // overrides), plan mode already bound above, and a resolved `Ask` already
            // prompts. Precedence is therefore deny > ask > allow — matching the
            // binary, NOT a divergence. No-op unless a hook returned `ask`.
            // This flag is metadata for an existing Ask/callback path; it must
            // not create an Ask by itself. The normal TUI tool owns its question
            // UI, and a generic permission prompt here would duplicate it.
            let resolution = if hook_ask && matches!(resolution, PermissionResolution::Allow { .. })
            {
                PermissionResolution::Ask
            } else {
                resolution
            };
            // Compose the tool-owned permission result with the policy
            // resolution.  A tool DENY is always final, including over an
            // explicit allow rule, auto, bypass, or a hook-approved path.
            // Protected tool ASKs (MCP ceilings / requiresUI and Workflow's
            // nested Read check) also survive matched allows and bypass.  The
            // ordinary Bash sandbox ASK retains its historical rule-source and
            // bypass carve-outs below.
            let bypass_mode = orch
                .permission_mode()
                .is_some_and(|m| m == "bypassPermissions");
            let mut tool_ask_reason: Option<permission::PermissionDecisionReason> = None;
            let resolution = match (&resolution, &tool_permission_result) {
                (
                    _,
                    permission::PermissionResult::Deny {
                        reason,
                        explanation,
                        ..
                    },
                ) => tool_permission_deny_resolution(name, reason, explanation.as_deref()),
                (
                    PermissionResolution::Allow {
                        rule_source,
                        classifier_approved,
                    },
                    permission::PermissionResult::Ask { reason, .. },
                ) if tool_ask_is_protected
                    || (rule_source.is_none()
                        && (!bypass_mode || requires_user_interaction)
                        && !(*classifier_approved
                            && name == "Monitor"
                            && effective_input.get("ws").is_some()
                            && matches!(
                                reason,
                                permission::PermissionDecisionReason::Other { .. }
                            ))) =>
                {
                    let (rt, rtext) = tool_ask_reason_context(reason);
                    tool_ask_reason = Some(reason.clone());
                    PermissionResolution::AskWithContext {
                        decision_reason_type: rt,
                        decision_reason: rtext,
                    }
                }
                (
                    PermissionResolution::Ask | PermissionResolution::AskWithContext { .. },
                    permission::PermissionResult::Ask { reason, .. },
                ) if tool_ask_is_protected => {
                    let (rt, rtext) = tool_ask_reason_context(reason);
                    tool_ask_reason = Some(reason.clone());
                    match &resolution {
                        PermissionResolution::AskWithContext { .. } => resolution,
                        _ => PermissionResolution::AskWithContext {
                            decision_reason_type: rt,
                            decision_reason: rtext,
                        },
                    }
                }
                // For ordinary Bash sandbox asks, only the old no-rule path
                // reaches the tool-owned refinement; an explicit rule or
                // bypass remains authoritative as before.
                _ => resolution,
            };
            // Tool-owned interaction must remain a per-call human decision.
            // Compute this AFTER the tool's own check has had a chance to
            // escalate an otherwise-permitted call to Ask. Merely being an
            // interactive tool must not create a generic permission prompt
            // (AskUserQuestion owns its business UI).
            let suppress_always_allow_rule = (requires_user_interaction
                || restricted_protected_mutation)
                && matches!(
                    resolution,
                    PermissionResolution::Ask | PermissionResolution::AskWithContext { .. }
                );
            let ask_reason_context = match &resolution {
                PermissionResolution::AskWithContext {
                    decision_reason_type,
                    decision_reason,
                } => (decision_reason_type.clone(), decision_reason.clone()),
                _ => (None, None),
            };
            match resolution {
                PermissionResolution::Allow { rule_source, .. } => {
                    decision_otel_source = rule_decision_otel_source(rule_source.as_deref(), true);
                    PermissionDecision::Allow
                }
                PermissionResolution::Deny {
                    reason,
                    source,
                    rule_source,
                    decision_reason_type,
                    decision_reason,
                    behavior_ask,
                    content_blocks,
                } => {
                    decision_otel_source = rule_decision_otel_source(rule_source.as_deref(), false);
                    // Classify the denial for the stream-json `tool_result_meta`
                    // while the structured provenance is still in scope — the
                    // `PermissionDecision::Deny` this arm returns keeps only `reason`.
                    denial_kind = tool_denial_kind(
                        behavior_ask,
                        decision_reason_type.as_deref(),
                        decision_reason.as_deref(),
                    );
                    // `ask`-behavior rejection contentBlocks (`toolExecution.ts:1040-1043`):
                    // claude-code appends `permissionDecision.contentBlocks` to the deny
                    // user message at top level ONLY when `behavior === 'ask'`. Carry them
                    // to the deny arm via the outer local. DORMANT in the external build —
                    // no gate produces an `ask`+contentBlocks rejection, so this stays empty
                    // and the deny message is byte-identical to today.
                    if behavior_ask {
                        reject_content_blocks = content_blocks;
                    }
                    // HOOK.3 issue 3 — the PermissionDenied hook (claude-code
                    // `executePermissionDeniedHooks`, fired from
                    // `toolExecution.ts:1075`) fires ONLY on an auto-mode CLASSIFIER
                    // deny (`decisionReason.type === 'classifier'`), NOT on a
                    // rule/mode/plan deny. LingXi now wires a deterministic
                    // auto-mode classifier, so classifier-source denies can
                    // reach this path in normal builds.
                    if matches!(source, PermissionDecisionSource::Classifier) {
                        // NOTE: the OTEL label stays "config" — `eQ_` groups
                        // `classifier` with `mode`/`safetyCheck`/… in the
                        // "config" arm. (`fI_` does have a "classifier" arm, but
                        // it needs a `logDecision({source:{type:"classifier"}})`
                        // and 2.1.220 has no such call site — every
                        // `source:{type:…}` there is user/user_reject/
                        // user_abort/hook.)
                        let denied_event = HookEvent::PermissionDenied {
                            tool_name: name.clone(),
                            tool_input: effective_input.clone(),
                            tool_use_id: tool_use_id.clone(),
                            reason: reason.clone(),
                        };
                        let denied_agg = orch.hooks.execute(denied_event, hook_ctx.clone()).await;
                        // `{retry: true}` reply (`toolExecution.ts:1080-1091`): a
                        // PermissionDenied hook can signal the auto-mode classifier
                        // deny is now approved. We honour it when classifier
                        // permissions are enabled, or when the runtime config bit
                        // forces the transcript-classifier path in tests.
                        let classifier_feature_on =
                            permission::classifier::is_classifier_permissions_enabled()
                                || orch.config.transcript_classifier_enabled;
                        if classifier_feature_on && denied_agg.retry {
                            deny_hook_says_retry = true;
                        }
                    }
                    // GATE-SYSMSG-01: emit the `permission_denied` system message on
                    // the stdio outbound. The MAIN-conversation deny path resolves
                    // via `resolve_detailed` (source-first, for the source-gated
                    // hooks), NOT `check_with_context`, so the gate's own
                    // `decide_outcome_with_context` emission (subagent dispatch) is
                    // never reached here — emit through the outer gate, which
                    // forwards to the stdio transport. No-op on non-stdio transports.
                    let sysmsg_ctx = platform_api::permission_gate::PermissionCheckContext {
                        tool_use_id: Some(tool_use_id.to_string()),
                        ..Default::default()
                    };
                    orch.perms
                        .on_permission_denied(
                            name,
                            &sysmsg_ctx,
                            decision_reason_type.as_deref(),
                            decision_reason.as_deref(),
                            &reason,
                        )
                        .await;
                    PermissionDecision::Deny { reason }
                }
                PermissionResolution::Ask | PermissionResolution::AskWithContext { .. } => {
                    // HOOK.3 issue 2 — the gate is ABOUT TO ASK. Fire the
                    // PermissionRequest hook FIRST (claude-code
                    // `runPermissionRequestHooksForHeadlessAgent`, fired on the ask
                    // path before the fallback resolution). A hook 'allow' RESCUES
                    // the call — resolved via `check_after_hook_allow` so explicit
                    // deny rules still bind (a PermissionRequest 'allow', like a
                    // PreToolUse 'allow', skips only the PROMPT), applying any
                    // `updatedInput`; a hook 'deny' denies; otherwise we delegate to
                    // the inner transport (interactive prompt, or a headless
                    // auto-deny). Strict no-op when no PermissionRequest hook is
                    // registered → the inner transport resolves exactly as before.
                    let req_event = HookEvent::PermissionRequest {
                        tool_name: name.clone(),
                        tool_input: effective_input.clone(),
                        reason: format!("Tool {name} requires permission"),
                    };
                    let req_agg = orch.hooks.execute(req_event, hook_ctx.clone()).await;
                    match req_agg.decision {
                        Some(HookDecision::Approve | HookDecision::Allow)
                            if !tool_ask_blocks_hook_rescue =>
                        {
                            // PermissionRequest allow responses may carry raw
                            // `updatedPermissions` entries. Apply and persist
                            // them before resolving the rescued call so the
                            // same live gate observes the host's updates.
                            if !req_agg.permission_updates.is_empty() {
                                orch.perms
                                    .apply_permission_updates(&req_agg.permission_updates);
                                orch.perms
                                    .persist_permission_updates(&req_agg.permission_updates)
                                    .await;
                            }
                            // (cc 2.1.218 `Fxy`) The headless PermissionRequest
                            // rescue re-checks the rules (`epr(_pt(...))`, where an
                            // ask rule becomes a HARD DENY — no prompt is available
                            // on this surface) ONLY when the hook supplied
                            // `updatedInput` OR the tool `requiresUserInteraction`:
                            //   if(a.updatedInput||e.requiresUserInteraction?.()){…}
                            //   return {behavior:"allow", updatedInput:l, …}
                            // MCP/org ceilings and explicit requiresUserInteraction
                            // asks are excluded from this rescue arm: their
                            // per-call contracts cannot be overridden by a
                            // PermissionRequest hook allow. A Workflow nested Read
                            // is intentionally not in that set; it is an ordinary
                            // Read ask and may be approved by the configured handler.
                            // With NEITHER trigger the allow STANDS UNCHECKED — we
                            // must NOT re-run the rule/mode verdict, or the rescue is
                            // defeated in its primary use case (an ordinary ask rule
                            // the hook meant to pre-approve would re-prompt / hard
                            // deny). Both the reachable ask rule and a deny rule were
                            // already resolved before this Ask branch, so honouring
                            // the unchanged input directly is safe.
                            let rewritten = req_agg.modified_input.is_some();
                            if let Some(updated) = req_agg.modified_input {
                                effective_input = updated;
                            }
                            let hook_decision = if rewritten || requires_user_interaction {
                                // Rewritten input, or a tool that requires user
                                // interaction: re-check via `epr(_pt(...))` — an ask
                                // becomes a hard deny carrying the ask's `c.message`
                                // (identical for both triggers), so the rewritten
                                // resolver serves both.
                                orch.perms
                                    .check_after_hook_allow_rewritten(name, &effective_input)
                                    .await
                            } else {
                                // Standing allow (`Fxy`'s no-recheck arm): honour the
                                // allow directly, keeping the auto-mode non-deny
                                // bookkeeping every allow arm records — mode-less, no
                                // rule re-check, no backstop.
                                orch.perms.honour_hook_allow(name, &effective_input).await
                            };
                            // `handleHookAllow` logs `source:{type:"hook"}`, but the
                            // `updatedInput` re-check that DENIES logs
                            // `{decision:"reject",source:"config"}` instead — the hook
                            // owns the label only while its allow stands.
                            decision_otel_source =
                                if matches!(hook_decision, PermissionDecision::Allow) {
                                    "hook"
                                } else {
                                    "config"
                                };
                            hook_decision
                        }
                        Some(HookDecision::Block) => {
                            decision_otel_source = "hook";
                            if req_agg.interrupt {
                                if let Some(cancel) = cancel.as_ref() {
                                    cancel.cancel();
                                }
                            }
                            PermissionDecision::Deny {
                                reason: req_agg
                                    .reason
                                    .unwrap_or_else(|| "permission denied by hook".into()),
                            }
                        }
                        _ => {
                            if plan_mode && !orch.config.interactive_permissions {
                                PermissionDecision::Deny {
                                    reason: permission::headless_gate::headless_deny_message(name),
                                }
                            } else {
                                // Delegate to the inner prompt transport, carrying the
                                // REAL tool_use_id (so a stdio `can_use_tool` request is
                                // byte-faithful) and applying the host's `updatedInput`
                                // rewrite to the input the tool actually runs with.
                                let ctx = platform_api::permission_gate::PermissionCheckContext {
                                    tool_use_id: Some(tool_use_id.to_string()),
                                    requires_user_interaction,
                                    suppress_always_allow_rule,
                                    // HOOK-ASKFLOOR-03: a PreToolUse hook `ask` sets the
                                    // floor so the Auto classifier can't re-allow past it
                                    // (policy_gate Ask arm gates the classifier on this).
                                    hook_ask_floor: hook_ask,
                                    is_non_interactive_session: !orch
                                        .config
                                        .interactive_permissions,
                                    decision_reason_type: ask_reason_context.0.clone(),
                                    decision_reason: ask_reason_context.1.clone(),
                                    ..Default::default()
                                };
                                // BASH-10: an ask that the TOOL raised must NOT be
                                // re-derived from the rule/mode layer — `PolicyPermissionGate`
                                // would recompute the very allow the tool escalated
                                // and silently defeat it. `ask_via_transport` hands
                                // the call straight to the prompt transport (the
                                // same `self.inner.check_with_context` the gate's own
                                // Ask arm reaches after it has decided to prompt).
                                // Unreachable unless a tool returned `Ask` above, so
                                // every policy-originated ask keeps the old call.
                                let outcome = if tool_ask_reason.is_some() {
                                    orch.perms
                                        .ask_via_transport(name, &effective_input, &ctx)
                                        .await
                                } else {
                                    orch.perms
                                        .check_with_context(name, &effective_input, &ctx)
                                        .await
                                };
                                match outcome {
                                    platform_api::permission_gate::PermissionOutcome::Allow {
                                        updated_input,
                                        // `permission_updates` (the host's
                                        // `updatedPermissions`) are applied + persisted
                                        // inside the stdio gate itself, which holds the
                                        // settings paths.
                                        permission_updates: _,
                                        decision_classification,
                                    } => {
                                        // The host's explicit classification wins when
                                        // valid; absent/unknown values were normalized to
                                        // `None` by the transport and use Claude's
                                        // temporary-allow fallback.
                                        decision_otel_source = decision_classification.map_or(
                                        "user_temporary",
                                        platform_api::permission_gate::ToolDecisionClassification::as_str,
                                    );
                                        if let Some(u) = updated_input {
                                            effective_input = u;
                                        }
                                        PermissionDecision::Allow
                                    }
                                    platform_api::permission_gate::PermissionOutcome::AllowAuto {
                                        updated_input,
                                    } => {
                                        if let Some(u) = updated_input {
                                            effective_input = u;
                                        }
                                        if let Err(error) =
                                            orch.perms.set_permission_mode("auto").await
                                        {
                                            tracing::warn!(
                                                %error,
                                                "permission prompt approved Auto mode but mode switch failed"
                                            );
                                        } else {
                                            decision_otel_source = "user_temporary";
                                        }
                                        PermissionDecision::Allow
                                    }
                                    platform_api::permission_gate::PermissionOutcome::Deny { reason } => {
                                        // An ABORTED prompt is a distinct label: claude-code
                                        // denies with `decisionReason: iYt` ("tool permission
                                        // request aborted") when `signal.aborted`, and `eQ_`
                                        // maps that `other` reason to "user_abort" (the
                                        // interactive twin is the prompt's `case "cancelled"`
                                        // → `source:{type:"user_abort"}`). The gate folds both
                                        // into `Deny`, so the turn's cancel token — the same
                                        // signal the stdio gate raced to produce this deny —
                                        // is what separates them.
                                        // Denial provenance: this arm keeps the
                                        // `permission-rule` fallthrough, and that is
                                        // CORRECT for the transport that can observe it.
                                        // claude-code's `JMn` (binary offset 246277535)
                                        // wraps a stdio `can_use_tool` result as
                                        // `{...hostResult, decisionReason:{type:
                                        // "permissionPromptTool", …}}`, PRESERVING the
                                        // host's `behavior`. So a host deny reaches the
                                        // kind classifier as `behavior === "deny"` with a
                                        // `permissionPromptTool` reason — neither the
                                        // `ask` branch nor the classifier branch — and
                                        // falls through to `permission-rule`.
                                        //
                                        // `user-rejected` means `behavior === "ask"`,
                                        // which is what the INTERACTIVE CLI prompt
                                        // returns (hence `userFeedback: behavior==="ask"
                                        // ? … : void 0` at offset 235412899). Real
                                        // transcripts from an interactive session are
                                        // therefore full of `user-rejected` — but
                                        // `tool_result_meta` is emitted only by the
                                        // stream-json transport, whose permission
                                        // transport is the stdio gate. Do NOT stamp
                                        // `user-rejected` here: this arm also covers
                                        // transport failure and a dropped response
                                        // channel, which claude-code maps to
                                        // `{type:"other"}` ⇒ `permission-rule` too.
                                        decision_otel_source =
                                            if cancel.as_ref().is_some_and(|t| t.is_cancelled()) {
                                                "user_abort"
                                            } else {
                                                "user_reject"
                                            };
                                        PermissionDecision::Deny { reason }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        };
        // The plan / hook / forced branches above intentionally use their
        // existing gate entrypoints.  Apply the same tool-owned result after
        // those branches so a tool-local DENY still binds and a protected ASK
        // cannot be swallowed by an Allow/Auto/Bypass response.  A headless
        // owner fails closed instead of handing a protected ask to a transport
        // that might have no way to represent the prompt.
        let decision = if !non_normal_permission_path {
            decision
        } else {
            match &tool_permission_result {
                permission::PermissionResult::Deny { explanation, .. } => match decision {
                    PermissionDecision::Deny { .. } => decision,
                    PermissionDecision::Allow => PermissionDecision::Deny {
                        reason: explanation.as_deref().map_or_else(
                            || format!("Permission to use {name} has been denied."),
                            str::to_string,
                        ),
                    },
                },
                permission::PermissionResult::Ask { reason, .. } if tool_ask_is_protected => {
                    if matches!(decision, PermissionDecision::Deny { .. }) {
                        decision
                    } else if !orch.config.interactive_permissions {
                        PermissionDecision::Deny {
                            reason: format!("Permission to use {name} has been denied."),
                        }
                    } else {
                        let (decision_reason_type, decision_reason) =
                            tool_ask_reason_context(reason);
                        let ask_ctx = platform_api::permission_gate::PermissionCheckContext {
                            tool_use_id: Some(tool_use_id.to_string()),
                            requires_user_interaction,
                            suppress_always_allow_rule: requires_user_interaction
                                || restricted_protected_mutation,
                            decision_reason_type,
                            decision_reason,
                            is_non_interactive_session: !orch.config.interactive_permissions,
                            ..Default::default()
                        };
                        match orch
                            .perms
                            .ask_via_transport(name, &effective_input, &ask_ctx)
                            .await
                        {
                            platform_api::permission_gate::PermissionOutcome::Allow {
                                updated_input,
                                ..
                            } => {
                                if let Some(updated) = updated_input {
                                    effective_input = updated;
                                }
                                PermissionDecision::Allow
                            }
                            platform_api::permission_gate::PermissionOutcome::AllowAuto {
                                updated_input,
                            } => {
                                if let Some(updated) = updated_input {
                                    effective_input = updated;
                                }
                                PermissionDecision::Allow
                            }
                            platform_api::permission_gate::PermissionOutcome::Deny { reason } => {
                                PermissionDecision::Deny { reason }
                            }
                        }
                    }
                }
                _ => decision,
            }
        };
        // OTEL: record the RESOLVED tool-permission decision — the CC
        // `XNr()?.add(1, await ICs(...))` counter (Edit/Write/NotebookEdit
        // only, gated inside) + the paired `tool_decision` `claude_code.events`
        // record (every tool). The path argument is the `getPath` twin: the
        // edit tools' `file_path` / NotebookEdit's `notebook_path` from the
        // input the decision was made on. Byte-noop when OTEL is off.
        telemetry::otel::record_tool_permission_decision(
            name,
            tool_use_id.as_str(),
            effective_input
                .get("file_path")
                .or_else(|| effective_input.get("notebook_path"))
                .and_then(serde_json::Value::as_str),
            if matches!(decision, PermissionDecision::Allow) {
                "accept"
            } else {
                "reject"
            },
            decision_otel_source,
            tool_handle.is_mcp(),
            Some(&effective_input),
        );
        match decision {
            PermissionDecision::Allow => {}
            PermissionDecision::Deny { reason } => {
                // Push the deny error `tool_result`. The PermissionDenied hook,
                // when applicable, already fired on the classifier-deny branch
                // above — claude-code fires it only for auto-mode classifier
                // denials, not for the rule/mode/plan denials that also reach here.
                // claude-code sends the permission deny message VERBATIM as the
                // tool_result content (e.g. "Permission to use Bash has been
                // denied." — built by the gate via `deny_reason_string`, or the
                // tool's explicit `explanation`), NOT wrapped in a
                // "Permission denied: " prefix.
                let result_block = ContentBlock::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: reason.clone(),
                    is_error: true,
                    provider_tool_use_id: provider_id.clone(),
                    content_blocks: None,
                };
                // Denial provenance (claude-code `toolDenialKind`), classified by
                // `tool_denial_kind` in the SOURCED resolution arm and carried
                // here via `denial_kind`. Transports that cannot carry it (all
                // but stream-json) inherit the trait default and ignore it.
                //
                // NOT yet distinguished: the stdio `can_use_tool` prompt-transport
                // deny (the `PermissionOutcome::Deny` arm below, which the port
                // labels `user_reject` / `user_abort` for OTEL) carries no
                // `behavior_ask`, so a host-side rejection currently reports the
                // `permission-rule` fallthrough instead of `user-rejected`.
                // Same provenance on BOTH surfaces: the stream-json frame gets
                // `tool_result_meta` via the emit below, and the persisted
                // transcript line gets the message-level `toolDenialKind` via
                // this record, consumed when the tool_result user line is
                // written.
                orch.record_tool_denial_kind(tool_use_id, denial_kind).await;
                // OR-1: the AUTHORITATIVE `permission_denials` record for the
                // stream-json `result` frame. Recorded here — the same funnel
                // `record_tool_denial_kind` uses — rather than aggregated from
                // the `permission_denied` system event, which claude-code's own
                // schema doc calls "best-effort advisory" and which does not
                // cover PreToolUse hook denies, deny-rule overrides of a hook
                // allow/ask, or file-tool calls refused by a path-scoped deny
                // rule. `effective_input` is the input the decision was made on,
                // matching the oracle's `tool_input`.
                orch.record_permission_denial(name, tool_use_id, &effective_input)
                    .await;
                // O1: claude's permission-deny arm (2.1.220 BIN off 235400200)
                // stamps `` toolUseResult: `Error: ${denyMessage}` `` — the
                // deny message with an `Error: ` prefix, while the model
                // content carries it wrapped in `<tool_use_error>`. LingXi
                // sends the deny message verbatim as the model content, so only
                // the persisted line gets the prefix.
                orch.record_tool_use_result(
                    tool_use_id,
                    serde_json::Value::String(format!("Error: {reason}")),
                )
                .await;
                orch.emit_tool_result_frame(
                    tool_use_id,
                    name,
                    &reason,
                    &serde_json::json!({ "error": reason }),
                    Some(denial_kind),
                )
                .await;
                results.push(result_block);
                // `ask`-behavior rejection contentBlocks (`toolExecution.ts:1039-1046`):
                // append the image/non-text blocks at the TOP LEVEL of the deny
                // user message — alongside, NOT inside, the text-only tool_result
                // (which rejects non-text when `is_error` is set). They join
                // `results`, which IS this turn's tool_result user message content,
                // so they land in the same message as the tool_result, exactly like
                // claude-code's `messageContent.push(...rejectContentBlocks)`.
                //
                // imagePasteId residual: claude-code assigns sequential
                // `imagePasteIds` via `getNextImagePasteId` (max prior id + 1, one
                // per image) — a TUI RENDER LABEL on the user message
                // (`messages.ts:801`). LingXi's `ConversationMessage::User` models no
                // `imagePasteIds` field (the same gap as `isMeta`; both are
                // display-only, never sent to the model and never written to JSONL),
                // so there is no home to store the id. The image BLOCKS themselves
                // are carried faithfully; the per-image label is the documented
                // residual. DORMANT: empty on every normal deny, so this loop is a
                // strict no-op and the common deny message is byte-identical.
                for block in reject_content_blocks {
                    results.push(block);
                }
                // HOOK.1: even on a permission DENY, claude-code's pre-hook
                // phase already pushed the PreToolUse `additionalContext`
                // (`toolExecution.ts:846`) before the gate ran — so surface it
                // here, ordered AFTER this path's deny error tool_result. No-op
                // when the hook emitted no context.
                if let Some(msg) = pre_context_message {
                    injected_messages.push((msg, tool_use_id.clone()));
                }
                // PermissionDenied-hook `{retry: true}` (`toolExecution.ts:1092-1099`):
                // after the deny user message, push a SECOND `isMeta` user message
                // with the verbatim approval-to-retry string. DOUBLE-GATED upstream
                // (the `deny_hook_says_retry` flag is set only when BOTH the
                // `TRANSCRIPT_CLASSIFIER` feature is on AND a classifier-source deny
                // ran a `PermissionDenied` hook that returned `{retry: true}`), so it
                // is DORMANT on the normal deny path — `deny_hook_says_retry` is
                // `false` there and this is a strict no-op. Built as a META user
                // message (CC `createUserMessage({…, isMeta:!0})`), so when it does
                // fire it persists with top-level `isMeta:true`.
                if deny_hook_says_retry {
                    let retry_msg = ConversationMessage::user_meta(
                        MessageId::new(),
                        PERMISSION_DENIED_RETRY_MESSAGE.to_string(),
                    );
                    injected_messages.push((retry_msg, tool_use_id.clone()));
                }
                continue;
            }
        }

        // #8 NOTE: the SubagentStart wire-event fire MOVED below — to the
        // post-`tool_handle.call()` site alongside `SubagentStop`. At this
        // pre-call point the spawn has not run yet, so the child's REAL pool
        // `AgentId` does not exist; firing here forced a fresh divergent id.
        // The Agent tool now surfaces the child id on its result
        // `data.agentId` (C1 seam: `SubagentResult` carries the real id back),
        // so BOTH SubagentStart and SubagentStop fire post-call with that one
        // canonical id — matching claude-code's single `agentId`
        // (runAgent.ts:347). The fire-only-on-actual-spawn semantics are
        // preserved: a pre-hook Block / permission denial `continue`s above
        // before `tool_handle.call()`, so no subagent spawns and neither event
        // fires.

        // Progress channel: drained CONCURRENTLY with the tool call. The Agent
        // tool forwards a `{"subagent_activity": "<line>"}` payload per nested
        // subagent tool call; re-emit each as `emit_subagent_activity` so the
        // subagent's work renders under its Task cell. Other tools send nothing,
        // so this is a no-op for them. The consumer exits when the tool drops
        // `progress_tx` (call returns).
        let (progress_tx, mut progress_rx) =
            tokio::sync::mpsc::channel::<tool_api::progress::ToolProgress>(64);
        let progress_output = orch.output.clone();
        // The spawning Task tool_use_id — stamped as `parent_tool_use_id` on any
        // forwarded subagent assistant frame (`--forward-subagent-text`). Uses
        // the same `ToolUseId::as_str` form the stream-json tool_use block id
        // carries, so a forwarded child frame correlates to its parent Task call.
        let progress_parent_tool_use_id = tool_use_id.as_str().to_string();
        // A JoinSet owns every auxiliary event producer. Dropping this dispatch
        // scope (runtime shutdown, panic, or future host cancellation changes)
        // aborts the children instead of detaching them and allowing stale
        // heartbeat/progress events to escape into a later turn.
        let mut event_tasks = tokio::task::JoinSet::new();
        let event_tasks_done = tokio_util::sync::CancellationToken::new();
        let progress_done = event_tasks_done.clone();
        event_tasks.spawn(async move {
            loop {
                tokio::select! {
                    () = progress_done.cancelled() => {
                        // A tool is allowed to retain a progress sender in work it
                        // spawned. Close the receiver so those detached producers
                        // cannot keep this turn alive, then drain progress that was
                        // already accepted before the tool reached its terminal state.
                        progress_rx.close();
                        while let Some(progress) = progress_rx.recv().await {
                            forward_tool_progress(
                                progress_output.as_ref(),
                                &progress_parent_tool_use_id,
                                progress,
                            ).await;
                        }
                        break;
                    }
                    progress = progress_rx.recv() => {
                        let Some(progress) = progress else { break; };
                        forward_tool_progress(
                            progress_output.as_ref(),
                            &progress_parent_tool_use_id,
                            progress,
                        ).await;
                    }
                }
            }
        });
        // Periodic tool heartbeat for long-running calls: transports that care
        // can surface "still running" state between ToolCall and ToolResult,
        // while sinks that ignore it keep the default no-op behavior.
        let heartbeat_output = orch.output.clone();
        let heartbeat_id = tool_use_id.clone();
        let heartbeat_tool = name.to_string();
        let _heartbeat_cancel = cancel.clone();
        let heartbeat_done = event_tasks_done.clone();
        let heartbeat_started = std::time::Instant::now();
        event_tasks.spawn(async move {
            // (review #10) `interval` fires its FIRST tick immediately, which
            // would emit a spurious `elapsed_ms≈0` heartbeat on EVERY tool call
            // (even instant ones), defeating the "long-running" intent. Start the
            // first tick one period out so heartbeats only fire for tools that
            // actually run >= 1s.
            let period = std::time::Duration::from_secs(1);
            let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    () = heartbeat_done.cancelled() => break,
                    _ = ticker.tick() => {
                        #[allow(clippy::cast_possible_truncation)]
                        let elapsed_ms = heartbeat_started.elapsed().as_millis() as u64;
                        #[cfg(debug_assertions)]
                        eprintln!(
                            "[turn-diagnostic] tool heartbeat id={} name={} elapsed_ms={} cancelled={}",
                            heartbeat_id.as_str(),
                            heartbeat_tool,
                            elapsed_ms,
                            _heartbeat_cancel
                                .as_ref()
                                .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
                        );
                        heartbeat_output
                            .emit_tool_heartbeat(&heartbeat_id, &heartbeat_tool, elapsed_ms)
                            .await;
                    }
                }
            }
        });

        // Time the tool dispatch ONLY (excludes the permission prompt above and
        // the Post hooks below) — surfaced to PostToolUse/Failure hooks as
        // `duration_ms` (claude-code 2.1.195).
        let tool_started = std::time::Instant::now();
        #[cfg(debug_assertions)]
        eprintln!(
            "[turn-diagnostic] tool call started id={} name={} cancel_present={} cancelled={}",
            tool_use_id.as_str(),
            name,
            ctx.cancel.is_some(),
            ctx.cancel
                .as_ref()
                .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
        );
        // `InterruptBehavior::Cancel` is a dispatcher contract, not merely a
        // suggestion that every tool implementation must remember to honor.
        // Some network-backed tools (notably client-side WebSearch) await an
        // HTTP future that does not observe `ToolUseContext.cancel`. Race that
        // future at the boundary so a user cancellation always drops it and
        // yields the same `ToolError::Aborted` path as a cooperative tool.
        // `Block` tools intentionally keep their existing wait-to-completion
        // behavior.
        let interrupt_behavior = tool_handle.interrupt_behavior(&effective_input);
        let dispatch_cancel = ctx.cancel.clone();
        let tool_outcome = {
            // Keep the call future in this inner scope. When cancellation wins,
            // leaving the scope drops the non-cooperative future (and its
            // progress sender) before we await the progress consumer below.
            let tool_call = tool_handle.call(effective_input.clone(), ctx, progress_tx);
            tokio::pin!(tool_call);
            match (interrupt_behavior, dispatch_cancel) {
                (tool_api::tool_trait::InterruptBehavior::Cancel, Some(cancel)) => {
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => Err(tool_api::ToolError::Aborted),
                        outcome = &mut tool_call => outcome,
                    }
                }
                _ => tool_call.await,
            }
        };
        #[cfg(debug_assertions)]
        eprintln!(
            "[turn-diagnostic] tool call returned id={} name={} elapsed_ms={} outcome={}",
            tool_use_id.as_str(),
            name,
            tool_started.elapsed().as_millis(),
            if tool_outcome.is_ok() { "ok" } else { "error" }
        );
        event_tasks_done.cancel();
        // Drain buffered progress and stop the heartbeat before publishing the
        // terminal tool result. The JoinSet aborts both tasks automatically if
        // this dispatch future is dropped by a parent turn/runtime shutdown.
        while event_tasks.join_next().await.is_some() {}
        #[allow(clippy::cast_possible_truncation)]
        let tool_duration_ms = tool_started.elapsed().as_millis() as u64;

        let (content, is_error, emit_payload, is_abort) = match tool_outcome {
            Ok(result) => {
                let turn_end = tool_result_turn_end(
                    tool_handle.result_ends_turn(&result),
                    result.is_error,
                    result.mcp_meta.as_ref(),
                );
                let text = result
                    .model_content
                    .clone()
                    .unwrap_or_else(|| tool_result_to_model_text(&result.data));
                // SKILLEXEC.3 (Part A): stash any tool-injected conversation
                // messages so the caller can append them after this batch's
                // tool_result user message. Non-empty only for the Skill tool
                // (the expanded skill prompt); empty for every other tool, so
                // the locked turn-loop fixtures stay byte-identical. Each is
                // paired with THIS tool's `tool_use_id` (TS
                // `tagMessagesWithToolUseID` stamps the Skill tool's own block
                // id as `sourceToolUseID`) for the caller's in-memory
                // `injected_message_sources` side-table.
                injected_messages.extend(
                    result
                        .new_messages
                        .into_iter()
                        .map(|m| (m, tool_use_id.clone())),
                );
                // SKILLEXEC.3 (model scope): stash any one-shot `context_modifier`
                // for the caller to fold POST-BATCH. NOT applied to the per-tool
                // `ctx` here (which is discarded at loop end) and NOT applied
                // per-tool — folding after the whole batch gives the concurrent
                // streaming path a single, race-free application point. `None`
                // for every existing tool + skills WITHOUT a `model:` frontmatter,
                // so this is a strict no-op there (byte-identical).
                if let Some(modifier) = result.context_modifier {
                    context_modifiers.push(modifier);
                }
                // O1: claude stamps the tool's RAW STRUCTURED result on the
                // persisted `tool_result` user line as `toolUseResult`
                // (2.1.220 BIN off 235420375: `toolUseResult: gt` where
                // `gt = se.data`) — NOT the model-facing string. Same value the
                // stream-json frame carries; recorded against the tool_use id
                // and consumed when the user line is persisted.
                orch.record_tool_use_result(tool_use_id, result.data.clone())
                    .await;
                // O1: an MCP server's `_meta`/`structuredContent` passthrough
                // rides as the TOP-LEVEL `mcpMeta` sibling. `Uks(agentId, meta)`
                // (BIN off 232969604) returns it verbatim on the main chain,
                // which is the only chain this orchestrator serves.
                if let Some(meta) = result.mcp_meta.clone() {
                    orch.record_tool_use_mcp_meta(tool_use_id, meta).await;
                }
                if let Some(turn_end) = turn_end {
                    orch.record_pending_tool_result_turn_end(tool_use_id, turn_end)
                        .await;
                }
                // `is_error` rides on the result (set by MCP tools from the
                // server's `isError`; `false` for every native success). A native
                // FAILURE is an `Err` handled below — this Ok arm only flags an
                // MCP logical-error RESULT.
                (text, result.is_error, result.data, false)
            }
            Err(err) => {
                // Bare error string — no <tool_use_error> wrapper.
                // claude-code/src/services/tools/toolExecution.ts:1691 does:
                //   const content = formatError(error)   // bare, from utils/toolErrors.ts
                // and feeds it raw into tool_result.content (line 1721).
                // Only pre-execution paths (unknown-tool, schema validation) wrap.
                //
                // The model-facing content uses `model_facing_message()` — the
                // BARE inner message (claude's `error.message`) — NOT the
                // `Display` form, which would leak a LingXi-internal variant
                // prefix (`invalid input: ` / `internal: `) into the wire bytes.
                //
                // O4-A: claude-code's `oQ_` catch (2.1.220 @235424972) also
                // stamps `toolDenialKind: YDd(err, signal)` on this very frame.
                // `YDd` (@235394375) returns a kind ONLY for an AbortError
                // (`tl`), an interrupted `ShellError` (`hW`), or an
                // abort-signalled `$7e`; LingXi's 1:1 analog of `tl` is
                // `ToolError::Aborted`. The `hW.interrupted` branch has NO port
                // analog today — `tools/shell/src/bash.rs` returns
                // `Ok(build_interrupted_result())` for a killed shell rather
                // than an `Err`, so it never reaches here; that branch is
                // deliberately UNMODELED. `YDd`'s `background` abort reason
                // (which maps to `"cancelled"`) likewise has no LingXi
                // equivalent, so every LingXi abort takes the `interrupted`
                // branch.
                let is_abort = matches!(err, tool_api::ToolError::Aborted);
                let bare = err.model_facing_message();
                let text = format!("Error: {bare}");
                // O1: on the ERROR arm claude stores the plain STRING
                // `` `Error: ${ae}` `` in `toolUseResult` (2.1.220 BIN off
                // 235424595), NOT a structured object. The `{"error": …}`
                // object below is the port's stream-json SDK frame — a
                // different wire that legitimately differs here.
                orch.record_tool_use_result(tool_use_id, serde_json::Value::String(text.clone()))
                    .await;
                (text, true, serde_json::json!({ "error": bare }), is_abort)
            }
        };

        if is_abort {
            // Denial provenance for an aborted tool. `record_tool_denial_kind`
            // feeds the persisted `tool_result` user line's `toolDenialKind`
            // (via `take_tool_denial_kind`); `emit_tool_result_denied` carries
            // the same kind on the stream-json frame. Same shape as the
            // hardcoded `"cancelled"` on the pre-cancel guard above.
            orch.record_tool_denial_kind(tool_use_id, "interrupted")
                .await;
            orch.emit_tool_result_frame(
                tool_use_id,
                name,
                &content,
                &emit_payload,
                Some("interrupted"),
            )
            .await;
        } else {
            orch.emit_tool_result_frame(tool_use_id, name, &content, &emit_payload, None)
                .await;
        }

        // (code-change stats for /usage — claude-code `Bhn(added, removed)`)
        // Only file-edit tools (Edit/Write/MultiEdit) put a `structuredPatch`
        // in their result data; sum its +/- lines into the session counters.
        accumulate_code_change(&emit_payload, orch.model_runtime.cost_tracker.as_ref()).await;

        // NOTE: the read-file-state registry (`context.readFileState`, backing
        // `/files`, conditional-rule matching, and the relevant-memory dedup) is
        // populated by the file tools themselves via `readFileState.set`
        // (Read/Edit/Write/MultiEdit/NotebookEdit) over the shared `Arc` the
        // composition root hands their `BuiltinToolContext` — 1:1 with
        // claude-code's single per-session map. The orchestrator no longer keeps
        // a separate insertion-ordered `Vec`, so there is nothing to record here.

        // M5-06 Task 14 + hooks B-tool-failure: the post-dispatch hook chain.
        // Byte-faithful to claude-code's split: a SUCCESSFUL tool result fires
        // `PostToolUse` (`executePostToolUseHooks`), a FAILED one fires
        // `PostToolUseFailure` (`executePostToolUseFailureHooks`,
        // `utils/hooks.ts:3492`) — never both. The `is_error` flag here is the
        // same `is_error` that lands on the `ToolResult` block (TS keys off the
        // tool result's `is_error`). Best-effort for BOTH arms — a Post hook's
        // `system_messages` are appended to the result text, but a hook failure
        // does NOT mutate `content` or `is_error`.
        //
        // The `PostToolUseFailure` variant carries `tool_name` / `tool_use_id`
        // (matching the prior `PreToolUse`) + the dispatched `tool_input` (the
        // same `effective_input` the `PostToolUse` success arm threads) + the
        // stringified `error`. We pass the raw error string the tool returned
        // (the `{"error": …}` envelope value = `format!("{err}")`), NOT the
        // `"Error: "`-prefixed model-facing `content`, mirroring the TS
        // `PostToolUseFailure` input's `error`.
        let post_event = if is_error {
            let error = emit_payload
                .get("error")
                .and_then(serde_json::Value::as_str)
                .map_or_else(|| content.clone(), ToString::to_string);
            HookEvent::PostToolUseFailure {
                tool_name: name.clone(),
                tool_input: effective_input.clone(),
                error,
                tool_use_id: tool_use_id.clone(),
                duration_ms: Some(tool_duration_ms),
            }
        } else {
            HookEvent::PostToolUse {
                tool_name: name.clone(),
                tool_input: effective_input.clone(),
                tool_output: emit_payload.clone(),
                tool_use_id: tool_use_id.clone(),
                duration_ms: Some(tool_duration_ms),
            }
        };
        // O2: the identity the post-hook attachment and model-facing records carry.
        // both off the event it actually fired, so the failure path renders as
        // `PostToolUseFailure:{tool}` (BIN off 234728254 / 234728470), not
        // `PostToolUse:{tool}`.
        let post_hook_event = if is_error {
            "PostToolUseFailure"
        } else {
            "PostToolUse"
        };
        let post_hook_name = format!("{post_hook_event}:{name}");
        let post_started = std::time::Instant::now();
        tracing::info!(
            event = orch_events::HOOK_POST_STARTED,
            tool_name = %name,
        );
        telemetry::otel::emit_hook_lifecycle("post", "started", name, None);
        let post_agg = orch.hooks.execute(post_event, hook_ctx.clone()).await;
        // hook duration bounded by tokio timeout — u128 ms cannot exceed u64::MAX
        #[allow(clippy::cast_possible_truncation)]
        let post_dur_ms = post_started.elapsed().as_millis() as u64;
        let mut post_additional_contexts = post_agg.additional_contexts.clone();
        if let Some(notice) = memdir_index_notice_for_tool(orch, name, &effective_input, is_error) {
            if let Some(bus) = orch.model_runtime.analytics_bus.as_ref() {
                let mut metadata = telemetry::LogEventMetadata::new();
                metadata.insert(
                    "over_cap".into(),
                    telemetry::AnalyticsValue::Bool(notice.over_cap),
                );
                bus.log_event(memory::TENGU_MEMDIR_ENTRYPOINT_NEAR_CAP, metadata)
                    .await;
            }
            post_additional_contexts.push(notice.text);
        }

        // #40 terminalSequence apply for the post-dispatch aggregate (claude-code
        // `szn` runs per hook result, all event types). Same as the PreToolUse
        // side: validate, warn on rejection, and write accepted bytes through
        // the active terminal bridge.
        apply_terminal_sequence(orch, name, post_agg.terminal_sequence.as_deref()).await;

        // FIX C (hook_stopped_continuation, PostToolUse twin): a PostToolUse
        // hook's `continue:false` (preventContinuation) becomes its OWN meta
        // message — claude yields it AFTER the tool_result (`toolHooks.ts:118-130`)
        // using `stopReason || 'Execution stopped by PostToolUse hook'` and
        // hookName `PostToolUse:{tool}`, then RETURNS (before any additionalContext),
        // so we queue it BEFORE the additionalContext loop below. Tagged with this
        // tool's `tool_use_id`; injected messages are appended after the
        // tool_result by both drivers, matching claude's ordering. `post_agg.reason`
        // carries the parsed `stopReason` (`hook_payload.rs:1113`). Strict no-op
        // when the hook did not request preventContinuation.
        // O2: `hook_blocking_error`. The oracle's PostToolUse consumer
        // (BIN off 234726074) re-emits the runner's bare `{blockingError}`
        // signal as an attachment, positioned AFTER the pass-through run record
        // and BEFORE the `preventContinuation` yield — so this block sits above
        // the stopped-continuation one.
        //
        // The EXECUTOR deliberately publishes nothing on a blocking run (its
        // `build_run_attachment` returns `None` for a `Block` decision, matching
        // BIN off 237805098, where the exit-2 arm yields no `message`); the
        // CALLER owns this record. Do not move it into the executor.
        //
        // Unlike almost every other hook attachment, this one IS model-facing:
        // the normalizer renders it as an `isMeta` user message
        // (BIN off 238107476). Previously `post_agg.decision` was never read
        // here, so a blocking PostToolUse hook produced nothing at all.
        if matches!(
            post_agg.decision,
            Some(hooks::response::HookDecision::Block)
        ) {
            let err = hooks::BlockingError {
                // `e.reason || "Blocked by hook"` (BIN off 237775430). On the
                // plain-text exit-2 arm the executor already parked the fully
                // rendered `[{display}]: {stderr}` string in `reason`.
                blocking_error: post_agg
                    .reason
                    .clone()
                    .unwrap_or_else(|| "Blocked by hook".to_string()),
                // Frozen at the first blocker alongside `reason`; the executor
                // picks `iSe` vs `qq` per arm.
                command: post_agg.block_command.clone().unwrap_or_default(),
            };
            orch.queue_hook_attachment(
                tool_use_id,
                hooks::blocking_error_attachment(
                    &hooks::HookAttachmentIdentity {
                        hook_name: post_hook_name.clone(),
                        hook_event: post_hook_event.to_string(),
                        tool_use_id: tool_use_id.as_str().to_string(),
                    },
                    &err,
                ),
            )
            .await;
            let body = hooks::blocking_error_prose(&post_hook_name, &err);
            injected_messages.push((
                ConversationMessage::user_meta(
                    MessageId::new(),
                    format!("<system-reminder>\n{body}\n</system-reminder>"),
                ),
                tool_use_id.clone(),
            ));
        }

        if post_agg.prevent_continuation {
            prevent_continuation = true;
            let reason = post_agg
                .reason
                .clone()
                .unwrap_or_else(|| format!("Execution stopped by {post_hook_event} hook"));
            // O2: the PERSISTED record. The model-facing prose below was
            // already byte-correct, but nothing reached the transcript —
            // the oracle yields a `hook_stopped_continuation` attachment
            // (BIN off 234726408) whose `message` sits SECOND in key order.
            orch.queue_hook_attachment(
                tool_use_id,
                hooks::stopped_continuation_attachment(
                    &hooks::HookAttachmentIdentity {
                        hook_name: post_hook_name.clone(),
                        hook_event: post_hook_event.to_string(),
                        tool_use_id: tool_use_id.as_str().to_string(),
                    },
                    &reason,
                ),
            )
            .await;
            injected_messages.push((
                ConversationMessage::user_meta(
                    MessageId::new(),
                    format!(
                        "<system-reminder>\n{post_hook_name} hook stopped continuation: {reason}\n</system-reminder>"
                    ),
                ),
                tool_use_id.clone(),
            ));
        }

        // HOOK.1 (additionalContext, PostToolUse twin): a PostToolUse hook's
        // `additionalContext` is ALSO a separate `hook_additional_context`
        // attachment in claude-code (`toolHooks.ts:133-143`), injected AFTER the
        // tool_result — exactly what the injected-messages seam does. The
        // hookName prefix follows the actual fired event. `systemMessage` stays folded
        // (handled by `final_content` below); only additionalContext splits out.
        // Strict no-op when no PostToolUse hook returned additionalContext.
        //
        // O3: claude emits ONE attachment carrying the whole `content` ARRAY
        // (BIN off 234726655), not one per entry; the renderer joins them with
        // `\n` into a single `<system-reminder>` message. The port keeps its
        // per-entry renderings on the injected channel (same model bytes when
        // there is one entry, which is every observed case) but the PERSISTED
        // record is the single attachment queued below.
        if !post_additional_contexts.is_empty() {
            orch.queue_hook_attachment(
                tool_use_id,
                hooks::additional_context_attachment(
                    &post_hook_name,
                    tool_use_id.as_str(),
                    post_hook_event,
                    &post_additional_contexts,
                ),
            )
            .await;
        }
        for ctx in &post_additional_contexts {
            let wrapped = format!(
                "<system-reminder>\n{post_hook_name} hook additional context: {ctx}\n</system-reminder>"
            );
            // `user_meta`: the rendering is `zr({isMeta:true})` and is
            // ephemeral — the attachment line above is the on-disk record.
            injected_messages.push((
                ConversationMessage::user_meta(MessageId::new(), wrapped),
                tool_use_id.clone(),
            ));
        }

        // PostToolUse `updatedToolOutput` (#38, all-tools) + `updatedMCPToolOutput`
        // (legacy, MCP-only) may REPLACE a SUCCESSFUL tool's output. claude yields
        // all-tools first, MCP second so MCP overrides (BIN off 202157140); applies
        // only if `outputSchema` is absent or validates (BIN off 202169384), else
        // keeps original + emits `hook_error_during_execution` (BIN off 202465455).
        // The substituted JSON feeds `tool_result_to_model_text` for its model text.
        // No-op when unset. Outer `Some` = key set even to `null` (`!== void 0`).
        let replacement: Option<serde_json::Value> = if is_error {
            None
        } else {
            let mut repl = post_agg
                .updated_tool_output
                .as_ref()
                .map(|inner| inner.clone().unwrap_or(serde_json::Value::Null));
            if tool_handle.is_mcp() {
                if let Some(mcp) = post_agg.updated_mcp_tool_output.as_ref() {
                    repl = Some(mcp.clone());
                }
            }
            repl
        };
        let (content, mcp_output_mutated) = match replacement {
            Some(new_output) => {
                // Validate against the tool's output schema when one exists
                // (`e.outputSchema?.safeParse(...)?.success!==!1`): substitute
                // unless validation EXPLICITLY fails. No schema → substitute.
                let schema_ok = match tool_handle.output_schema() {
                    Some(schema) => {
                        crate::schema_validation::validate_tool_output_schema(schema, &new_output)
                    }
                    None => Ok(()),
                };
                match schema_ok {
                    Ok(()) => (tool_result_to_model_text(&new_output), true),
                    Err(detail) => {
                        // Schema MISMATCH: keep the ORIGINAL output and surface
                        // the `hook_error_during_execution` meta message
                        // (BIN off 202465455) to the model, after the tool_result.
                        let msg = format!(
                            "PostToolUse hook returned updatedToolOutput that does not match {name}'s output shape; using original output. {detail}"
                        );
                        tracing::warn!(tool_name = %name, "{msg}");
                        // O3: this is a `hook_error_during_execution`
                        // attachment (2.1.220 BIN off 235421957 — the exact
                        // same message text). Its renderer entry is
                        // `hook_error_during_execution: () => []` (BIN off
                        // 238107100), so the MODEL NEVER SEES IT — the port
                        // previously pushed it onto the injected channel, which
                        // sent the model text claude suppresses.
                        orch.queue_hook_attachment(
                            tool_use_id,
                            hooks::error_during_execution_attachment(
                                &msg,
                                &format!("PostToolUse:{name}"),
                                tool_use_id.as_str(),
                                "PostToolUse",
                            ),
                        )
                        .await;
                        (content, false)
                    }
                }
            }
            None => (content, false),
        };

        // HOOK.1: BOTH the PreToolUse and the PostToolUse `additionalContext`
        // ride the `injected` channel as their OWN messages — neither is folded
        // into the tool-result content.
        //
        // O3 fix: the port used to ALSO concatenate every
        // `post_additional_contexts` entry onto the tool_result string, so a
        // PostToolUse hook's context reached the model TWICE. claude does
        // neither fold: its success arm (2.1.220 BIN off 235420375) assembles
        // the result blocks as `[formattedResult, acceptFeedback?,
        // ...contentBlocks?]` with no hook context, and the PostToolUse
        // consumer (BIN off 234726655) only yields the
        // `hook_additional_context` ATTACHMENT.
        //
        // `system_messages` was never folded and still is not: a PostToolUse
        // `systemMessage` is transcript/user-facing only and must NOT reach the
        // model (claude-code `hook_system_message` → `normalizeAttachmentForAPI`
        // returns `[]`, `messages.ts:4258`).
        //
        // `mutated` now tracks ONLY a genuine output REPLACEMENT
        // (`updatedToolOutput` / `updatedMCPToolOutput`), which is what the
        // `mutated_response` telemetry field means.
        let mutated = mcp_output_mutated;
        let final_content = content;

        tracing::info!(
            event = orch_events::HOOK_POST_COMPLETED,
            tool_name = %name,
            duration_ms = post_dur_ms,
            mutated_response = mutated,
        );
        telemetry::otel::emit_hook_lifecycle("post", "completed", name, Some(post_dur_ms));

        // Worktree-creation hook (parity with claude-code `executeWorktreeCreateHook`,
        // `utils/hooks.ts:4928`). claude-code fires `WorktreeCreate` from the
        // worktree-creation logic (`createWorktreeForSession` /
        // `createAgentWorktree`); the LingXi port creates worktrees only through
        // the registered, turn_loop-dispatched `EnterWorktree` tool, so we fire it
        // here — same TIMING (immediately after the worktree exists), the fire just
        // lives in the dispatch chokepoint alongside `PostToolUse`. Only a
        // SUCCESSFUL `EnterWorktree` result counts (an errored create never made a
        // worktree). The wire payload carries only `name` — the requested slug, the
        // single field claude-code passes to `executeWorktreeCreateHook(slug)`. We
        // thread the resolved `path`/`branch` as engine-side context too (not on the
        // wire). Best-effort: a failing/absent hook never breaks the worktree op
        // (`orch.hooks.execute` is a strict no-op when no `WorktreeCreate` hook is
        // registered, mirroring the `PostToolUse` arm above).
        if !is_error && name == ENTER_WORKTREE_TOOL_NAME {
            // `name` (slug) is the requested input; `path`/`branch_name` come from
            // the tool's result data (`{"path":…,"branch_name":…}`).
            let slug = effective_input
                .get("slug")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            let wt_path = emit_payload
                .get("path")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let wt_branch = emit_payload
                .get("branch_name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            let wt_event = HookEvent::WorktreeCreate {
                name: slug,
                path: std::path::PathBuf::from(wt_path),
                branch: wt_branch,
            };
            let wt_started = std::time::Instant::now();
            // Reuse the same hook context (session_id / cwd) the pre/post hooks used.
            let _wt_agg = orch.hooks.execute(wt_event, hook_ctx.clone()).await;
            // hook duration bounded by tokio timeout — u128 ms cannot exceed u64::MAX
            #[allow(clippy::cast_possible_truncation)]
            let wt_dur_ms = wt_started.elapsed().as_millis() as u64;
            // No `tengu_*` analytic here: claude-code's worktree-create path emits
            // no orchestrator-lifecycle event, so we keep parity by logging only.
            tracing::debug!(
                tool_name = %name,
                duration_ms = wt_dur_ms,
                "fired WorktreeCreate hook after successful EnterWorktree",
            );
        }

        // SubagentStart + SubagentStop hooks (claude `executeSubagentStartHooks`
        // runAgent.ts:532; `executeStopHooks`→`SubagentStop` utils/hooks.ts:3653-3678).
        // claude fires both in `runAgent` on ONE canonical `agentId` (runAgent.ts:347).
        // The port spawns subagents only via the dispatched `Agent`/`Task` tool, so we
        // fire here at spawn-completion (alongside PostToolUse/WorktreeCreate). Fires on
        // success AND failure (subagent started+stopped), NOT on pre-hook Block/deny
        // (those `continue` before any spawn). Best-effort.
        //
        // #8 (real id): the Agent tool surfaces the child's REAL pool `AgentId` on
        // `data.agentId` (C1 seam) so both events use one canonical id; a FAILED spawn
        // (no `data`) falls back to a fresh `AgentId::new()` — the single residual.
        //
        // SINGLE-FIRE (R7): the real tool's runner ALREADY fires SubagentStart
        // (runAgent.ts:530-555) + the child's frontmatter SubagentStop, marking
        // `data.subagentHooksFired`. So: skip the chokepoint SubagentStart when the
        // runner fired it (else fire — FakeAgentTool/failure); fire only the COMPLEMENT
        // SubagentStop via `execute_excluding_agent(child_id)` (omits the re-fired
        // frontmatter bucket, race-free vs `clear_agent_hooks`).
        if name == AGENT_TOOL_NAME || name == LEGACY_AGENT_TOOL_NAME {
            let subagent_type = effective_input
                .get("subagent_type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            // #8: recover the REAL child id surfaced on the success result's
            // `data.agentId`. Absent (the failure path carries `ToolError`, no
            // `data`) → fresh fallback id, the single residual divergence.
            let real_agent_id = emit_payload
                .get("agentId")
                .and_then(serde_json::Value::as_str)
                .and_then(protocol::AgentId::parse_prefixed);
            let child_id = real_agent_id.unwrap_or_else(protocol::AgentId::new);
            // R7: did the child runner already fire the canonical SubagentStart
            // (+ its own frontmatter SubagentStop)? Only the REAL Agent tool sets
            // this; FakeAgentTool fixtures and the failure path leave it absent.
            let runner_fired_start = emit_payload
                .get("subagentHooksFired")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            // G008: `fusion_tool_result` also stamps `subagentHooksFired: true`
            // (a Fusion run's panels already fired their own hooks — or, for
            // `fusion-panel`, none at all, by runner-side design) but NEVER an
            // `agentId` — a Fusion run is N panels, not one child with a
            // canonical id. That combination (`subagentHooksFired` true, no real
            // `agentId`) can only be a Fusion result: an ordinary failed Agent
            // spawn also lacks `agentId` but never sets `subagentHooksFired`
            // either. Skip BOTH SubagentStart and SubagentStop here instead of
            // firing a phantom `"fusion"` pair on a freshly-minted id that has
            // no transcript and no real child behind it.
            let is_fusion_result = runner_fired_start && real_agent_id.is_none();
            if is_fusion_result {
                tracing::debug!(
                    tool_name = %name,
                    "skipped chokepoint Subagent hooks for a Fusion tool result (no single child agent id)",
                );
            } else {
                // Carry the dispatched `subagent_type` as the hook context's
                // `agent_type` so the wire payload's `agent_type` is faithful
                // (claude-code passes the subagent's `agentType` into the hooks).
                // The session_id / cwd reuse the same context the pre/post hooks used.
                let mut sa_ctx = HookContext {
                    agent_type: Some(subagent_type.clone()),
                    agent_id: Some(child_id),
                    ..hook_ctx.clone()
                };
                // SubagentStart FIRST (claude start-then-stop), with the canonical id.
                // SKIP when the runner already fired it (no production double-fire).
                if !runner_fired_start {
                    let start_event = HookEvent::SubagentStart {
                        agent_id: child_id,
                        agent_type: subagent_type,
                        parent_agent_id: None,
                    };
                    let _start_agg = orch.hooks.execute(start_event, sa_ctx.clone()).await;
                }

                let status = if is_error { "failed" } else { "completed" };
                let sa_event = HookEvent::SubagentStop {
                    agent_id: child_id,
                    status: status.to_string(),
                    // Same subagent type as the SubagentStart above — claude keys
                    // SubagentStop matchers on it. `subagent_type` was moved into the
                    // SubagentStart event, so source it from the cloned `sa_ctx`.
                    agent_type: sa_ctx.agent_type.clone().unwrap_or_default(),
                };
                // claude-code stamps `background_tasks` + `session_crons` onto the
                // SubagentStop payload too (the `$Ee` firer's `...m` covers both the
                // Stop and SubagentStop branches when the tool-use context is
                // present). Populate the snapshot onto the SubagentStop context ONLY
                // (NOT the SubagentStart cloned above, which claude never carries it
                // on).
                orch.populate_stop_hook_snapshot(&mut sa_ctx).await;
                let sa_started = std::time::Instant::now();
                // EXCLUDE the child's own frontmatter bucket — the runner fired those
                // agent-scoped (claude fires a subagent's stop hooks in-child). This
                // covers session / plugin SubagentStop without double-firing the
                // child's frontmatter ones, race-free vs. the runner's
                // `clear_agent_hooks`.
                let _sa_agg = orch
                    .hooks
                    .execute_excluding_agent(sa_event, sa_ctx, child_id)
                    .await;
                // hook duration bounded by tokio timeout — u128 ms cannot exceed u64::MAX
                #[allow(clippy::cast_possible_truncation)]
                let sa_dur_ms = sa_started.elapsed().as_millis() as u64;
                // No `tengu_*` analytic here: claude-code's subagent-stop path emits
                // no orchestrator-lifecycle event, so we keep parity by logging only.
                tracing::debug!(
                    tool_name = %name,
                    status,
                    runner_fired_start,
                    duration_ms = sa_dur_ms,
                    "fired chokepoint SubagentStart (if runner didn't) + session/plugin SubagentStop after Agent/Task tool completed",
                );
            }
        }

        // MCP results carry the content-block array directly AS `data` (1:1 with
        // the binary's MCPTool result `data = mcpResult.content`) so the egress can
        // send it VERBATIM as `tool_result.content` (claude-code passes the MCP
        // content array directly — images/resources stay structured). When `data`
        // is an ARRAY it IS that wire form; a bare-string `data` (or large-output
        // file replacement) is not. Gated to MCP tools so non-MCP tools whose
        // `data` happens to be an array (e.g. the Agent tool's transcript blocks)
        // are unaffected. A hook-mutated result (output replaced or
        // additionalContext appended) drops to the text-only `final_content`.
        //
        // Non-MCP `{type:"image"}` results (Read on an image file, rendered PDF
        // pages) get the binary result-mapper's `case "image"` form: the image
        // block INSIDE the tool_result content (`image_tool_result_blocks`).
        // Bash `{isImage:true}` results get the binary's `hKn` form — the image
        // block derived from the stdout data-URI (`bash_image_tool_result_blocks`).
        let content_blocks = if mutated {
            None
        } else if tool_handle.is_mcp() {
            emit_payload.as_array().cloned()
        } else if name == "ToolSearch" {
            tool_search_reference_blocks(&emit_payload)
        } else {
            image_tool_result_blocks(&emit_payload)
                .or_else(|| bash_image_tool_result_blocks(&emit_payload))
        };
        // A1: the LAST thing that touches a successful `tool_result` before it
        // is handed to the model — claude-code's `yor` wrapper around the
        // result mapper (BIN off **235420440**:
        // `let Ft=[Dt ? await N0u(…) : await yor(e,gt,t)]`). Blank results get
        // the `(<tool> completed with no output)` sentinel; oversized ones are
        // written to `<session>/tool-results/` and replaced by a
        // `<persisted-output>` envelope.
        let process_output_file = if name == "Bash" {
            process_output_file_from_data(&emit_payload)
        } else {
            None
        };
        let persistence = apply_tool_result_persistence_with_process_output(
            orch,
            name,
            tool_use_id,
            tool_handle.persistence_threshold().map(|raw| {
                crate::tool_result_persistence::resolve_threshold(
                    raw,
                    tool_handle.persistence_threshold_ceiling(),
                )
            }),
            final_content,
            content_blocks.as_deref(),
            process_output_file.as_ref(),
        )
        .await;
        // claude-code's `F0u` substitutes the ONE model-facing payload
        // (`{...e, content: a}`, where `content` is a string OR an array).
        // LingXi splits that payload in two and the wire prefers the array when
        // present, so a substitution must drop the array too — otherwise the
        // envelope is computed, the file written, the telemetry fired, and the
        // model still receives the full oversized payload.
        let (final_content, content_blocks) = if persistence.replaced {
            (
                persistence.content,
                persistence
                    .utf16_code_units
                    .map(protocol::js_utf16::tool_result_sidecar),
            )
        } else {
            (persistence.content, content_blocks)
        };
        results.push(ContentBlock::ToolResult {
            tool_use_id: tool_use_id.clone(),
            content: final_content,
            is_error,
            provider_tool_use_id: provider_id.clone(),
            content_blocks,
        });

        // HOOK.1: queue this tool's PreToolUse `additionalContext` as its OWN
        // message on the `injected` channel, tagged with this tool's
        // `tool_use_id` (TS `toolUseID`). Both drivers append `injected` AFTER
        // the tool_result user message, so the context is ordered after the
        // result — matching claude-code's `resultingMessages` push order
        // (`toolExecution.ts:845`). `None` (the common no-context case) is a
        // strict no-op.
        if let Some(msg) = pre_context_message {
            injected_messages.push((msg, tool_use_id.clone()));
        }

        // FIX C (PreToolUse hook_stopped_continuation): emit the stop-reason meta
        // AFTER this tool's tool_result, mirroring claude's post-execution push
        // (`toolExecution.ts:1571`). Ordered after `pre_context_message` so the
        // relative order matches claude (additionalContext at 846 → stopped at
        // 1571). Success path only — a Block/Defer `continue`d above without ever
        // executing the tool, so this site is unreached there. No-op when the
        // hook did not request preventContinuation.
        if let Some((msg, attachment)) = pre_prevent {
            // O2: the persisted record rides the same tool-keyed queue as the
            // additionalContext one, so it is flushed right after this tool's
            // tool_result — the position the oracle's post-execution yield puts
            // it in.
            orch.queue_hook_attachment(tool_use_id, attachment).await;
            injected_messages.push((msg, tool_use_id.clone()));
        }
    }

    // The oracle builds PostToolBatch from every assistant `tool_use` block,
    // then looks up each final yielded `tool_result.content` by id. Therefore
    // original (pre-hook) input is retained, error/synthetic results are
    // included, and a call that yielded no result has no `tool_response`.
    let post_tool_batch_calls = tool_uses
        .iter()
        .map(|(id, name, input, _)| {
            let tool_response = results.iter().find_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    content_blocks,
                    ..
                } if tool_use_id == id => Some(content_blocks.as_ref().map_or_else(
                    || serde_json::Value::String(content.clone()),
                    |blocks| {
                        serde_json::to_value(blocks)
                            .unwrap_or_else(|_| serde_json::Value::String(content.clone()))
                    },
                )),
                _ => None,
            });
            hooks::events::PostToolBatchCall {
                tool_name: name.clone(),
                tool_input: input.clone(),
                tool_use_id: id.clone(),
                tool_response,
            }
        })
        .collect();

    Ok(DeferredToolDispatch {
        results,
        prevent_continuation,
        injected_messages,
        context_modifiers,
        post_tool_batch_calls,
    })
}

/// Compatibility surface for direct dispatch callers and focused hook tests.
/// Conversation drivers use [`dispatch_tool_uses_tracked_deferred`] so they can
/// place `PostToolBatch` after persistence and coalesce streaming calls.
pub(crate) async fn dispatch_tool_uses_tracked(
    orch: &ConversationOrchestrator,
    tool_uses: &[(ToolUseId, String, serde_json::Value, Option<String>)],
    cancel: Option<tokio_util::sync::CancellationToken>,
) -> Result<
    (
        Vec<ContentBlock>,
        bool,
        Vec<(ConversationMessage, ToolUseId)>,
        Vec<ContextModifier>,
    ),
    OrchestratorError,
> {
    let mut dispatched = dispatch_tool_uses_tracked_deferred(orch, tool_uses, cancel, None).await?;
    if !dispatched.prevent_continuation {
        let (batch_prevent, batch_messages) =
            run_post_tool_batch_hooks(orch, dispatched.post_tool_batch_calls).await;
        dispatched.prevent_continuation |= batch_prevent;
        dispatched.injected_messages.extend(batch_messages);
    }
    Ok((
        dispatched.results,
        dispatched.prevent_continuation,
        dispatched.injected_messages,
        dispatched.context_modifiers,
    ))
}
