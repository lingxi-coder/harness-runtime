use super::batch_hooks::run_post_tool_batch_hooks;
use super::tool_results::{
    apply_tool_result_persistence_with_process_output, bash_image_tool_result_blocks,
    image_tool_result_blocks, process_output_file_from_data, tool_result_to_model_text,
    tool_search_reference_blocks,
};
use super::{
    AGENT_TOOL_NAME, CANCEL_MESSAGE, ENTER_WORKTREE_TOOL_NAME, PERMISSION_DENIED_RETRY_MESSAGE,
    apply_terminal_sequence,
};
use crate::autonomous_tool_scheduler::ToolDispatchPublicationFence;
use crate::conversation::{
    ConversationOrchestrator, ModResultStage, active_mod_result_stage_is_virtual,
    with_mod_result_stage, with_virtual_mod_result_stage,
};
use crate::error::OrchestratorError;
use crate::test_support::{PermissionDecision, PermissionDecisionSource, PermissionResolution};
use hooks::attachment::HookPublicationGuard;
use hooks::events::HookEvent;
use hooks::registry::HookContext;
use hooks::response::HookDecision;
use lingxi_core::host::tool_invoker::tool_call_ref_index;
use lingxi_core::types::{
    ContentBlock, ConversationMessage, MessageId, ToolUseId,
    utf16_json::{Utf16JsonKey, Utf16JsonProjection, Utf16JsonString},
};
use std::path::{Component, Path, PathBuf};
use telemetry::tengu::orchestrator as orch_events;
use tool_api::ContextModifier;
use tool_api::context::{ToolUseContext, ToolUseOptions};
use tool_api::tool_trait::tool_result_turn_end;

#[path = "mod_agent_api.rs"]
mod mod_agent_api;

fn session_usage_js_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".into(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Array(values) => values
            .iter()
            .map(|value| match value {
                serde_json::Value::Null => String::new(),
                value => session_usage_js_string(value),
            })
            .collect::<Vec<_>>()
            .join(","),
        serde_json::Value::Object(_) => "[object Object]".into(),
    }
}

fn mod_tool_call_arguments(
    input: &serde_json::Value,
) -> serde_json::Map<String, serde_json::Value> {
    let mut arguments = input.as_object().cloned().unwrap_or_default();
    let shadowed = arguments
        .remove("$shadowed")
        .and_then(|value| value.as_object().cloned());
    for key in ["tool", "tool_use_id", "agentId", "consent"] {
        arguments.remove(key);
    }
    if let Some(shadowed) = shadowed {
        arguments.extend(shadowed);
    }
    arguments
}

fn mod_tool_call_projects_consent_rejected(
    tool_name: &str,
    facts: &hooks::mods::ProjectsConsentFacts,
) -> Result<bool, hooks::mods::ModError> {
    if !matches!(tool_name, "WebFetch" | "WebSearch") {
        return Ok(false);
    }

    let base_facts = [
        facts.default_host_sticky_latch,
        facts.projects_env,
        facts.session_mcp_signal,
    ];
    let in_projects_session = base_facts.contains(&Some(true));
    if !in_projects_session {
        if base_facts.iter().all(Option::is_some) {
            return Ok(false);
        }
        return Err(hooks::mods::ModError::Unavailable(
            "$.tool.call needs authoritative Projects-session facts for WebFetch/WebSearch consent"
                .into(),
        ));
    }

    let Some(feature) = facts.feature_result else {
        return Err(hooks::mods::ModError::Unavailable(
            "$.tool.call needs the authoritative Projects feature source for WebFetch/WebSearch consent"
                .into(),
        ));
    };
    if feature.value && feature.source == hooks::mods::ProjectsFeatureSource::Payload {
        return match facts.growthbook_used_non_default_host {
            Some(false) => Ok(false),
            Some(true) => Ok(true),
            None => Err(hooks::mods::ModError::Unavailable(
                "$.tool.call needs the authoritative GrowthBook host fact for WebFetch/WebSearch consent"
                    .into(),
            )),
        };
    }
    Ok(true)
}

/// Native 2.1.291's `pee` hard-holds a Mod `tool.check` Allow for WebFetch and
/// WebSearch only when a Projects-session trigger is present, except when the
/// feature is explicitly enabled from a default-host payload. Missing host
/// observations remain unknown; they do not become positive protection facts.
/// Native's separate computer-use MCP predicate is disabled in 2.1.291, and
/// this predicate intentionally does not infer protection from `Tool::is_mcp`.
fn projects_session_tool_check_mod_hard_hold(
    tool_name: &str,
    facts: &hooks::mods::ProjectsConsentFacts,
) -> Option<bool> {
    if !matches!(tool_name, "WebFetch" | "WebSearch") {
        // Native 2.1.291's `eVe(mcpInfo)` path cannot hold: its `pDe()` helper
        // returns false. Other MCP tools are not protected by `pee` either.
        return Some(false);
    }

    let session_facts = [
        facts.default_host_sticky_latch,
        facts.projects_env,
        facts.session_mcp_signal,
    ];
    if !session_facts.contains(&Some(true)) {
        return session_facts.iter().all(Option::is_some).then_some(false);
    }

    // Native calls `gr(F, false)`, so an unavailable equivalent feature value
    // follows that explicit fallback rather than inventing a payload value.
    let feature = facts
        .feature_result
        .unwrap_or(hooks::mods::ProjectsFeatureResult {
            value: false,
            source: hooks::mods::ProjectsFeatureSource::Fallback,
        });
    if feature.value && feature.source == hooks::mods::ProjectsFeatureSource::Unknown {
        return None;
    }
    if feature.value && feature.source == hooks::mods::ProjectsFeatureSource::Payload {
        return facts
            .growthbook_used_non_default_host
            .map(|used_non_default_host| used_non_default_host);
    }
    Some(true)
}

async fn projects_session_tool_check_mod_hard_hold_for_call(
    orch: &ConversationOrchestrator,
    tool_name: &str,
) -> bool {
    if !matches!(tool_name, "WebFetch" | "WebSearch") {
        return false;
    }
    let facts = orch.resolved_mod_projects_consent_facts().await;
    projects_session_tool_check_mod_hard_hold(tool_name, &facts) == Some(true)
}

fn mod_tool_call_agent_result(agent_id: &str, raw_result: &serde_json::Value) -> serde_json::Value {
    let mut result = serde_json::Map::new();
    result.insert(
        "agentId".into(),
        serde_json::Value::String(agent_id.to_owned()),
    );
    if let Some(resolved_model) = raw_result
        .get("resolvedModel")
        .and_then(serde_json::Value::as_str)
    {
        result.insert(
            "resolvedModel".into(),
            serde_json::Value::String(resolved_model.to_owned()),
        );
    }
    serde_json::Value::Object(result)
}

fn mod_tool_call_agent_error(text: String) -> serde_json::Value {
    serde_json::json!({"text": text, "isError": true})
}

fn mod_tool_call_hook_caller(
    context: &hooks::mods::ModToolCallContext,
) -> Result<String, hooks::mods::ModError> {
    match &context.agent_spawn_provenance.hook_caller {
        lingxi_core::host::task_registry::FieldPresence::Value(serde_json::Value::String(
            plugin,
        )) => Ok(plugin.clone()),
        _ => Err(hooks::mods::ModError::Protocol(
            "tool.call transaction is missing its trusted hook caller".into(),
        )),
    }
}

fn mod_tool_call_agent_failure_text(plugin: &str, status: &str, output: Option<&str>) -> String {
    match output.filter(|output| !output.is_empty()) {
        Some(output) => format!("{plugin}: $.agent.spawn: the subagent {status}: {output}"),
        None => format!("{plugin}: $.agent.spawn: the subagent {status}"),
    }
}

/// Opaque host handle placed in `PreparedModToolCall`; the `Tool` object is
/// resolved from the active catalog once, before middleware can run.
struct PreparedToolHandle(std::sync::Arc<dyn tool_api::tool_trait::Tool>);

/// A registered Mod tool is advertised as an MCP-shaped tool. Its execution
/// goes through the surrounding `tool.call` Mod chain; reaching this bottom
/// handler means no Mod answered the call.
struct ModRegisteredTool {
    name: String,
    description: String,
    schema: serde_json::Value,
}

#[async_trait::async_trait]
impl tool_api::tool_trait::Tool for ModRegisteredTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn input_schema(&self) -> &serde_json::Value {
        &self.schema
    }
    fn input_validation_schema(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::LazyLock<serde_json::Value> =
            std::sync::LazyLock::new(|| serde_json::json!({"type":"object"}));
        &SCHEMA
    }
    fn is_enabled(&self, _: &tool_api::tool_trait::ToolStaticContext) -> bool {
        true
    }
    fn is_mcp(&self) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        30_000
    }
    fn is_concurrency_safe(&self, _: &serde_json::Value) -> bool {
        false
    }
    fn is_read_only(&self, _: &serde_json::Value) -> bool {
        false
    }
    fn is_open_world(&self, _: &serde_json::Value) -> bool {
        true
    }
    fn interrupt_behavior(&self, _: &serde_json::Value) -> tool_api::tool_trait::InterruptBehavior {
        tool_api::tool_trait::InterruptBehavior::Cancel
    }
    async fn check_permissions(
        &self,
        _: &serde_json::Value,
        _: &ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "Mod tool".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }
    async fn description(
        &self,
        _: &serde_json::Value,
        _: &tool_api::tool_trait::DescriptionOptions,
    ) -> String {
        self.description.clone()
    }
    async fn prompt(&self, _: &tool_api::tool_trait::PromptOptions) -> String {
        self.description.clone()
    }
    async fn call(
        &self,
        _: serde_json::Value,
        _: ToolUseContext,
        _: tool_api::progress::ToolProgressSender,
    ) -> Result<tool_api::tool_trait::ToolCallResult, tool_api::tool_trait::ToolError> {
        Err(tool_api::tool_trait::ToolError::Internal(format!(
            "{}: registered Mod tool call was not answered",
            self.name
        )))
    }
}

pub(super) async fn forward_tool_progress(
    output: &dyn lingxi_core::host::OutputStream,
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
    } else if let Some(payload) = progress.data.get("forward_server_fallback_tombstone") {
        let Some(message) = payload.get("message") else {
            tracing::warn!("forwarded subagent tombstone omitted its row");
            return;
        };
        let Some(display_only) = payload
            .get("display_only")
            .and_then(serde_json::Value::as_bool)
        else {
            tracing::warn!("forwarded subagent tombstone omitted display_only");
            return;
        };
        match serde_json::from_value::<lingxi_core::host::ServerFallbackTombstoneMessage>(
            message.clone(),
        ) {
            Ok(message) => {
                output
                    .emit_server_fallback_tombstone(&message, display_only)
                    .await
            }
            Err(error) => tracing::warn!(%error, "invalid forwarded subagent tombstone row"),
        }
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
    use permission::PermissionDecisionReason as R;
    use permission::result::SandboxOverrideReason;
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
/// carve-outs remain unchanged. This is Ask composition/transport state, not a
/// blanket Mod `tool.check` hard hold; that hold uses Native's separate exact
/// Projects WebFetch/WebSearch predicate below.
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
/// but does not make the hook rescue unsafe. This remains independent of a
/// Mod's earlier `tool.check` verdict.
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

fn permission_tool_check_mode_name(
    mode: lingxi_core::host::permission_gate::PermissionToolCheckMode,
) -> &'static str {
    use lingxi_core::host::permission_gate::PermissionToolCheckMode as M;
    match mode {
        M::Default | M::Bubble => "default",
        M::Plan => "plan",
        M::AcceptEdits => "acceptEdits",
        M::BypassPermissions => "bypassPermissions",
        M::DontAsk => "dontAsk",
        M::Auto => "auto",
    }
}

fn tool_check_result_decision_reason_type(
    reason: &lingxi_core::host::permission_gate::PermissionToolCheckReason,
) -> Option<String> {
    use lingxi_core::host::permission_gate::PermissionToolCheckReason as R;
    Some(
        match reason {
            R::MatchedRule { .. } => "rule",
            R::PermissionMode { .. } => "mode",
            R::SubcommandResults { .. } => "subcommandResults",
            R::PermissionPromptTool { .. } => "permissionPromptTool",
            R::ClassifierApproved { .. } | R::ClassifierRejected { .. } => "classifier",
            R::HookOverride { .. } => "hook",
            R::AsyncAgent { .. } => "asyncAgent",
            R::SandboxOverride { .. } => "sandboxOverride",
            R::WorkingDirectory { .. } => "workingDir",
            R::SafetyCheck { .. } => "safetyCheck",
            R::Other { .. } => "other",
            R::DenialLimitExceeded | R::AutoModeFallback | R::BypassPermissions => return None,
        }
        .to_string(),
    )
}

fn tool_check_result_rule_source(
    reason: &lingxi_core::host::permission_gate::PermissionToolCheckReason,
    behavior: lingxi_core::host::permission_gate::PermissionToolCheckBehavior,
) -> Option<String> {
    use lingxi_core::host::permission_gate::{
        PermissionToolCheckReason as R, PermissionToolCheckRuleSource as S,
    };
    match reason {
        R::MatchedRule { rule } if rule.behavior == behavior => Some(
            match rule.source {
                S::UserSettings => "userSettings",
                S::ProjectSettings => "projectSettings",
                S::LocalSettings => "localSettings",
                S::ManagedPolicy => "policySettings",
                S::FlagSettings => "flagSettings",
                S::CliArg => "cliArg",
                S::Command => "command",
                S::Session => "session",
                S::ToolsNarrowing => "toolsNarrowing",
                S::McpServerPolicy => "mcpServerPolicy",
            }
            .to_string(),
        ),
        R::SubcommandResults { reasons } => reasons.iter().find_map(|(_, child)| {
            let (child_behavior, child_reason) = match child.as_ref() {
                lingxi_core::host::permission_gate::PermissionToolCheckResult::Allow {
                    reason,
                    ..
                } => (
                    lingxi_core::host::permission_gate::PermissionToolCheckBehavior::Allow,
                    reason,
                ),
                lingxi_core::host::permission_gate::PermissionToolCheckResult::Deny {
                    reason,
                    ..
                } => (
                    lingxi_core::host::permission_gate::PermissionToolCheckBehavior::Deny,
                    reason,
                ),
                lingxi_core::host::permission_gate::PermissionToolCheckResult::Ask {
                    reason,
                    ..
                } => (
                    lingxi_core::host::permission_gate::PermissionToolCheckBehavior::Ask,
                    reason,
                ),
            };
            (child_behavior == behavior)
                .then(|| tool_check_result_rule_source(child_reason, behavior))
                .flatten()
        }),
        _ => None,
    }
}

fn raw_permission_resolution_from_tool_check(
    name: &str,
    evaluation: &lingxi_core::host::permission_gate::PermissionToolCheckEvaluation,
) -> PermissionResolution {
    use lingxi_core::host::permission_gate::{
        ModToolCheckDecision, PermissionDecisionSource, PermissionResolution as R,
        PermissionToolCheckResult,
    };
    let reason = match &evaluation.result {
        PermissionToolCheckResult::Allow { reason, .. }
        | PermissionToolCheckResult::Deny { reason, .. }
        | PermissionToolCheckResult::Ask { reason, .. } => reason,
    };
    let decision_reason_type = tool_check_result_decision_reason_type(reason);
    let behavior = match evaluation.verdict.decision {
        ModToolCheckDecision::Allow => {
            lingxi_core::host::permission_gate::PermissionToolCheckBehavior::Allow
        }
        ModToolCheckDecision::Ask => {
            lingxi_core::host::permission_gate::PermissionToolCheckBehavior::Ask
        }
        ModToolCheckDecision::Deny => {
            lingxi_core::host::permission_gate::PermissionToolCheckBehavior::Deny
        }
    };
    let rule_source = tool_check_result_rule_source(reason, behavior);
    match evaluation.verdict.decision {
        ModToolCheckDecision::Allow => R::Allow {
            rule_source,
            classifier_approved: matches!(
                reason,
                lingxi_core::host::permission_gate::PermissionToolCheckReason::ClassifierApproved { .. }
            ),
        },
        ModToolCheckDecision::Ask => R::AskWithContext {
            decision_reason_type,
            decision_reason: evaluation.verdict.reason.clone(),
        },
        ModToolCheckDecision::Deny => R::Deny {
            reason: evaluation
                .verdict
                .reason
                .clone()
                .unwrap_or_else(|| format!("Permission to use {name} has been denied.")),
            source: if evaluation.verdict.rule.is_some() {
                PermissionDecisionSource::Rule
            } else {
                PermissionDecisionSource::Mode
            },
            rule_source,
            decision_reason_type,
            decision_reason: None,
            behavior_ask: false,
            content_blocks: Vec::new(),
        },
    }
}

fn compose_tool_permission_result(
    name: &str,
    input: &serde_json::Value,
    requires_user_interaction: bool,
    tool_ask_is_protected: bool,
    policy_ask_rule: bool,
    bypass_mode: bool,
    resolution: PermissionResolution,
    tool_permission_result: &permission::PermissionResult,
) -> (
    PermissionResolution,
    Option<permission::PermissionDecisionReason>,
) {
    let mut tool_ask_reason = None;
    let resolution = match (&resolution, tool_permission_result) {
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
                    && input.get("ws").is_some()
                    && matches!(reason, permission::PermissionDecisionReason::Other { .. }))) =>
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
            if policy_ask_rule {
                // Native `Urn` returns an explicit ask rule before it applies
                // `effectiveMaxPermission === "ask"`. Keep the rule's reason
                // and let the rule-aware permission transport handle the ask;
                // the org ceiling still prevents any later allow from winning.
                resolution
            } else {
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
        }
        _ => resolution,
    };
    (resolution, tool_ask_reason)
}

fn permission_check_updated_input(
    resolution: &PermissionResolution,
    evaluation: Option<&lingxi_core::host::permission_gate::PermissionToolCheckEvaluation>,
    tool_permission_result: Option<&permission::PermissionResult>,
) -> Option<serde_json::Value> {
    if !matches!(resolution, PermissionResolution::Allow { .. }) {
        return None;
    }
    if let Some(lingxi_core::host::permission_gate::PermissionToolCheckResult::Allow {
        updated_input: Some(updated),
        ..
    }) = evaluation.map(|evaluation| &evaluation.result)
    {
        return Some(updated.clone());
    }
    if let Some(permission::PermissionResult::Allow {
        updated_input: Some(updated),
        ..
    }) = tool_permission_result
    {
        return Some(updated.clone());
    }
    None
}

async fn evaluate_normal_tool_check_core(
    orch: &ConversationOrchestrator,
    name: &str,
    input: &serde_json::Value,
    tool_use_id: &ToolUseId,
    tool: &dyn tool_api::tool_trait::Tool,
    tool_ctx: &ToolUseContext,
    tool_check_ceiling: Option<lingxi_core::host::McpPermissionCeiling>,
    plan_mode: bool,
    hook_ask: bool,
    requires_user_interaction: bool,
    restricted_protected_mutation: bool,
) -> Result<NormalToolCheckCore, OrchestratorError> {
    // Native `hVt` consults explicit deny rules before it parses the input or
    // calls the tool's `checkPermissions`. Keep that early rejection inside
    // the lazy core callback so a direct Mod answer still skips it altogether.
    let policy_ctx = lingxi_core::host::permission_gate::PermissionCheckContext {
        input_projection: Some(
            tool_ctx
                .projected_input(input)
                .map_err(|error| OrchestratorError::Internal(error.to_string()))?,
        ),
        tool_use_id: Some(tool_use_id.to_string()),
        tool_check_ceiling,
        requires_user_interaction,
        suppress_always_allow_rule: requires_user_interaction || restricted_protected_mutation,
        hook_ask_floor: hook_ask,
        is_non_interactive_session: !orch.config.interactive_permissions,
        mode_override: plan_mode.then(|| "plan".to_string()),
        ..Default::default()
    };
    let evaluation = orch.perms.check_mod_query(name, input, &policy_ctx).await;
    if let Some(evaluation) = &evaluation {
        if evaluation.verdict.decision
            == lingxi_core::host::permission_gate::ModToolCheckDecision::Deny
            && evaluation.verdict.rule.is_some()
        {
            return Ok(NormalToolCheckCore {
                resolution: raw_permission_resolution_from_tool_check(name, evaluation),
                evaluation: Some(evaluation.clone()),
                tool_ask_reason: None,
                updated_input: None,
                tool_permission_result: None,
            });
        }
    }
    let tool_permission_result = tool.check_permissions(input, tool_ctx).await;
    let tool_ask_is_protected = tool_permission_ask_is_protected(tool, &tool_permission_result);
    let policy_ask_rule = evaluation.as_ref().is_some_and(|evaluation| {
        evaluation.verdict.decision == lingxi_core::host::permission_gate::ModToolCheckDecision::Ask
            && evaluation.verdict.rule.is_some()
    });
    let mut resolution = evaluation.as_ref().map_or_else(
        || PermissionResolution::Allow {
            rule_source: None,
            classifier_approved: false,
        },
        |evaluation| raw_permission_resolution_from_tool_check(name, evaluation),
    );
    if hook_ask && matches!(resolution, PermissionResolution::Allow { .. }) {
        resolution = PermissionResolution::Ask;
    }
    let bypass_mode = evaluation.as_ref().map_or_else(
        || orch.permission_mode().as_deref() == Some("bypassPermissions"),
        |evaluation| {
            permission_tool_check_mode_name(evaluation.effective_mode) == "bypassPermissions"
        },
    );
    let (resolution, tool_ask_reason) = compose_tool_permission_result(
        name,
        input,
        requires_user_interaction,
        tool_ask_is_protected,
        policy_ask_rule,
        bypass_mode,
        resolution,
        &tool_permission_result,
    );
    let updated_input = permission_check_updated_input(
        &resolution,
        evaluation.as_ref(),
        Some(&tool_permission_result),
    );
    Ok(NormalToolCheckCore {
        resolution,
        evaluation,
        tool_ask_reason,
        updated_input,
        tool_permission_result: Some(tool_permission_result),
    })
}

async fn finish_normal_tool_check_core(
    orch: &ConversationOrchestrator,
    name: &str,
    input: &serde_json::Value,
    tool_use_id: &ToolUseId,
    tool: &dyn tool_api::tool_trait::Tool,
    tool_ctx: &ToolUseContext,
    plan_mode: bool,
    hook_ask: bool,
    requires_user_interaction: bool,
    restricted_protected_mutation: bool,
    mut core: NormalToolCheckCore,
) -> Result<NormalToolCheckCore, OrchestratorError> {
    let resolution_ctx = lingxi_core::host::permission_gate::PermissionCheckContext {
        input_projection: Some(
            tool_ctx
                .projected_input(input)
                .map_err(|error| OrchestratorError::Internal(error.to_string()))?,
        ),
        tool_use_id: Some(tool_use_id.to_string()),
        tool_check_ceiling: core
            .evaluation
            .as_ref()
            .and_then(|evaluation| evaluation.ceiling),
        requires_user_interaction,
        suppress_always_allow_rule: requires_user_interaction || restricted_protected_mutation,
        hook_ask_floor: hook_ask,
        is_non_interactive_session: !orch.config.interactive_permissions,
        mode_override: plan_mode.then(|| "plan".to_string()),
        ..Default::default()
    };
    let resolution = if let Some(evaluation) = &core.evaluation {
        orch.perms
            .resolve_tool_check_execution(name, input, &resolution_ctx, evaluation)
            .await
    } else if plan_mode {
        orch.perms
            .resolve_detailed_in_plan_mode_or_abort(name, input, &resolution_ctx)
            .await
    } else {
        orch.perms
            .resolve_detailed_or_abort(name, input, &resolution_ctx)
            .await
    }
    .map_err(|abort| OrchestratorError::PermissionAbort {
        message: abort.message,
    })?;
    let resolution = if hook_ask && matches!(resolution, PermissionResolution::Allow { .. }) {
        PermissionResolution::Ask
    } else {
        resolution
    };
    let tool_ask_is_protected = core
        .tool_permission_result
        .as_ref()
        .is_some_and(|result| tool_permission_ask_is_protected(tool, result));
    let policy_ask_rule = core.evaluation.as_ref().is_some_and(|evaluation| {
        evaluation.verdict.decision == lingxi_core::host::permission_gate::ModToolCheckDecision::Ask
            && evaluation.verdict.rule.is_some()
    });
    let bypass_mode = core.evaluation.as_ref().map_or_else(
        || orch.permission_mode().as_deref() == Some("bypassPermissions"),
        |evaluation| {
            permission_tool_check_mode_name(evaluation.effective_mode) == "bypassPermissions"
        },
    );
    if let Some(tool_permission_result) = &core.tool_permission_result {
        let (resolution, tool_ask_reason) = compose_tool_permission_result(
            name,
            input,
            requires_user_interaction,
            tool_ask_is_protected,
            policy_ask_rule,
            bypass_mode,
            resolution,
            tool_permission_result,
        );
        core.resolution = resolution;
        core.tool_ask_reason = tool_ask_reason;
    } else {
        core.resolution = resolution;
        core.tool_ask_reason = None;
    }
    core.updated_input = permission_check_updated_input(
        &core.resolution,
        core.evaluation.as_ref(),
        core.tool_permission_result.as_ref(),
    );
    Ok(core)
}

async fn resolve_normal_tool_check_core(
    orch: &ConversationOrchestrator,
    name: &str,
    input: &serde_json::Value,
    tool_use_id: &ToolUseId,
    tool: &dyn tool_api::tool_trait::Tool,
    tool_ctx: &ToolUseContext,
    tool_check_ceiling: Option<lingxi_core::host::McpPermissionCeiling>,
    plan_mode: bool,
    hook_ask: bool,
    requires_user_interaction: bool,
    restricted_protected_mutation: bool,
) -> Result<NormalToolCheckCore, OrchestratorError> {
    let core = evaluate_normal_tool_check_core(
        orch,
        name,
        input,
        tool_use_id,
        tool,
        tool_ctx,
        tool_check_ceiling,
        plan_mode,
        hook_ask,
        requires_user_interaction,
        restricted_protected_mutation,
    )
    .await?;
    finish_normal_tool_check_core(
        orch,
        name,
        input,
        tool_use_id,
        tool,
        tool_ctx,
        plan_mode,
        hook_ask,
        requires_user_interaction,
        restricted_protected_mutation,
        core,
    )
    .await
}

/// Results from dispatching a tool batch before the once-per-batch hook runs.
///
/// The two conversation drivers persist the tool results first, then run the
/// deferred `PostToolBatch` event. This split is load-bearing: claude-code
/// observes end-turn metadata only after yielding the results and emits its
/// end-turn telemetry before `PostToolBatch`; the streaming executor also
/// dispatches tools one at a time, so firing the hook inside this function
/// would incorrectly produce one batch event per tool.
#[derive(Clone)]
pub(crate) struct ToolResultFramePublication {
    pub(crate) tool: String,
    pub(crate) model_text: String,
    pub(crate) result: serde_json::Value,
    pub(crate) denial_kind: Option<String>,
}

/// Result side channels staged by W1 and committed only when Native's Tn path
/// accepts the result row. A discarded actor generation therefore has no path
/// to publish transcript metadata or a client ToolResult frame.
#[derive(Clone)]
pub(crate) struct ToolResultPublication {
    pub(crate) tool_use_id: ToolUseId,
    pub(crate) tool_use_result: Option<serde_json::Value>,
    pub(crate) mcp_meta: Option<serde_json::Value>,
    pub(crate) turn_end: Option<tool_api::tool_trait::ToolResultTurnEnd>,
    pub(crate) denial_kind: Option<String>,
    pub(crate) permission_denial: Option<(String, serde_json::Value)>,
    pub(crate) frame: Option<ToolResultFramePublication>,
}

impl ToolResultPublication {
    pub(crate) fn frame_only(
        tool_use_id: &ToolUseId,
        tool: &str,
        model_text: &str,
        result: serde_json::Value,
    ) -> Self {
        Self {
            tool_use_id: tool_use_id.clone(),
            tool_use_result: None,
            mcp_meta: None,
            turn_end: None,
            denial_kind: None,
            permission_denial: None,
            frame: Some(ToolResultFramePublication {
                tool: tool.to_owned(),
                model_text: model_text.to_owned(),
                result,
                denial_kind: None,
            }),
        }
    }

    pub(crate) async fn commit_metadata(&self, orch: &ConversationOrchestrator) {
        if let Some((tool_name, tool_input)) = &self.permission_denial {
            orch.record_permission_denial(tool_name, &self.tool_use_id, tool_input)
                .await;
        }
        if let Some(denial_kind) = &self.denial_kind {
            orch.record_tool_denial_kind(&self.tool_use_id, denial_kind)
                .await;
        }
        if let Some(result) = &self.tool_use_result {
            orch.record_tool_use_result(&self.tool_use_id, result.clone())
                .await;
        }
        if let Some(meta) = &self.mcp_meta {
            orch.record_tool_use_mcp_meta(&self.tool_use_id, meta.clone())
                .await;
        }
        if let Some(turn_end) = self.turn_end {
            orch.record_pending_tool_result_turn_end(&self.tool_use_id, turn_end)
                .await;
        }
    }

    async fn commit(self, orch: &ConversationOrchestrator) {
        self.commit_metadata(orch).await;
        if let Some(frame) = self.frame {
            orch.emit_tool_result_frame(
                &self.tool_use_id,
                &frame.tool,
                &frame.model_text,
                &frame.result,
                frame.denial_kind.as_deref(),
            )
            .await;
        }
    }
}

#[derive(Default)]
pub(crate) struct DeferredToolDispatch {
    pub(crate) results: Vec<ContentBlock>,
    /// Per-result metadata and frame payloads withheld from W1 until Tn.
    pub(crate) publications: Vec<ToolResultPublication>,
    /// Stop requested by a per-tool Pre/PostToolUse hook. This has precedence
    /// over a tool result's end-turn marker and suppresses `PostToolBatch`.
    pub(crate) prevent_continuation: bool,
    pub(crate) injected_messages: Vec<(ConversationMessage, ToolUseId)>,
    pub(crate) context_modifiers: Vec<ContextModifier>,
    pub(crate) post_tool_batch_calls: Vec<hooks::events::PostToolBatchCall>,
}

impl DeferredToolDispatch {
    /// Publish an accepted batch before serializing its result rows or consuming
    /// end-turn requests. Both native and ordinary dispatch defer this boundary.
    pub(crate) async fn publish_results(
        &mut self,
        orch: &ConversationOrchestrator,
        fence: &crate::autonomous_tool_scheduler::ToolDispatchPublicationFence,
    ) -> bool {
        for publication in std::mem::take(&mut self.publications) {
            if !fence
                .commit_if_current(Box::pin(publication.commit_metadata(orch)))
                .await
            {
                return false;
            }
            if let Some(frame) = &publication.frame {
                if !fence
                    .publish_if_current(orch.emit_tool_result_frame(
                        &publication.tool_use_id,
                        &frame.tool,
                        &frame.model_text,
                        &frame.result,
                        frame.denial_kind.as_deref(),
                    ))
                    .await
                {
                    return false;
                }
            }
        }
        fence.is_current()
    }
}

async fn publish_or_defer_tool_result(
    orch: &ConversationOrchestrator,
    fence: Option<&dyn HookPublicationGuard>,
    publications: &mut Vec<ToolResultPublication>,
    publication: ToolResultPublication,
) {
    if fence.is_some() {
        publications.push(publication);
    } else {
        publication.commit(orch).await;
    }
}

async fn finish_mod_result_stage(
    orch: &ConversationOrchestrator,
    fence: Option<&dyn HookPublicationGuard>,
    all: &mut DeferredToolDispatch,
    id: &ToolUseId,
    name: &str,
    stage: crate::conversation::ModResultStage,
    replacement: Option<(serde_json::Value, String)>,
) {
    if fence.is_some() {
        all.publications
            .push(stage.into_tool_result_publication(id, name, replacement));
    } else {
        orch.commit_mod_result_stage(id, name, stage, replacement)
            .await;
    }
}

struct ModCoreRun {
    dispatched: DeferredToolDispatch,
    stage: ModResultStage,
}

impl ConversationOrchestrator {
    /// Add a remote UI client before notifying Mods so
    /// `$.session.surfaces()` already contains the attached surface.
    pub async fn mod_ui_attach(
        &self,
        client_id: &str,
        surface: crate::config::ModRenderSurface,
    ) -> bool {
        match self
            .mod_surface_roster
            .attach(client_id.to_owned(), surface)
        {
            Ok(true) => {}
            Ok(false) => return false,
            Err(error) => {
                tracing::warn!(%error, "ignored invalid Mod UI attachment");
                return false;
            }
        }
        self.dispatch_mod_surface_event(
            "session.attach",
            serde_json::json!({"surface":surface.as_str(),"clientId":client_id}),
        )
        .await;
        true
    }

    /// Remove a known remote UI client before notifying Mods so
    /// `$.session.surfaces()` reflects the post-detach roster.
    pub async fn mod_ui_detach(
        &self,
        client_id: &str,
        reason: crate::mod_surface_roster::ModSurfaceDetachReason,
    ) -> bool {
        let Some(attachment) = self.mod_surface_roster.detach(client_id) else {
            return false;
        };
        self.dispatch_mod_surface_event(
            "session.detach",
            serde_json::json!({
                "surface":attachment.surface.as_str(),
                "clientId":attachment.client_id,
                "reason":reason.as_str(),
            }),
        )
        .await;
        true
    }

    /// Detach the clients still present in this orchestrator, one at a time.
    /// Each event sees the roster after its own client has been removed.
    pub async fn mod_ui_detach_all(
        &self,
        reason: crate::mod_surface_roster::ModSurfaceDetachReason,
    ) {
        let client_ids = self
            .mod_surface_roster
            .attachments()
            .into_iter()
            .map(|attachment| attachment.client_id)
            .collect::<Vec<_>>();
        for client_id in client_ids {
            self.mod_ui_detach(&client_id, reason).await;
        }
    }

    async fn dispatch_mod_surface_event(&self, event_name: &str, input: serde_json::Value) {
        let Some(registry) = &self.lifecycle_runtime.hook_registry else {
            return;
        };
        let Some(host) = registry.read().await.mod_host() else {
            return;
        };
        let log_output = self.output.clone();
        let toast_output = self.output.clone();
        let status_output = self.output.clone();
        if let Err(error) = host
            .dispatch_with_ui_at_session(
                event_name,
                input,
                self,
                |event| async move { Ok(serde_json::json!({"clientId":event["clientId"]})) },
                move |plugin, text| {
                    let output = log_output.clone();
                    async move { output.emit_mod_log(&plugin, &text).await }
                },
                move |plugin, text, timeout_ms| {
                    let output = toast_output.clone();
                    async move { output.emit_mod_toast(&plugin, &text, timeout_ms).await }
                },
                move |plugin, text| {
                    let output = status_output.clone();
                    async move { output.emit_mod_status(&plugin, text.as_deref()).await }
                },
            )
            .await
        {
            tracing::warn!(%error, event = event_name, "Mod session UI lifecycle dispatch failed");
        }
    }
}

#[async_trait::async_trait]
impl hooks::mods::ModSessionContext for ConversationOrchestrator {
    fn cwd(&self) -> std::path::PathBuf {
        self.current_cwd()
    }

    async fn agent_list(&self) -> Result<serde_json::Value, hooks::mods::ModError> {
        self.mod_agent_list().await
    }

    async fn agent_spawn_api(
        &self,
        input: hooks::mods::ModAgentSpawnInput,
        context: hooks::mods::ModAgentSpawnContext,
    ) -> Result<serde_json::Value, hooks::mods::ModError> {
        self.mod_agent_spawn(input, context).await
    }

    async fn ui_selection(&self) -> Result<Option<serde_json::Value>, hooks::mods::ModError> {
        Ok(self
            .mod_ui_selection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|selection| match selection.request_id.as_ref() {
                Some(request_id) => {
                    serde_json::json!({"text":selection.text,"requestId":request_id})
                }
                None => serde_json::json!({"text":selection.text}),
            }))
    }

    async fn model_fork(
        &self,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, hooks::mods::ModError> {
        self.mod_model_fork(input).await
    }

    async fn model_complete(
        &self,
        input: serde_json::Value,
        plugin: &str,
    ) -> Result<serde_json::Value, hooks::mods::ModError> {
        self.mod_model_complete(input, plugin, None).await
    }

    async fn model_classify(
        &self,
        input: lingxi_core::types::utf16_json::Utf16JsonProjection,
        plugin: &str,
    ) -> Result<lingxi_core::types::utf16_json::Utf16JsonProjection, hooks::mods::ModError> {
        self.mod_model_classify(input, plugin).await
    }

    async fn invalidate_prompt_section(&self) -> Result<(), hooks::mods::ModError> {
        let mut cache = self.prompt_runtime.mod_prompt_sections.lock().await;
        cache.generation = cache.generation.wrapping_add(1);
        cache.answers.clear();
        Ok(())
    }

    async fn invalidate_prompt_context(&self) -> Result<(), hooks::mods::ModError> {
        self.prompt_runtime.invalidate_mod_prompt_context().await;
        self.invalidate_instruction_context(
            lingxi_core::host::instructions::InstructionRefreshReason::HooksInvalidate,
        );
        Ok(())
    }

    async fn invalidate_prompt_attachment(&self) -> Result<(), hooks::mods::ModError> {
        self.prompt_runtime
            .invalidate_mod_prompt_attachments()
            .await;
        Ok(())
    }

    async fn prompt_attachment_generation(&self) -> u64 {
        self.prompt_runtime
            .mod_prompt_attachments
            .lock()
            .await
            .generation
    }

    async fn invalidate_tool_describe(&self) -> Result<(), hooks::mods::ModError> {
        self.prompt_runtime.invalidate_mod_tool_descriptions().await;
        Ok(())
    }

    async fn invalidate_command_describe(&self) -> Result<(), hooks::mods::ModError> {
        self.prompt_runtime
            .invalidate_mod_command_descriptions()
            .await;
        Ok(())
    }

    async fn prompt_compose_facts(
        &self,
        input: hooks::mods::ModUtf16ValueProjection,
    ) -> Result<hooks::mods::ModUtf16ValueProjection, hooks::mods::ModError> {
        self.mod_prompt_compose_facts(input).await
    }

    async fn prompt_compose_core(
        &self,
        facts: hooks::mods::ModUtf16ValueProjection,
        origin: Option<serde_json::Value>,
        skip_hook_id: Option<u64>,
    ) -> Result<hooks::mods::ModUtf16ValueProjection, hooks::mods::ModError> {
        self.mod_prompt_compose_core(facts, origin, skip_hook_id)
            .await
    }

    async fn settings_read(
        &self,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, hooks::mods::ModError> {
        let reader = self.mod_settings_reader.as_ref().ok_or_else(|| {
            hooks::mods::ModError::Unavailable(
                "settings.read needs a settings-aware session".into(),
            )
        })?;
        reader.read(input).await
    }

    async fn tool_list(&self) -> Result<serde_json::Value, hooks::mods::ModError> {
        let tools = self
            .build_wire_tools()
            .await
            .0
            .into_iter()
            .filter_map(|tool| {
                let name = tool.get("name")?.as_str()?;
                let description = tool
                    .get("description")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let mcp = self
                    .tools
                    .find_by_name(name)
                    .is_some_and(|registered| registered.is_mcp());
                Some(serde_json::json!({
                    "name": name,
                    "description": description,
                    "mcp": mcp,
                }))
            })
            .collect();
        Ok(serde_json::Value::Array(tools))
    }

    async fn projects_consent_facts(&self) -> hooks::mods::ProjectsConsentFacts {
        self.resolved_mod_projects_consent_facts().await
    }

    async fn prepare_tool_call(
        &self,
        plugin: &str,
        requested_tool_name: String,
        consent: Option<String>,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<hooks::mods::PreparedModToolCall, hooks::mods::ModError> {
        let Some(tool) = self.resolve_mod_tool_call_tool(&requested_tool_name).await else {
            return Err(hooks::mods::ModError::Hook(format!(
                "{plugin}: $.tool.call: no tool named \"{requested_tool_name}\" in this session"
            )));
        };
        // Native runs Uoe against the resolved canonical name only after the
        // current catalog has accepted the requested name. It does not need
        // Projects facts for a call that supplied no consent.
        let projects_consent =
            if consent.is_some() && matches!(tool.name(), "WebFetch" | "WebSearch") {
                self.resolved_mod_projects_consent_facts().await
            } else {
                hooks::mods::ProjectsConsentFacts::default()
            };
        if consent.is_some()
            && mod_tool_call_projects_consent_rejected(tool.name(), &projects_consent)?
        {
            return Err(hooks::mods::ModError::Hook(format!(
                "{plugin}: $.tool.call: {} does not accept `consent` in a Projects session. Call it without `consent`, and the normal permission check will decide.",
                tool.name()
            )));
        }
        Ok(hooks::mods::PreparedModToolCall::new(
            tool.name().to_owned(),
            consent,
            projects_consent,
            PreparedToolHandle(tool),
        ))
    }

    async fn tool_call(
        &self,
        plugin: &str,
        input: serde_json::Value,
        context: &hooks::mods::ModToolCallContext,
    ) -> Result<serde_json::Value, hooks::mods::ModError> {
        dispatch_mod_session_tool_call(self, None, plugin, input, context).await
    }

    async fn project_tool_call_api_result(
        &self,
        input: serde_json::Value,
        accepted_answer: serde_json::Value,
        context: &hooks::mods::ModToolCallContext,
    ) -> Result<serde_json::Value, hooks::mods::ModError> {
        if input.get("tool").and_then(serde_json::Value::as_str) != Some(AGENT_TOOL_NAME) {
            return Ok(accepted_answer);
        }
        let Some(raw_result) = accepted_answer.get("result") else {
            return Ok(accepted_answer);
        };
        if raw_result.get("status").and_then(serde_json::Value::as_str) != Some("async_launched") {
            return Ok(accepted_answer);
        }
        let Some(agent_id) = raw_result
            .get("agentId")
            .and_then(serde_json::Value::as_str)
        else {
            return Ok(accepted_answer);
        };
        let plugin = mod_tool_call_hook_caller(context)?;
        let Some(registry) = self.task_registry.as_ref() else {
            return Ok(mod_tool_call_agent_error(format!(
                "{plugin}: $.agent.spawn: no answer within 10 minutes"
            )));
        };
        let outcome = registry
            .wait_for_agent_terminal(
                agent_id,
                context.cancellation.clone(),
                Some(std::time::Duration::from_secs(10 * 60)),
            )
            .await
            .map_err(|error| {
                hooks::mods::ModError::Unavailable(format!(
                    "{plugin}: $.agent.spawn: cannot wait for Agent {agent_id}: {error}"
                ))
            })?;

        use lingxi_core::host::task_registry::{AgentTerminalWaitOutcome, AgentTerminalWaitReason};
        match outcome {
            AgentTerminalWaitOutcome::Completed(snapshot) => Ok(serde_json::json!({
                "result": mod_tool_call_agent_result(agent_id, raw_result),
                "text": snapshot.native_transcript_text,
            })),
            AgentTerminalWaitOutcome::Failed(snapshot) => {
                Ok(mod_tool_call_agent_error(mod_tool_call_agent_failure_text(
                    &plugin,
                    "failed",
                    snapshot
                        .error
                        .as_deref()
                        .or(Some(snapshot.native_transcript_text.as_str())),
                )))
            }
            AgentTerminalWaitOutcome::Killed(snapshot) => {
                Ok(mod_tool_call_agent_error(mod_tool_call_agent_failure_text(
                    &plugin,
                    "killed",
                    snapshot
                        .error
                        .as_deref()
                        .or(Some(snapshot.native_transcript_text.as_str())),
                )))
            }
            AgentTerminalWaitOutcome::Interrupted { reason, .. } => {
                let text = match reason {
                    AgentTerminalWaitReason::Aborted => {
                        format!("{plugin}: $.agent.spawn aborted")
                    }
                    AgentTerminalWaitReason::StartupTimeout
                    | AgentTerminalWaitReason::SettleTimeout => {
                        format!("{plugin}: $.agent.spawn: no answer within 10 minutes")
                    }
                };
                Ok(mod_tool_call_agent_error(text))
            }
            AgentTerminalWaitOutcome::Evicted {
                native_transcript_text: Some(text),
            } if !text.is_empty() => Ok(serde_json::json!({
                "result": mod_tool_call_agent_result(agent_id, raw_result),
                "text": text,
            })),
            AgentTerminalWaitOutcome::Evicted { .. } => Ok(mod_tool_call_agent_error(format!(
                "{plugin}: $.agent.spawn: the subagent's record was evicted before its answer was read"
            ))),
        }
    }

    async fn command_list(&self) -> Result<serde_json::Value, hooks::mods::ModError> {
        let catalog = self.mod_command_catalog.as_ref().ok_or_else(|| {
            hooks::mods::ModError::Unavailable("command.list needs a command-aware session".into())
        })?;
        catalog.list().await
    }

    async fn command_register(
        &self,
        plugin: &str,
        spec: serde_json::Value,
    ) -> Result<serde_json::Value, hooks::mods::ModError> {
        let catalog = self.mod_command_catalog.as_ref().ok_or_else(|| {
            hooks::mods::ModError::Unavailable(
                "command.register needs a command-aware session".into(),
            )
        })?;
        catalog.register(plugin, spec).await
    }

    async fn command_run(
        &self,
        plugin: &str,
        command: &str,
        args: &str,
    ) -> Result<serde_json::Value, hooks::mods::ModError> {
        let catalog = self.mod_command_catalog.as_ref().ok_or_else(|| {
            hooks::mods::ModError::Unavailable("command.run needs a command-aware session".into())
        })?;
        catalog.run(plugin, command, args).await
    }

    async fn prompt_submit(
        &self,
        plugin: &str,
        text: &str,
        as_user: bool,
    ) -> Result<serde_json::Value, hooks::mods::ModError> {
        let catalog = self.mod_command_catalog.as_ref().ok_or_else(|| {
            hooks::mods::ModError::Unavailable("prompt.submit needs a prompt queue".into())
        })?;
        catalog.submit_prompt(plugin, text, as_user).await
    }

    async fn command_unregister_plugin(&self, plugin: &str) {
        if let Some(catalog) = &self.mod_command_catalog {
            catalog.unregister_plugin(plugin).await;
        }
    }

    async fn tool_register(
        &self,
        plugin: &str,
        spec: serde_json::Value,
    ) -> Result<serde_json::Value, hooks::mods::ModError> {
        let name = spec
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                hooks::mods::ModError::Hook("tool.register name must be a string".into())
            })?;
        if name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return Err(hooks::mods::ModError::Hook(
                "tool.register name uses letters, digits, _, or - and has at most 64 characters"
                    .into(),
            ));
        }
        if self.tools.mod_registration_disabled() {
            return Err(hooks::mods::ModError::Hook(format!(
                "{plugin}: $.tool.register: \"{name}\" refused: this session has no built-in tools (--tools \"\"), so it takes none from plugins"
            )));
        }
        let description = spec
            .get("description")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                hooks::mods::ModError::Hook("tool.register description must be a string".into())
            })?;
        let schema = spec
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({"type":"object"}));
        if !schema.is_object() {
            return Err(hooks::mods::ModError::Hook(
                "tool.register inputSchema must be an object".into(),
            ));
        }
        let full_name = format!("mcp__{plugin}__{name}");
        if self.tools.find_registered(&full_name).is_some()
            && !self.tools.has_mod_tool(plugin, &full_name)
        {
            return Err(hooks::mods::ModError::Hook(format!(
                "tool.register would shadow an existing tool: {full_name}"
            )));
        }
        self.tools.register_mod_tool(
            plugin,
            std::sync::Arc::new(ModRegisteredTool {
                name: full_name.clone(),
                description: description.to_owned(),
                schema,
            }),
        );
        Ok(serde_json::json!({"tool": full_name}))
    }

    fn tool_unregister_plugin(&self, plugin: &str) {
        self.tools.unregister_mod_tools(plugin);
    }

    async fn messages(
        &self,
        input: serde_json::Value,
    ) -> Result<hooks::mods::ModUtf16ValueProjection, hooks::mods::ModError> {
        let Some(args) = input.as_object() else {
            return Err(hooks::mods::ModError::Hook(
                "session.messages takes { agentId, as } or nothing".into(),
            ));
        };
        if args.keys().any(|key| key != "agentId" && key != "as") {
            return Err(hooks::mods::ModError::Hook(
                "session.messages takes { agentId, as } or nothing".into(),
            ));
        }
        if let Some(agent_id) = args.get("agentId") {
            let Some(agent_id) = agent_id.as_str().filter(|id| !id.is_empty()) else {
                return Err(hooks::mods::ModError::Hook(
                    "session.messages agentId must be a non-empty string".into(),
                ));
            };
            return Err(hooks::mods::ModError::Hook(format!(
                "no conversation of agent {agent_id} in this session: not one of its agents, running in another process, or finished with no saved transcript this session reads back"
            )));
        }
        let as_api = match args.get("as") {
            None => false,
            Some(serde_json::Value::String(format)) if format == "api" => true,
            _ => {
                return Err(hooks::mods::ModError::Hook(
                    "session.messages as must be api or absent".into(),
                ));
            }
        };
        if as_api {
            let history = self.session.lock().await.history.clone();
            return super::mod_session_messages::api_projection(history)
                .map_err(hooks::mods::ModError::Hook);
        }
        let (mut projection, session_id) = {
            let session = self.session.lock().await;
            (
                super::mod_session_messages::summarize_projection(&session.history),
                session.session_id,
            )
        };
        let path = self
            .transcript
            .jsonl_writer
            .as_ref()
            .map(|writer| writer.active_path().to_path_buf())
            .unwrap_or_else(|| self.computed_transcript_path(&session_id));
        super::mod_session_messages::hydrate_results(&path, &mut projection.value).await;
        Ok(projection)
    }

    async fn usage(
        &self,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, hooks::mods::ModError> {
        let Some(args) = input.as_object() else {
            return Err(hooks::mods::ModError::Hook(
                "session.usage takes { breakdown, columns } or nothing".into(),
            ));
        };
        let extra = args
            .keys()
            .filter(|key| key.as_str() != "breakdown" && key.as_str() != "columns")
            .cloned()
            .collect::<Vec<_>>();
        if !extra.is_empty() {
            return Err(hooks::mods::ModError::Hook(format!(
                "session.usage takes {{ breakdown, columns }} or nothing (not {})",
                extra.join(", ")
            )));
        }
        let breakdown = match args.get("breakdown") {
            None => None,
            Some(serde_json::Value::String(detail))
                if matches!(detail.as_str(), "summary" | "full") =>
            {
                Some(detail.as_str())
            }
            Some(detail) => {
                return Err(hooks::mods::ModError::Hook(format!(
                    "session.usage takes breakdown \"summary\" or \"full\" (got {})",
                    session_usage_js_string(detail)
                )));
            }
        };
        let columns = match args.get("columns") {
            None => None,
            Some(serde_json::Value::Number(columns))
                if columns.as_f64().is_some_and(|value| {
                    value.is_finite() && value > 0.0 && value.fract() == 0.0
                }) =>
            {
                Some(columns.clone())
            }
            Some(columns) => {
                return Err(hooks::mods::ModError::Hook(format!(
                    "session.usage takes columns, a positive whole number (got {})",
                    session_usage_js_string(columns)
                )));
            }
        };
        self.mod_session_usage_snapshot(breakdown, columns).await
    }

    async fn fs_ancestors(
        &self,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, hooks::mods::ModError> {
        self.fs_ancestors_at(input, &self.cwd).await
    }

    async fn fs_ancestors_at(
        &self,
        input: serde_json::Value,
        cwd: &std::path::Path,
    ) -> Result<serde_json::Value, hooks::mods::ModError> {
        let approved = migrations::global_config::global_config_path().is_some_and(|path| {
            migrations::global_config::check_has_lingxi_md_external_includes_approved(&path, cwd)
        });
        super::mod_fs_ancestors::run(input, cwd.to_path_buf(), approved, self.memory.excluder())
            .await
    }

    fn root(&self) -> std::path::PathBuf {
        // Claude's session root follows host /cd and worktree swaps. The
        // configured project_root is fixed at composition and would go stale.
        self.session_cwd.cwd()
    }

    fn surfaces(&self) -> Vec<String> {
        self.mod_prompt_surfaces(self.prompt_is_interactive())
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    async fn model(&self) -> String {
        self.session.lock().await.model.clone()
    }

    async fn id(&self) -> String {
        self.session.lock().await.session_id.to_string()
    }

    async fn turns(&self) -> u64 {
        self.session.lock().await.real_user_turns()
    }

    async fn version(&self) -> Result<serde_json::Value, hooks::mods::ModError> {
        let version = env!("CARGO_PKG_VERSION");
        let base = version.split_once('+').map_or(version, |(base, _)| base);
        Ok(serde_json::json!({"version":version,"base":base}))
    }

    async fn emit_mod_log(&self, plugin: &str, text: &str) {
        self.output.emit_mod_log(plugin, text).await;
    }

    async fn emit_mod_toast(&self, plugin: &str, text: &str, timeout_ms: u64) {
        self.output.emit_mod_toast(plugin, text, timeout_ms).await;
    }

    async fn emit_mod_status(&self, plugin: &str, text: Option<&str>) {
        self.output.emit_mod_status(plugin, text).await;
    }

    async fn emit_mod_ui_client_frame(&self, runtime_id: &str, frame_json: &str) {
        self.output
            .emit_mod_ui_client_frame(runtime_id, frame_json)
            .await;
    }

    async fn emit_mod_ui_invalidate(
        &self,
        instances_json: Option<&str>,
        uuid: &str,
        session_id: &str,
    ) {
        self.output
            .emit_mod_ui_invalidate(instances_json, uuid, session_id)
            .await;
    }

    async fn tool_check(
        &self,
        name: &str,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, hooks::mods::ModError> {
        use lingxi_core::host::permission_gate::{ModToolCheckDecision, PermissionCheckContext};

        let tool = self.find_tool_for_dispatch(name).ok_or_else(|| {
            hooks::mods::ModError::Hook(format!("No such tool available: {name}"))
        })?;
        let plan_mode = self.session.lock().await.plan_mode;
        let tool_check_ceiling = tool.tool_check_permission_ceiling(&input).await;
        let policy_ctx = PermissionCheckContext {
            tool_check_ceiling,
            requires_user_interaction: tool.requires_user_interaction(),
            suppress_always_allow_rule: tool.requires_user_interaction(),
            mode_override: plan_mode.then(|| "plan".to_string()),
            ..Default::default()
        };
        let evaluation = self
            .perms
            .check_mod_query(name, &input, &policy_ctx)
            .await
            .ok_or_else(|| {
                hooks::mods::ModError::Unavailable(
                    "permission gate cannot answer a declarative Mod query".into(),
                )
            })?;
        let mut verdict = evaluation.verdict;

        let (messages, model, model_profile) = {
            let session = self.session.lock().await;
            (
                session.model_context_history(),
                session.model.clone(),
                session.model_profile.clone(),
            )
        };
        let context = ToolUseContext {
            input_projection: None,
            options: ToolUseOptions {
                debug: false,
                verbose: false,
                main_loop_model: model,
                model_profile,
                max_budget_nano_usd: None,
                mcp_clients: Vec::new(),
                is_non_interactive_session: !self.config.interactive_permissions,
                custom_system_prompt: self.config.system_prompt_override.clone(),
                append_system_prompt: None,
            },
            messages,
            tool_use_id: None,
            assistant_message_id: None,
            assistant_message: None,
            same_turn_tool_uses: Vec::new(),
            agent_id: None,
            agent_spawn_provenance:
                lingxi_core::host::subagent_spawn::AgentSpawnProvenance::default(),
            nested_memory_triggers: self.prompt_runtime.nested_memory_triggers.clone(),
            agent_name: None,
            team_name: None,
            origin_session_id: None,
            tool_execution_policy: lingxi_core::host::tool_invoker::ToolExecutionPolicy::Ordinary,
            trusted_effective_permission_mode: None,
            classifier_only_review: None,
            instruction_context: Some(self.instruction_context_snapshot().await),
            content_replacement_state: None,
            session: Some(self.session.clone()),
            observer_pairings: self.model_runtime.observer_pairings.clone(),
            subagent_registry: Some(self.tools.clone()),
            cancel: None,
            fork_parent_system_prompt: None,
            cwd: None,
            depth: 0,
            observer: None,
            file_history: self
                .file_history
                .clone()
                .map(|history| history as std::sync::Arc<dyn lingxi_core::host::FileHistorySink>),
        };
        let own = tool.check_permissions(&input, &context).await;
        match &own {
            permission::PermissionResult::Deny {
                reason,
                explanation,
                ..
            } => {
                let resolved =
                    tool_permission_deny_resolution(name, reason, explanation.as_deref());
                let PermissionResolution::Deny { reason, .. } = resolved else {
                    unreachable!()
                };
                verdict.decision = ModToolCheckDecision::Deny;
                verdict.reason = Some(reason);
                verdict.rule = None;
            }
            permission::PermissionResult::Ask { prompt, .. }
                if tool_permission_ask_is_protected(tool.as_ref(), &own)
                    || (verdict.decision == ModToolCheckDecision::Allow
                        && verdict.rule.is_none()
                        && self.permission_mode().as_deref() != Some("bypassPermissions")) =>
            {
                verdict.decision = ModToolCheckDecision::Ask;
                verdict.reason = Some(prompt.message.clone());
            }
            _ => {}
        }
        let decision = match verdict.decision {
            ModToolCheckDecision::Allow => "allow",
            ModToolCheckDecision::Ask => "ask",
            ModToolCheckDecision::Deny => "deny",
        };
        let mut result = serde_json::Map::new();
        result.insert("decision".into(), serde_json::json!(decision));
        if let Some(reason) = verdict.reason {
            result.insert("reason".into(), serde_json::json!(reason));
        }
        if let Some(rule) = verdict.rule {
            result.insert("rule".into(), serde_json::json!(rule));
        }
        if let Some(ceiling) = mod_tool_check_ceiling_value(evaluation.ceiling) {
            result.insert("ceiling".into(), ceiling);
        }
        Ok(serde_json::Value::Object(result))
    }
}

async fn dispatch_mod_session_tool_call(
    orch: &ConversationOrchestrator,
    publication_guard: Option<std::sync::Arc<dyn HookPublicationGuard>>,
    plugin: &str,
    input: serde_json::Value,
    context: &hooks::mods::ModToolCallContext,
) -> Result<serde_json::Value, hooks::mods::ModError> {
    if publication_guard
        .as_ref()
        .is_some_and(|fence| !fence.is_current())
    {
        return Err(hooks::mods::ModError::Unavailable(
            "tool.call generation was discarded".into(),
        ));
    }

    let tool_name = input
        .get("tool")
        .and_then(serde_json::Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            hooks::mods::ModError::Hook(format!(
                "{plugin}: $.tool.call takes the event's input: {{ tool, ...args }}"
            ))
        })?;
    let consent = match input.get("consent") {
        None => None,
        Some(serde_json::Value::String(consent)) => Some(consent.clone()),
        Some(_) => {
            return Err(hooks::mods::ModError::Hook(format!(
                "{plugin}: $.tool.call: consent, when given, is a string"
            )));
        }
    };
    let prepared = context.prepared.as_ref();
    let Some(PreparedToolHandle(tool)) = prepared.resolved_tool::<PreparedToolHandle>() else {
        return Err(hooks::mods::ModError::Protocol(
            "tool.call transaction has no prepared tool handle".into(),
        ));
    };
    if prepared.canonical_tool_name() != tool.name()
        || (tool_name != tool.name() && !tool.aliases().contains(&tool_name))
        || prepared.consent() != consent.as_deref()
    {
        return Err(hooks::mods::ModError::Protocol(
            "tool.call input does not match its prepared catalog entry".into(),
        ));
    }
    let consent = prepared.consent().map(str::to_owned);
    let input_args = mod_tool_call_arguments(&input);
    let tool_use_id = ToolUseId::from(context.virtual_tool_use_id.clone());
    let assistant_message_id = MessageId::parse_prefixed(&context.virtual_assistant_uuid)
        .ok_or_else(|| {
            hooks::mods::ModError::Protocol(
                "tool.call transaction has an invalid virtual assistant UUID".into(),
            )
        })?;
    let tool_uses = vec![(
        tool_use_id.clone(),
        prepared.canonical_tool_name().to_owned(),
        serde_json::Value::Object(input_args),
        None,
    )];
    let cancellation = context.cancellation.clone();
    let (dispatch, stage) = with_virtual_mod_result_stage(
        &tool_use_id,
        dispatch_tool_uses_tracked_deferred_core_with_tool(
            orch,
            &tool_uses,
            Some(cancellation),
            Some(assistant_message_id),
            1,
            consent,
            Some(context.agent_spawn_provenance.clone()),
            std::sync::Arc::clone(tool),
            publication_guard,
        ),
    )
    .await;
    let dispatch = dispatch.map_err(|error| {
        hooks::mods::ModError::Hook(format!(
            "{plugin}: $.tool.call({}) failed: {error}",
            tool.name()
        ))
    })?;
    let Some(ContentBlock::ToolResult {
    content,
    is_error,
    ..
}) = dispatch.results.iter().find(|block| {
    matches!(block, ContentBlock::ToolResult { tool_use_id: result_id, .. } if result_id == &tool_use_id)
})
else {
    return Err(hooks::mods::ModError::Hook(format!(
        "{plugin}: $.tool.call({}) produced no result",
        tool.name()
    )));
};
    let content = content.clone();
    if stage.denial_kind().is_some() {
        let reason = content.strip_prefix("<tool_use_error>").unwrap_or(&content);
        let reason = reason.strip_suffix("</tool_use_error>").unwrap_or(reason);
        return Ok(serde_json::json!({"deny":reason}));
    }
    let mut result = serde_json::Map::new();
    result.insert(
        "result".into(),
        stage
            .tool_use_result
            .unwrap_or_else(|| serde_json::Value::String(content.clone())),
    );
    result.insert("text".into(), serde_json::Value::String(content));
    if is_error.unwrap_or(false) {
        result.insert("isError".into(), serde_json::Value::Bool(true));
    }
    Ok(serde_json::Value::Object(result))
}

/// Per-W1 Mod session view. A Mod worker can issue session-backed UI operations
/// in addition to the per-dispatch log/toast/status callbacks, so the session
/// itself must carry the owning generation fence as well.
#[derive(Clone)]
struct GenerationBoundModSessionContext {
    inner: std::sync::Arc<ConversationOrchestrator>,
    publication_guard: std::sync::Arc<dyn HookPublicationGuard>,
}

pub(crate) fn generation_bound_mod_session_context(
    orch: &ConversationOrchestrator,
    publication_guard: std::sync::Arc<dyn HookPublicationGuard>,
) -> Option<std::sync::Arc<dyn hooks::mods::ModSessionContext>> {
    let inner = orch.upgrade_streaming_tool_dispatch_owner()?;
    Some(std::sync::Arc::new(GenerationBoundModSessionContext {
        inner,
        publication_guard,
    }))
}

#[async_trait::async_trait]
impl hooks::mods::ModSessionContext for GenerationBoundModSessionContext {
    fn cwd(&self) -> std::path::PathBuf {
        hooks::mods::ModSessionContext::cwd(self.inner.as_ref())
    }

    fn root(&self) -> std::path::PathBuf {
        hooks::mods::ModSessionContext::root(self.inner.as_ref())
    }

    async fn projects_consent_facts(&self) -> hooks::mods::ProjectsConsentFacts {
        hooks::mods::ModSessionContext::projects_consent_facts(self.inner.as_ref()).await
    }

    fn surfaces(&self) -> Vec<String> {
        hooks::mods::ModSessionContext::surfaces(self.inner.as_ref())
    }

    async fn ui_selection(&self) -> Result<Option<serde_json::Value>, hooks::mods::ModError> {
        hooks::mods::ModSessionContext::ui_selection(self.inner.as_ref()).await
    }

    fn ui_invalidation_context(
        &self,
    ) -> Option<std::sync::Arc<dyn hooks::mods::ModSessionContext>> {
        Some(std::sync::Arc::new(self.clone()))
    }

    fn generation_cancellation_token(&self) -> Option<lingxi_core::host::CancellationToken> {
        self.publication_guard.generation_cancellation_token()
    }

    async fn emit_mod_ui_client_frame(&self, runtime_id: &str, frame_json: &str) {
        self.publication_guard
            .publish_if_current(
                self.inner
                    .output
                    .emit_mod_ui_client_frame(runtime_id, frame_json),
            )
            .await;
    }

    async fn emit_mod_ui_invalidate(
        &self,
        instances_json: Option<&str>,
        uuid: &str,
        session_id: &str,
    ) {
        self.publication_guard
            .publish_if_current(self.inner.output.emit_mod_ui_invalidate(
                instances_json,
                uuid,
                session_id,
            ))
            .await;
    }

    async fn messages(
        &self,
        input: serde_json::Value,
    ) -> Result<hooks::mods::ModUtf16ValueProjection, hooks::mods::ModError> {
        hooks::mods::ModSessionContext::messages(self.inner.as_ref(), input).await
    }

    async fn usage(
        &self,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, hooks::mods::ModError> {
        hooks::mods::ModSessionContext::usage(self.inner.as_ref(), input).await
    }

    async fn model_fork(
        &self,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, hooks::mods::ModError> {
        hooks::mods::ModSessionContext::model_fork(self.inner.as_ref(), input).await
    }

    async fn model_complete(
        &self,
        input: serde_json::Value,
        plugin: &str,
    ) -> Result<serde_json::Value, hooks::mods::ModError> {
        hooks::mods::ModSessionContext::model_complete(self.inner.as_ref(), input, plugin).await
    }

    async fn model_classify(
        &self,
        input: lingxi_core::types::utf16_json::Utf16JsonProjection,
        plugin: &str,
    ) -> Result<lingxi_core::types::utf16_json::Utf16JsonProjection, hooks::mods::ModError> {
        hooks::mods::ModSessionContext::model_classify(self.inner.as_ref(), input, plugin).await
    }

    async fn fs_ancestors(
        &self,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, hooks::mods::ModError> {
        hooks::mods::ModSessionContext::fs_ancestors(self.inner.as_ref(), input).await
    }

    async fn fs_ancestors_at(
        &self,
        input: serde_json::Value,
        cwd: &std::path::Path,
    ) -> Result<serde_json::Value, hooks::mods::ModError> {
        hooks::mods::ModSessionContext::fs_ancestors_at(self.inner.as_ref(), input, cwd).await
    }

    async fn settings_read(
        &self,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, hooks::mods::ModError> {
        hooks::mods::ModSessionContext::settings_read(self.inner.as_ref(), input).await
    }

    async fn invalidate_prompt_section(&self) -> Result<(), hooks::mods::ModError> {
        hooks::mods::ModSessionContext::invalidate_prompt_section(self.inner.as_ref()).await
    }

    async fn invalidate_prompt_context(&self) -> Result<(), hooks::mods::ModError> {
        hooks::mods::ModSessionContext::invalidate_prompt_context(self.inner.as_ref()).await
    }

    async fn invalidate_prompt_attachment(&self) -> Result<(), hooks::mods::ModError> {
        hooks::mods::ModSessionContext::invalidate_prompt_attachment(self.inner.as_ref()).await
    }

    async fn prompt_attachment_generation(&self) -> u64 {
        hooks::mods::ModSessionContext::prompt_attachment_generation(self.inner.as_ref()).await
    }

    async fn invalidate_tool_describe(&self) -> Result<(), hooks::mods::ModError> {
        hooks::mods::ModSessionContext::invalidate_tool_describe(self.inner.as_ref()).await
    }

    async fn invalidate_command_describe(&self) -> Result<(), hooks::mods::ModError> {
        hooks::mods::ModSessionContext::invalidate_command_describe(self.inner.as_ref()).await
    }

    async fn prompt_compose_facts(
        &self,
        input: hooks::mods::ModUtf16ValueProjection,
    ) -> Result<hooks::mods::ModUtf16ValueProjection, hooks::mods::ModError> {
        hooks::mods::ModSessionContext::prompt_compose_facts(self.inner.as_ref(), input).await
    }

    async fn prompt_compose_core(
        &self,
        facts: hooks::mods::ModUtf16ValueProjection,
        origin: Option<serde_json::Value>,
        skip_hook_id: Option<u64>,
    ) -> Result<hooks::mods::ModUtf16ValueProjection, hooks::mods::ModError> {
        hooks::mods::ModSessionContext::prompt_compose_core(
            self.inner.as_ref(),
            facts,
            origin,
            skip_hook_id,
        )
        .await
    }

    async fn tool_list(&self) -> Result<serde_json::Value, hooks::mods::ModError> {
        hooks::mods::ModSessionContext::tool_list(self.inner.as_ref()).await
    }

    async fn agent_list(&self) -> Result<serde_json::Value, hooks::mods::ModError> {
        hooks::mods::ModSessionContext::agent_list(self.inner.as_ref()).await
    }

    async fn agent_spawn_api(
        &self,
        input: hooks::mods::ModAgentSpawnInput,
        context: hooks::mods::ModAgentSpawnContext,
    ) -> Result<serde_json::Value, hooks::mods::ModError> {
        hooks::mods::ModSessionContext::agent_spawn_api(self.inner.as_ref(), input, context).await
    }

    async fn prepare_tool_call(
        &self,
        plugin: &str,
        requested_tool_name: String,
        consent: Option<String>,
        cancellation: lingxi_core::host::CancellationToken,
    ) -> Result<hooks::mods::PreparedModToolCall, hooks::mods::ModError> {
        hooks::mods::ModSessionContext::prepare_tool_call(
            self.inner.as_ref(),
            plugin,
            requested_tool_name,
            consent,
            cancellation,
        )
        .await
    }

    async fn tool_call(
        &self,
        plugin: &str,
        input: serde_json::Value,
        context: &hooks::mods::ModToolCallContext,
    ) -> Result<serde_json::Value, hooks::mods::ModError> {
        dispatch_mod_session_tool_call(
            self.inner.as_ref(),
            Some(std::sync::Arc::clone(&self.publication_guard)),
            plugin,
            input,
            context,
        )
        .await
    }

    async fn project_tool_call_api_result(
        &self,
        input: serde_json::Value,
        accepted_answer: serde_json::Value,
        context: &hooks::mods::ModToolCallContext,
    ) -> Result<serde_json::Value, hooks::mods::ModError> {
        hooks::mods::ModSessionContext::project_tool_call_api_result(
            self.inner.as_ref(),
            input,
            accepted_answer,
            context,
        )
        .await
    }

    async fn complete_tool_call(
        &self,
        transaction_id: hooks::mods::ModToolCallTransactionId,
    ) -> Result<(), hooks::mods::ModError> {
        hooks::mods::ModSessionContext::complete_tool_call(self.inner.as_ref(), transaction_id)
            .await
    }

    async fn abort_tool_call(
        &self,
        transaction_id: hooks::mods::ModToolCallTransactionId,
    ) -> Result<(), hooks::mods::ModError> {
        hooks::mods::ModSessionContext::abort_tool_call(self.inner.as_ref(), transaction_id).await
    }

    async fn command_list(&self) -> Result<serde_json::Value, hooks::mods::ModError> {
        hooks::mods::ModSessionContext::command_list(self.inner.as_ref()).await
    }

    async fn command_register(
        &self,
        plugin: &str,
        spec: serde_json::Value,
    ) -> Result<serde_json::Value, hooks::mods::ModError> {
        hooks::mods::ModSessionContext::command_register(self.inner.as_ref(), plugin, spec).await
    }

    async fn command_run(
        &self,
        plugin: &str,
        command: &str,
        args: &str,
    ) -> Result<serde_json::Value, hooks::mods::ModError> {
        hooks::mods::ModSessionContext::command_run(self.inner.as_ref(), plugin, command, args)
            .await
    }

    async fn prompt_submit(
        &self,
        plugin: &str,
        text: &str,
        as_user: bool,
    ) -> Result<serde_json::Value, hooks::mods::ModError> {
        hooks::mods::ModSessionContext::prompt_submit(self.inner.as_ref(), plugin, text, as_user)
            .await
    }

    async fn command_unregister_plugin(&self, plugin: &str) {
        hooks::mods::ModSessionContext::command_unregister_plugin(self.inner.as_ref(), plugin).await
    }

    async fn tool_register(
        &self,
        plugin: &str,
        spec: serde_json::Value,
    ) -> Result<serde_json::Value, hooks::mods::ModError> {
        hooks::mods::ModSessionContext::tool_register(self.inner.as_ref(), plugin, spec).await
    }

    fn tool_unregister_plugin(&self, plugin: &str) {
        hooks::mods::ModSessionContext::tool_unregister_plugin(self.inner.as_ref(), plugin)
    }

    async fn model(&self) -> String {
        hooks::mods::ModSessionContext::model(self.inner.as_ref()).await
    }

    async fn id(&self) -> String {
        hooks::mods::ModSessionContext::id(self.inner.as_ref()).await
    }

    async fn turns(&self) -> u64 {
        hooks::mods::ModSessionContext::turns(self.inner.as_ref()).await
    }

    async fn version(&self) -> Result<serde_json::Value, hooks::mods::ModError> {
        hooks::mods::ModSessionContext::version(self.inner.as_ref()).await
    }

    async fn tool_check(
        &self,
        tool: &str,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, hooks::mods::ModError> {
        hooks::mods::ModSessionContext::tool_check(self.inner.as_ref(), tool, input).await
    }

    async fn emit_mod_log(&self, plugin: &str, text: &str) {
        self.publication_guard
            .publish_if_current(self.inner.output.emit_mod_log(plugin, text))
            .await;
    }

    async fn emit_mod_toast(&self, plugin: &str, text: &str, timeout_ms: u64) {
        self.publication_guard
            .publish_if_current(self.inner.output.emit_mod_toast(plugin, text, timeout_ms))
            .await;
    }

    async fn emit_mod_status(&self, plugin: &str, text: Option<&str>) {
        self.publication_guard
            .publish_if_current(self.inner.output.emit_mod_status(plugin, text))
            .await;
    }
}

async fn apply_mod_tool_check(
    orch: &ConversationOrchestrator,
    name: &str,
    input: &serde_json::Value,
    tool_use_id: &ToolUseId,
    resolution: PermissionResolution,
    tool_permission_result: Option<&permission::PermissionResult>,
    captured_evaluation: Option<&lingxi_core::host::permission_gate::PermissionToolCheckEvaluation>,
    restricted_protected_mutation: bool,
    agent_id: Option<String>,
    tool_check_ceiling: Option<lingxi_core::host::McpPermissionCeiling>,
    publication_fence: Option<std::sync::Arc<dyn HookPublicationGuard>>,
) -> PermissionResolution {
    let Some(registry) = &orch.lifecycle_runtime.hook_registry else {
        return resolution;
    };
    let Some(host) = registry.read().await.mod_host() else {
        return resolution;
    };
    let original_decision = match resolution {
        PermissionResolution::Allow { .. } => "allow",
        PermissionResolution::Deny { .. } => "deny",
        PermissionResolution::Ask | PermissionResolution::AskWithContext { .. } => "ask",
    };
    let captured_evaluation = captured_evaluation.filter(|evaluation| {
        matches!(
            (original_decision, evaluation.verdict.decision),
            (
                "allow",
                lingxi_core::host::permission_gate::ModToolCheckDecision::Allow
            ) | (
                "ask",
                lingxi_core::host::permission_gate::ModToolCheckDecision::Ask
            ) | (
                "deny",
                lingxi_core::host::permission_gate::ModToolCheckDecision::Deny
            )
        )
    });
    let rule = captured_evaluation.and_then(|evaluation| evaluation.verdict.rule.clone());
    let captured_reason =
        captured_evaluation.and_then(|evaluation| evaluation.verdict.reason.clone());
    let tool_reason = || match tool_permission_result {
        Some(permission::PermissionResult::Ask { prompt, .. }) => Some(prompt.message.clone()),
        _ => None,
    };
    let reason = match &resolution {
        PermissionResolution::Deny { reason, .. } => Some(reason.clone()),
        PermissionResolution::Ask => tool_reason().or(captured_reason),
        PermissionResolution::AskWithContext {
            decision_reason, ..
        } => tool_reason()
            .or_else(|| decision_reason.clone())
            .or(captured_reason),
        PermissionResolution::Allow { .. } => None,
    };
    let mut core = serde_json::Map::new();
    core.insert("decision".into(), serde_json::json!(original_decision));
    if let Some(ceiling) = mod_tool_check_ceiling_value(tool_check_ceiling) {
        core.insert("ceiling".into(), ceiling);
    }
    if let Some(reason) = reason {
        core.insert("reason".into(), serde_json::json!(reason));
    }
    if let Some(rule) = rule {
        core.insert("rule".into(), serde_json::json!(rule));
    }
    let core = serde_json::Value::Object(core);
    let event = mod_tool_check_event(
        name,
        input,
        tool_use_id,
        agent_id.as_deref(),
        tool_check_ceiling,
    );
    let expected = event.clone();
    let mod_log_output = orch.output.clone();
    let mod_toast_output = orch.output.clone();
    let mod_status_output = orch.output.clone();
    let mod_log_fence = publication_fence.clone();
    let mod_toast_fence = publication_fence.clone();
    let mod_status_fence = publication_fence.clone();
    let generation_bound_session =
        publication_fence
            .clone()
            .map(|publication_guard| GenerationBoundModSessionContext {
                inner: orch
                    .upgrade_streaming_tool_dispatch_owner()
                    .expect("streaming tool.check uses its bound orchestrator owner"),
                publication_guard,
            });
    let session: &dyn hooks::mods::ModSessionContext = generation_bound_session
        .as_ref()
        .map_or(orch, |session| session);
    let checked = host
        .dispatch_with_ui_meta_at_session(
            "tool.check",
            event,
            session,
            move |forwarded| {
                let expected = expected.clone();
                let core = core.clone();
                async move {
                    if forwarded != expected {
                        return Err(hooks::mods::ModError::Hook(
                            "tool.check identity is pinned".into(),
                        ));
                    }
                    Ok(core)
                }
            },
            move |plugin, text| {
                let output = mod_log_output.clone();
                let fence = mod_log_fence.clone();
                async move {
                    if let Some(fence) = fence {
                        fence
                            .publish_if_current(output.emit_mod_log(&plugin, &text))
                            .await;
                    } else {
                        output.emit_mod_log(&plugin, &text).await;
                    }
                }
            },
            move |plugin, text, timeout_ms| {
                let output = mod_toast_output.clone();
                let fence = mod_toast_fence.clone();
                async move {
                    if let Some(fence) = fence {
                        fence
                            .publish_if_current(output.emit_mod_toast(&plugin, &text, timeout_ms))
                            .await;
                    } else {
                        output.emit_mod_toast(&plugin, &text, timeout_ms).await;
                    }
                }
            },
            move |plugin, text| {
                let output = mod_status_output.clone();
                let fence = mod_status_fence.clone();
                async move {
                    if let Some(fence) = fence {
                        fence
                            .publish_if_current(output.emit_mod_status(&plugin, text.as_deref()))
                            .await;
                    } else {
                        output.emit_mod_status(&plugin, text.as_deref()).await;
                    }
                }
            },
        )
        .await;
    let checked = match checked {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(tool = name, %error, "mod tool.check failed; keeping core permission decision");
            return resolution;
        }
    };
    let projects_hard_hold = projects_session_tool_check_mod_hard_hold_for_call(orch, name).await;
    let mod_decision = checked
        .result
        .get("decision")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if projects_hard_hold
        && mod_decision != "deny"
        && (mod_decision == "allow" || permission_resolution_kind(&resolution) == "deny")
    {
        // Native `zjn` forces the core when the exact Projects predicate is
        // active; a Mod Allow cannot replace that decision, and a core Deny
        // cannot be weakened by a non-deny Mod result.
        return resolution;
    }
    project_mod_tool_check_result(
        name,
        &checked.result,
        &checked.hooked,
        Some(resolution.clone()),
        restricted_protected_mutation,
    )
    .unwrap_or(resolution)
}

fn permission_resolution_kind(resolution: &PermissionResolution) -> &'static str {
    match resolution {
        PermissionResolution::Allow { .. } => "allow",
        PermissionResolution::Deny { .. } => "deny",
        PermissionResolution::Ask | PermissionResolution::AskWithContext { .. } => "ask",
    }
}

fn project_mod_tool_check_result(
    name: &str,
    checked: &serde_json::Value,
    hooked: &[String],
    core: Option<PermissionResolution>,
    restricted_protected_mutation: bool,
) -> Option<PermissionResolution> {
    let decision = checked
        .get("decision")
        .and_then(serde_json::Value::as_str)?;
    let reason = checked.get("reason").and_then(serde_json::Value::as_str);
    let original_decision = core.as_ref().map(permission_resolution_kind);
    if original_decision == Some(decision) {
        return core.map(|resolution| match (resolution, reason) {
            (
                PermissionResolution::Deny {
                    source,
                    rule_source,
                    decision_reason_type,
                    decision_reason,
                    behavior_ask,
                    content_blocks,
                    ..
                },
                Some(rewritten),
            ) => PermissionResolution::Deny {
                reason: rewritten.to_string(),
                source,
                rule_source,
                decision_reason_type,
                decision_reason,
                behavior_ask,
                content_blocks,
            },
            (PermissionResolution::Ask, Some(rewritten)) => PermissionResolution::AskWithContext {
                decision_reason_type: None,
                decision_reason: Some(rewritten.to_string()),
            },
            (
                PermissionResolution::AskWithContext {
                    decision_reason_type,
                    ..
                },
                Some(rewritten),
            ) => PermissionResolution::AskWithContext {
                decision_reason_type,
                decision_reason: Some(rewritten.to_string()),
            },
            (unchanged, _) => unchanged,
        });
    }
    if restricted_protected_mutation && original_decision == Some("deny") && decision != "deny" {
        return core;
    }
    let plugin_names = hooked.join(", ");
    let actor = if hooked.len() == 1 {
        format!("plugin {plugin_names}")
    } else {
        format!("plugins {plugin_names}")
    };
    let suffix = reason.map_or_else(String::new, |reason| format!(": {reason}"));
    match decision {
        "allow" => Some(PermissionResolution::Allow {
            rule_source: None,
            classifier_approved: false,
        }),
        "ask" => Some(PermissionResolution::AskWithContext {
            decision_reason_type: Some("hook".into()),
            decision_reason: Some(format!("{name} needs approval (asked by {actor}{suffix})")),
        }),
        "deny" => Some(PermissionResolution::Deny {
            reason: format!("Permission to use {name} denied by {actor}{suffix}"),
            source: PermissionDecisionSource::Unspecified,
            rule_source: None,
            decision_reason_type: Some("hook".into()),
            decision_reason: reason.map(str::to_string),
            behavior_ask: false,
            content_blocks: Vec::new(),
        }),
        _ => core,
    }
}

fn retain_same_decision_mod_reason(
    mut execution: PermissionResolution,
    projected_tool_check: &PermissionResolution,
    original_decision: &str,
) -> PermissionResolution {
    // A Mod may rewrite the message while retaining its decision. Preserve that
    // copy only if the post-Mod execution stage still has the same behavior;
    // e.g. a ToolCheck Ask rewritten by a Mod must not undo z7o's later
    // `dontAsk` Ask→Deny transform.
    if permission_resolution_kind(&execution) != original_decision {
        return execution;
    }
    match (&mut execution, projected_tool_check) {
        (
            PermissionResolution::Deny { reason, .. },
            PermissionResolution::Deny {
                reason: projected, ..
            },
        ) => *reason = projected.clone(),
        (
            PermissionResolution::AskWithContext {
                decision_reason, ..
            },
            PermissionResolution::AskWithContext {
                decision_reason: Some(projected),
                ..
            },
        ) => *decision_reason = Some(projected.clone()),
        _ => {}
    }
    execution
}

#[derive(Clone)]
struct NormalToolCheckCore {
    resolution: PermissionResolution,
    evaluation: Option<lingxi_core::host::permission_gate::PermissionToolCheckEvaluation>,
    tool_ask_reason: Option<permission::PermissionDecisionReason>,
    updated_input: Option<serde_json::Value>,
    tool_permission_result: Option<permission::PermissionResult>,
}

fn mod_tool_check_ceiling_value(
    ceiling: Option<lingxi_core::host::McpPermissionCeiling>,
) -> Option<serde_json::Value> {
    match ceiling {
        Some(lingxi_core::host::McpPermissionCeiling::Ask) => Some(serde_json::json!("ask")),
        Some(
            lingxi_core::host::McpPermissionCeiling::Allow
            | lingxi_core::host::McpPermissionCeiling::Deny,
        )
        | None => None,
    }
}

fn mod_tool_check_event(
    name: &str,
    input: &serde_json::Value,
    tool_use_id: &ToolUseId,
    agent_id: Option<&str>,
    ceiling: Option<lingxi_core::host::McpPermissionCeiling>,
) -> serde_json::Value {
    let mut event = serde_json::Map::new();
    event.insert("tool".into(), serde_json::json!(name));
    event.insert("input".into(), input.clone());
    event.insert(
        "tool_use_id".into(),
        serde_json::json!(tool_use_id.as_str()),
    );
    if let Some(agent_id) = agent_id {
        event.insert("agentId".into(), serde_json::json!(agent_id));
    }
    if let Some(ceiling) = mod_tool_check_ceiling_value(ceiling) {
        event.insert("ceiling".into(), ceiling);
    }
    serde_json::Value::Object(event)
}

fn normal_core_mod_result(
    core: &NormalToolCheckCore,
    tool_check_ceiling: Option<lingxi_core::host::McpPermissionCeiling>,
) -> serde_json::Value {
    let reason = match &core.resolution {
        PermissionResolution::Deny { reason, .. } => Some(reason.clone()),
        PermissionResolution::Ask | PermissionResolution::AskWithContext { .. } => {
            let explicit_ask_rule = core.evaluation.as_ref().is_some_and(|evaluation| {
                evaluation.verdict.decision
                    == lingxi_core::host::permission_gate::ModToolCheckDecision::Ask
                    && evaluation.verdict.rule.is_some()
            });
            if explicit_ask_rule {
                core.evaluation
                    .as_ref()
                    .and_then(|evaluation| evaluation.verdict.reason.clone())
            } else {
                match &core.tool_permission_result {
                    Some(permission::PermissionResult::Ask { prompt, .. }) => {
                        Some(prompt.message.clone())
                    }
                    _ => core
                        .evaluation
                        .as_ref()
                        .and_then(|evaluation| evaluation.verdict.reason.clone()),
                }
            }
        }
        PermissionResolution::Allow { .. } => None,
    };
    let rule = core
        .evaluation
        .as_ref()
        .and_then(|evaluation| evaluation.verdict.rule.clone());
    let mut value = serde_json::Map::new();
    value.insert(
        "decision".into(),
        serde_json::json!(permission_resolution_kind(&core.resolution)),
    );
    if let Some(reason) = reason {
        value.insert("reason".into(), serde_json::json!(reason));
    }
    if let Some(rule) = rule {
        value.insert("rule".into(), serde_json::json!(rule));
    }
    if let Some(ceiling) = mod_tool_check_ceiling_value(tool_check_ceiling) {
        value.insert("ceiling".into(), ceiling);
    }
    serde_json::Value::Object(value)
}

async fn apply_mod_tool_check_lazy_normal(
    orch: &ConversationOrchestrator,
    name: &str,
    input: &serde_json::Value,
    tool_use_id: &ToolUseId,
    tool: &dyn tool_api::tool_trait::Tool,
    tool_ctx: &ToolUseContext,
    tool_check_ceiling: Option<lingxi_core::host::McpPermissionCeiling>,
    plan_mode: bool,
    hook_ask: bool,
    requires_user_interaction: bool,
    restricted_protected_mutation: bool,
    publication_fence: Option<std::sync::Arc<dyn HookPublicationGuard>>,
) -> Result<
    (
        PermissionResolution,
        Option<permission::PermissionDecisionReason>,
        Option<permission::PermissionResult>,
        Option<serde_json::Value>,
        Option<lingxi_core::host::permission_gate::PermissionToolCheckEvaluation>,
        bool,
    ),
    OrchestratorError,
> {
    let agent_id = tool_ctx.agent_id.as_ref().map(ToString::to_string);
    let host = if let Some(registry) = &orch.lifecycle_runtime.hook_registry {
        registry.read().await.mod_host()
    } else {
        None
    };
    let Some(host) = host else {
        let core = resolve_normal_tool_check_core(
            orch,
            name,
            input,
            tool_use_id,
            tool,
            tool_ctx,
            tool_check_ceiling,
            plan_mode,
            hook_ask,
            requires_user_interaction,
            restricted_protected_mutation,
        )
        .await?;
        return Ok((
            core.resolution,
            core.tool_ask_reason,
            core.tool_permission_result,
            core.updated_input,
            core.evaluation,
            false,
        ));
    };
    let event = mod_tool_check_event(
        name,
        input,
        tool_use_id,
        agent_id.as_deref(),
        tool_check_ceiling,
    );
    let expected = event.clone();
    let core_slot = std::sync::Arc::new(tokio::sync::Mutex::new(None::<NormalToolCheckCore>));
    let abort_slot = std::sync::Arc::new(tokio::sync::Mutex::new(None::<OrchestratorError>));
    let core_writer = core_slot.clone();
    let abort_writer = abort_slot.clone();
    let mod_log_output = orch.output.clone();
    let mod_toast_output = orch.output.clone();
    let mod_status_output = orch.output.clone();
    let mod_log_fence = publication_fence.clone();
    let mod_toast_fence = publication_fence.clone();
    let mod_status_fence = publication_fence.clone();
    let generation_bound_session =
        publication_fence
            .clone()
            .map(|publication_guard| GenerationBoundModSessionContext {
                inner: orch
                    .upgrade_streaming_tool_dispatch_owner()
                    .expect("streaming tool.check uses its bound orchestrator owner"),
                publication_guard,
            });
    let session: &dyn hooks::mods::ModSessionContext = generation_bound_session
        .as_ref()
        .map_or(orch, |session| session);
    let checked = host
        .dispatch_with_ui_meta_at_session(
            "tool.check",
            event,
            session,
            move |forwarded| {
                let expected = expected.clone();
                let core_writer = core_writer.clone();
                let abort_writer = abort_writer.clone();
                async move {
                    if forwarded != expected {
                        return Err(hooks::mods::ModError::Hook(
                            "tool.check identity is pinned".into(),
                        ));
                    }
                    let mut cached_core = core_writer.lock().await;
                    let core = if let Some(core) = cached_core.as_ref() {
                        core.clone()
                    } else {
                        let core = match evaluate_normal_tool_check_core(
                            orch,
                            name,
                            input,
                            tool_use_id,
                            tool,
                            tool_ctx,
                            tool_check_ceiling,
                            plan_mode,
                            hook_ask,
                            requires_user_interaction,
                            restricted_protected_mutation,
                        )
                        .await
                        {
                            Ok(result) => result,
                            Err(error) => {
                                *abort_writer.lock().await = Some(error);
                                return Err(hooks::mods::ModError::Hook(
                                    "permission resolution aborted".into(),
                                ));
                            }
                        };
                        *cached_core = Some(core.clone());
                        core
                    };
                    drop(cached_core);
                    Ok(normal_core_mod_result(&core, tool_check_ceiling))
                }
            },
            move |plugin, text| {
                let output = mod_log_output.clone();
                let fence = mod_log_fence.clone();
                async move {
                    if let Some(fence) = fence {
                        fence
                            .publish_if_current(output.emit_mod_log(&plugin, &text))
                            .await;
                    } else {
                        output.emit_mod_log(&plugin, &text).await;
                    }
                }
            },
            move |plugin, text, timeout_ms| {
                let output = mod_toast_output.clone();
                let fence = mod_toast_fence.clone();
                async move {
                    if let Some(fence) = fence {
                        fence
                            .publish_if_current(output.emit_mod_toast(&plugin, &text, timeout_ms))
                            .await;
                    } else {
                        output.emit_mod_toast(&plugin, &text, timeout_ms).await;
                    }
                }
            },
            move |plugin, text| {
                let output = mod_status_output.clone();
                let fence = mod_status_fence.clone();
                async move {
                    if let Some(fence) = fence {
                        fence
                            .publish_if_current(output.emit_mod_status(&plugin, text.as_deref()))
                            .await;
                    } else {
                        output.emit_mod_status(&plugin, text.as_deref()).await;
                    }
                }
            },
        )
        .await;
    if let Some(error) = abort_slot.lock().await.take() {
        return Err(error);
    }
    let checked = match checked {
        Ok(checked) => checked,
        Err(error) => {
            tracing::warn!(tool = name, %error, "mod tool.check failed; evaluating core permission");
            let core = resolve_normal_tool_check_core(
                orch,
                name,
                input,
                tool_use_id,
                tool,
                tool_ctx,
                tool_check_ceiling,
                plan_mode,
                hook_ask,
                requires_user_interaction,
                restricted_protected_mutation,
            )
            .await?;
            return Ok((
                core.resolution,
                core.tool_ask_reason,
                core.tool_permission_result,
                core.updated_input,
                core.evaluation,
                false,
            ));
        }
    };
    let projects_hard_hold = projects_session_tool_check_mod_hard_hold_for_call(orch, name).await;
    let mut evaluated = core_slot.lock().await.clone();
    let Some(decision) = checked
        .result
        .get("decision")
        .and_then(serde_json::Value::as_str)
    else {
        let core = if let Some(core) = evaluated {
            core
        } else {
            evaluate_normal_tool_check_core(
                orch,
                name,
                input,
                tool_use_id,
                tool,
                tool_ctx,
                tool_check_ceiling,
                plan_mode,
                hook_ask,
                requires_user_interaction,
                restricted_protected_mutation,
            )
            .await?
        };
        let core = finish_normal_tool_check_core(
            orch,
            name,
            input,
            tool_use_id,
            tool,
            tool_ctx,
            plan_mode,
            hook_ask,
            requires_user_interaction,
            restricted_protected_mutation,
            core,
        )
        .await?;
        return Ok((
            core.resolution,
            core.tool_ask_reason,
            core.tool_permission_result,
            core.updated_input,
            core.evaluation,
            false,
        ));
    };
    let mut projects_execution_finished = false;
    if projects_hard_hold && decision != "deny" {
        // Native evaluates the normal core lazily for this exact subset after
        // any non-deny Mod verdict. Finish execution before deciding whether
        // that verdict may replace the core, preserving classifier/plan/ceiling
        // provenance and the original permission message.
        let core = if let Some(core) = evaluated.take() {
            core
        } else {
            evaluate_normal_tool_check_core(
                orch,
                name,
                input,
                tool_use_id,
                tool,
                tool_ctx,
                tool_check_ceiling,
                plan_mode,
                hook_ask,
                requires_user_interaction,
                restricted_protected_mutation,
            )
            .await?
        };
        let core = finish_normal_tool_check_core(
            orch,
            name,
            input,
            tool_use_id,
            tool,
            tool_ctx,
            plan_mode,
            hook_ask,
            requires_user_interaction,
            restricted_protected_mutation,
            core,
        )
        .await?;
        if decision == "allow" || permission_resolution_kind(&core.resolution) == "deny" {
            return Ok((
                core.resolution,
                core.tool_ask_reason,
                core.tool_permission_result,
                core.updated_input,
                core.evaluation,
                false,
            ));
        }
        evaluated = Some(core);
        projects_execution_finished = true;
    }
    if restricted_protected_mutation && decision != "deny" && evaluated.is_none() {
        let core = evaluate_normal_tool_check_core(
            orch,
            name,
            input,
            tool_use_id,
            tool,
            tool_ctx,
            tool_check_ceiling,
            plan_mode,
            hook_ask,
            requires_user_interaction,
            restricted_protected_mutation,
        )
        .await?;
        evaluated = Some(core);
    }
    let original_decision = evaluated
        .as_ref()
        .map(|core| permission_resolution_kind(&core.resolution));
    let projected = project_mod_tool_check_result(
        name,
        &checked.result,
        &checked.hooked,
        evaluated.as_ref().map(|core| core.resolution.clone()),
        restricted_protected_mutation,
    )
    .expect("validated tool.check decision");
    let changed_decision = original_decision != Some(permission_resolution_kind(&projected));
    let (resolution, tool_ask_reason, tool_permission_result, updated_input, evaluation) =
        if let Some(core) = evaluated {
            if changed_decision {
                (
                    projected,
                    core.tool_ask_reason,
                    core.tool_permission_result,
                    None,
                    None,
                )
            } else {
                let original_decision = original_decision.expect("core evaluation has a decision");
                let core = if projects_execution_finished {
                    core
                } else {
                    finish_normal_tool_check_core(
                        orch,
                        name,
                        input,
                        tool_use_id,
                        tool,
                        tool_ctx,
                        plan_mode,
                        hook_ask,
                        requires_user_interaction,
                        restricted_protected_mutation,
                        core,
                    )
                    .await?
                };
                let resolution =
                    retain_same_decision_mod_reason(core.resolution, &projected, original_decision);
                (
                    resolution,
                    core.tool_ask_reason,
                    core.tool_permission_result,
                    core.updated_input,
                    core.evaluation,
                )
            }
        } else {
            (projected, None, None, None, None)
        };
    Ok((
        resolution,
        tool_ask_reason,
        tool_permission_result,
        updated_input,
        evaluation,
        changed_decision,
    ))
}

pub(crate) async fn dispatch_tool_uses_tracked_deferred(
    orch: &ConversationOrchestrator,
    tool_uses: &[(ToolUseId, String, serde_json::Value, Option<String>)],
    cancel: Option<tokio_util::sync::CancellationToken>,
    assistant_message_id: Option<MessageId>,
) -> Result<DeferredToolDispatch, OrchestratorError> {
    dispatch_tool_uses_tracked_deferred_with_facts(
        orch,
        tool_uses,
        cancel,
        assistant_message_id,
        None,
        None,
        None,
        None,
        None,
    )
    .await
}

/// Host-captured facts for a raw assistant row's tool dispatch. `query_history`
/// is the exact successful request snapshot (or explicit turn.step input for a
/// synthetic response); the raw current row and prior same-turn tool blocks
/// remain separate. Batched dispatch adds the current row's earlier ToolUse
/// prefix per tool before constructing its receiver context.
#[derive(Debug, Clone)]
pub(crate) struct ToolUseDispatchFacts {
    pub(crate) query_history: Vec<ConversationMessage>,
    pub(crate) assistant_message: ConversationMessage,
    pub(crate) same_turn_tool_uses: Vec<ContentBlock>,
}

fn dispatch_facts_for_tool_index(
    tool_uses: &[(ToolUseId, String, serde_json::Value, Option<String>)],
    dispatch_facts: Option<&ToolUseDispatchFacts>,
    tool_index: usize,
) -> Option<ToolUseDispatchFacts> {
    let mut facts = dispatch_facts.cloned()?;
    facts
        .same_turn_tool_uses
        .extend(
            tool_uses
                .iter()
                .take(tool_index)
                .map(|(id, name, input, provider_id)| ContentBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    provider_id: provider_id.clone(),
                }),
        );
    Some(facts)
}

pub(crate) async fn streaming_tool_context_base(
    orch: &ConversationOrchestrator,
    messages: Vec<ConversationMessage>,
) -> ToolUseContext {
    let (main_loop_model, model_profile) = {
        let session = orch.session.lock().await;
        (session.model.clone(), session.model_profile.clone())
    };
    ToolUseContext {
        input_projection: None,
        options: ToolUseOptions {
            debug: false,
            verbose: false,
            main_loop_model,
            model_profile,
            max_budget_nano_usd: None,
            mcp_clients: Vec::new(),
            is_non_interactive_session: !orch.config.interactive_permissions,
            custom_system_prompt: orch.config.system_prompt_override.clone(),
            append_system_prompt: None,
        },
        messages,
        tool_use_id: None,
        assistant_message_id: None,
        assistant_message: None,
        same_turn_tool_uses: Vec::new(),
        agent_id: None,
        agent_spawn_provenance: Default::default(),
        nested_memory_triggers: orch.prompt_runtime.nested_memory_triggers.clone(),
        agent_name: None,
        team_name: None,
        origin_session_id: None,
        instruction_context: Some(orch.instruction_context_snapshot().await),
        tool_execution_policy: lingxi_core::host::tool_invoker::ToolExecutionPolicy::Ordinary,
        trusted_effective_permission_mode: None,
        classifier_only_review: None,
        content_replacement_state: None,
        session: Some(orch.session.clone()),
        observer_pairings: orch.model_runtime.observer_pairings.clone(),
        subagent_registry: Some(orch.tools.clone()),
        cancel: None,
        fork_parent_system_prompt: orch.current_turn_system_prompt().await,
        cwd: None,
        depth: 0,
        observer: None,
        file_history: orch
            .file_history
            .clone()
            .map(|fh| fh as std::sync::Arc<dyn lingxi_core::host::FileHistorySink>),
    }
}

/// Streaming-only entry point. A completed row can sit queued until another
/// tool releases a concurrency slot, so its query facts travel with that
/// registered tool rather than being reread from session state at start time.
pub(crate) async fn dispatch_streaming_tool_use(
    orch: &ConversationOrchestrator,
    tool_use: &(ToolUseId, String, serde_json::Value, Option<String>),
    cancel: Option<tokio_util::sync::CancellationToken>,
    assistant_message_id: MessageId,
    facts: ToolUseDispatchFacts,
) -> Result<DeferredToolDispatch, OrchestratorError> {
    dispatch_tool_uses_tracked_deferred_with_facts(
        orch,
        std::slice::from_ref(tool_use),
        cancel,
        Some(assistant_message_id),
        Some(facts),
        None,
        None,
        None,
        None,
    )
    .await
}

/// Streaming scheduler entry point. Reuses the handle selected at ToolUse
/// admission and carries the scheduler's current context-layer model into the
/// existing Mod and core dispatch pipeline.
pub(crate) async fn dispatch_streaming_tool_use_owned(
    orch: &ConversationOrchestrator,
    tool_use: &(ToolUseId, String, serde_json::Value, Option<String>),
    cancel: Option<tokio_util::sync::CancellationToken>,
    assistant_message_id: MessageId,
    facts: ToolUseDispatchFacts,
    prepared_tool: std::sync::Arc<dyn tool_api::tool_trait::Tool>,
    inherited_context: Option<ToolUseContext>,
    dispatch_started: Option<tokio::sync::oneshot::Sender<()>>,
    publication_fence: ToolDispatchPublicationFence,
) -> Result<DeferredToolDispatch, OrchestratorError> {
    dispatch_tool_uses_tracked_deferred_with_facts(
        orch,
        std::slice::from_ref(tool_use),
        cancel,
        Some(assistant_message_id),
        Some(facts),
        Some(prepared_tool),
        inherited_context,
        dispatch_started,
        Some(std::sync::Arc::new(publication_fence)),
    )
    .await
}

pub(crate) fn dispatch_tool_uses_tracked_deferred_with_facts<'a>(
    orch: &'a ConversationOrchestrator,
    tool_uses: &'a [(ToolUseId, String, serde_json::Value, Option<String>)],
    cancel: Option<tokio_util::sync::CancellationToken>,
    assistant_message_id: Option<MessageId>,
    dispatch_facts: Option<ToolUseDispatchFacts>,
    prepared_tool: Option<std::sync::Arc<dyn tool_api::tool_trait::Tool>>,
    inherited_context: Option<ToolUseContext>,
    dispatch_started: Option<tokio::sync::oneshot::Sender<()>>,
    publication_fence: Option<std::sync::Arc<dyn HookPublicationGuard>>,
) -> futures::future::BoxFuture<'a, Result<DeferredToolDispatch, OrchestratorError>> {
    Box::pin(async move {
        // This is the real streaming dispatch entry: after executor admission but
        // before hook lookup, permission waits, or any tool-call completion. The
        // scheduler Add barrier matches Native W1 at this point.
        if let Some(started) = dispatch_started {
            let _ = started.send(());
        }
        if publication_fence
            .as_ref()
            .is_some_and(|fence| !fence.is_current())
        {
            return Ok(DeferredToolDispatch::default());
        }
        let mod_host = if let Some(registry) = &orch.lifecycle_runtime.hook_registry {
            registry.read().await.mod_host()
        } else {
            None
        };
        let Some(mod_host) = mod_host else {
            return dispatch_tool_uses_tracked_deferred_core(
                orch,
                tool_uses,
                cancel,
                assistant_message_id,
                dispatch_facts.clone(),
                tool_uses.len(),
                None,
                None,
                prepared_tool.clone(),
                inherited_context.clone(),
                publication_fence.clone(),
            )
            .await;
        };
        let mut all = DeferredToolDispatch {
            results: Vec::with_capacity(tool_uses.len()),
            publications: Vec::with_capacity(tool_uses.len()),
            prevent_continuation: false,
            injected_messages: Vec::new(),
            context_modifiers: Vec::new(),
            post_tool_batch_calls: Vec::new(),
        };
        for (tool_index, (id, name, input, provider_id)) in tool_uses.iter().enumerate() {
            let per_tool_facts =
                dispatch_facts_for_tool_index(tool_uses, dispatch_facts.as_ref(), tool_index);
            let managed_pre_hook = if let Some(registry) = &orch.lifecycle_runtime.hook_registry {
                let event = HookEvent::PreToolUse {
                    tool_name: name.clone(),
                    tool_input: input.clone(),
                    tool_use_id: id.clone(),
                };
                let context = HookContext {
                    cwd: orch.current_cwd(),
                    ..Default::default()
                };
                registry
                    .read()
                    .await
                    .has_managed_pre_tool_use_match(&event, &context)
            } else {
                false
            };
            if managed_pre_hook {
                // Managed PreToolUse must run before a Mod can short-circuit the
                // call. Until the two hook phases are split, run the core path so
                // the managed hook retains its authority.
                let single = vec![(id.clone(), name.clone(), input.clone(), provider_id.clone())];
                append_mod_dispatch(
                    &mut all,
                    dispatch_tool_uses_tracked_deferred_core(
                        orch,
                        &single,
                        cancel.clone(),
                        assistant_message_id,
                        per_tool_facts.clone(),
                        tool_uses.len(),
                        None,
                        None,
                        prepared_tool.clone(),
                        inherited_context.clone(),
                        publication_fence.clone(),
                    )
                    .await?,
                );
                continue;
            }
            let mut event = serde_json::Map::new();
            event.insert("tool".into(), serde_json::Value::String(name.clone()));
            event.insert(
                "tool_use_id".into(),
                serde_json::Value::String(id.as_str().to_owned()),
            );
            if let Some(args) = input.as_object() {
                for (key, value) in args {
                    if key != "tool" && key != "tool_use_id" && key != "agentId" {
                        event.insert(key.clone(), value.clone());
                    }
                }
            }
            let completed = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<ModCoreRun>::new()));
            let completed_for_core = completed.clone();
            let core_error = std::sync::Arc::new(tokio::sync::Mutex::new(None));
            let core_error_for_dispatch = core_error.clone();
            let id_for_core = id.clone();
            let name_for_core = name.clone();
            let provider_for_core = provider_id.clone();
            let cancel_for_core = cancel.clone();
            let batch_tool_count = tool_uses.len();
            let dispatch_facts_for_core = per_tool_facts.clone();
            let prepared_tool_for_core = prepared_tool.clone();
            let inherited_context_for_core = inherited_context.clone();
            let publication_fence_for_core = publication_fence.clone();
            let mod_log_output = orch.output.clone();
            let mod_toast_output = orch.output.clone();
            let mod_status_output = orch.output.clone();
            let mod_log_fence = publication_fence.clone();
            let mod_toast_fence = publication_fence.clone();
            let mod_status_fence = publication_fence.clone();
            let generation_bound_session = publication_fence.clone().map(|publication_guard| {
                GenerationBoundModSessionContext {
                    inner: orch
                        .upgrade_streaming_tool_dispatch_owner()
                        .expect("streaming tool.call uses its bound orchestrator owner"),
                    publication_guard,
                }
            });
            let session: &dyn hooks::mods::ModSessionContext = generation_bound_session
                .as_ref()
                .map_or(orch, |session| session);
            let mod_cwd = session.cwd();
            let answer = mod_host
                .dispatch_with_utf16_at_context(
                    "tool.call",
                    hooks::mods::ModUtf16ValueProjection::plain(serde_json::Value::Object(event)),
                    &mod_cwd,
                    Some(session),
                    None,
                    None,
                    None,
                    lingxi_core::host::task_registry::FieldPresence::Missing,
                    move |next_event| {
                        let completed = completed_for_core.clone();
                        let core_error = core_error_for_dispatch.clone();
                        let id = id_for_core.clone();
                        let name = name_for_core.clone();
                        let provider_id = provider_for_core.clone();
                        let cancel = cancel_for_core.clone();
                        let assistant_message_id = assistant_message_id;
                        let dispatch_facts = dispatch_facts_for_core.clone();
                        let prepared_tool = prepared_tool_for_core.clone();
                        let inherited_context = inherited_context_for_core.clone();
                        let publication_fence = publication_fence_for_core.clone();
                        async move {
                            if next_event
                                .value
                                .get("tool")
                                .and_then(serde_json::Value::as_str)
                                != Some(name.as_str())
                                || next_event
                                    .value
                                    .get("tool_use_id")
                                    .and_then(serde_json::Value::as_str)
                                    != Some(id.as_str())
                                || next_event.value.get("agentId").is_some()
                            {
                                return Err(hooks::mods::ModError::Hook(
                                    "tool, tool_use_id, and agentId are reserved".into(),
                                ));
                            }
                            let mut args =
                                next_event.value.as_object().cloned().ok_or_else(|| {
                                    hooks::mods::ModError::Hook(
                                        "tool.call input must be an object".into(),
                                    )
                                })?;
                            args.remove("tool");
                            args.remove("tool_use_id");
                            args.remove("agentId");
                            let single =
                                vec![(id, name, serde_json::Value::Object(args), provider_id)];
                            let (dispatched, stage) = with_mod_result_stage(
                                &single[0].0,
                                dispatch_tool_uses_tracked_deferred_core(
                                    orch,
                                    &single,
                                    cancel,
                                    assistant_message_id,
                                    dispatch_facts,
                                    batch_tool_count,
                                    None,
                                    None,
                                    prepared_tool,
                                    inherited_context,
                                    publication_fence,
                                ),
                            )
                            .await;
                            let mut dispatched = match dispatched {
                                Ok(dispatched) => dispatched,
                                Err(error) => {
                                    let message = error.to_string();
                                    *core_error.lock().await = Some(error);
                                    return Err(hooks::mods::ModError::Hook(message));
                                }
                            };
                            for publication in std::mem::take(&mut dispatched.publications) {
                                // The active ModResultStage captures these local
                                // sidecars; the outer wrapper decides whether Tn
                                // accepts and publishes them.
                                publication.commit(orch).await;
                            }
                            let mut result = serde_json::Map::new();
                            if let Some(ContentBlock::ToolResult {
                                content, is_error, ..
                            }) = dispatched.results.first()
                            {
                                result.insert(
                                    "result".into(),
                                    stage.tool_use_result.clone().unwrap_or_else(|| {
                                        serde_json::Value::String(content.clone())
                                    }),
                                );
                                result.insert(
                                    "text".into(),
                                    serde_json::Value::String(content.clone()),
                                );
                                if is_error.unwrap_or(false) {
                                    result.insert("isError".into(), serde_json::Value::Bool(true));
                                }
                            } else {
                                result.insert("result".into(), serde_json::Value::Null);
                            }
                            let mut completed = completed.lock().await;
                            let index = completed.len() + 1;
                            completed.push(ModCoreRun { dispatched, stage });
                            result.insert("ref".into(), serde_json::json!(index));
                            Ok(hooks::mods::ModUtf16ValueProjection::plain(
                                serde_json::Value::Object(result),
                            ))
                        }
                    },
                    move |plugin, line| {
                        let output = mod_log_output.clone();
                        let fence = mod_log_fence.clone();
                        async move {
                            if let Some(fence) = fence {
                                fence
                                    .publish_if_current(output.emit_mod_log(&plugin, &line))
                                    .await;
                            } else {
                                output.emit_mod_log(&plugin, &line).await;
                            }
                        }
                    },
                    move |plugin, text, timeout_ms| {
                        let output = mod_toast_output.clone();
                        let fence = mod_toast_fence.clone();
                        async move {
                            if let Some(fence) = fence {
                                fence
                                    .publish_if_current(
                                        output.emit_mod_toast(&plugin, &text, timeout_ms),
                                    )
                                    .await;
                            } else {
                                output.emit_mod_toast(&plugin, &text, timeout_ms).await;
                            }
                        }
                    },
                    move |plugin, text| {
                        let output = mod_status_output.clone();
                        let fence = mod_status_fence.clone();
                        async move {
                            if let Some(fence) = fence {
                                fence
                                    .publish_if_current(
                                        output.emit_mod_status(&plugin, text.as_deref()),
                                    )
                                    .await;
                            } else {
                                output.emit_mod_status(&plugin, text.as_deref()).await;
                            }
                        }
                    },
                )
                .await;
            if publication_fence
                .as_ref()
                .is_some_and(|fence| !fence.is_current())
            {
                return Ok(all);
            }
            let answer = match answer {
                Ok(answer) => Utf16JsonProjection {
                    value: answer.result,
                    strings: answer
                        .result_utf16_strings
                        .into_iter()
                        .map(|sidecar| Utf16JsonString {
                            pointer: sidecar.pointer,
                            code_units: sidecar.code_units,
                        })
                        .collect(),
                    keys: answer
                        .result_utf16_keys
                        .into_iter()
                        .map(|sidecar| Utf16JsonKey {
                            pointer: sidecar.pointer,
                            placeholder: sidecar.placeholder,
                            code_units: sidecar.code_units,
                        })
                        .collect(),
                },
                Err(error) => {
                    tracing::warn!(tool = %name, error = %error, "mod tool.call failed; continuing without mod result");
                    if let Some(core_error) = core_error.lock().await.take() {
                        return Err(core_error);
                    }
                    let completed_core = completed.lock().await.pop();
                    if let Some(core) = completed_core {
                        finish_mod_result_stage(
                            orch,
                            publication_fence.as_deref(),
                            &mut all,
                            id,
                            name,
                            core.stage,
                            None,
                        )
                        .await;
                        append_mod_dispatch(&mut all, core.dispatched);
                        continue;
                    }
                    let single =
                        vec![(id.clone(), name.clone(), input.clone(), provider_id.clone())];
                    let dispatched = dispatch_tool_uses_tracked_deferred_core(
                        orch,
                        &single,
                        cancel.clone(),
                        assistant_message_id,
                        per_tool_facts.clone(),
                        tool_uses.len(),
                        None,
                        None,
                        prepared_tool.clone(),
                        inherited_context.clone(),
                        publication_fence.clone(),
                    )
                    .await?;
                    append_mod_dispatch(&mut all, dispatched);
                    continue;
                }
            };
            let answer_value = answer.value.clone();
            let schema_error = if answer_value.get("deny").is_some() {
                None
            } else if let Some(result) = answer_value.get("result") {
                let reuses_core = {
                    let completed = completed.lock().await;
                    answer_value
                        .get("ref")
                        .and_then(|reference| tool_call_ref_index(reference, completed.len()))
                        .and_then(|index| completed.get(index))
                        .is_some_and(|core| {
                            let original = core.stage.tool_use_result.clone().or_else(|| {
                                core.dispatched
                                    .results
                                    .first()
                                    .and_then(|block| match block {
                                        ContentBlock::ToolResult { content, .. } => {
                                            Some(serde_json::Value::String(content.clone()))
                                        }
                                        _ => None,
                                    })
                            });
                            // Defer has no tool result; its core reply carries the
                            // null placeholder above. Reusing that reply must keep
                            // the core's stop signal without validating an output.
                            original
                                .as_ref()
                                .map_or(result.is_null(), |original| original == result)
                        })
                };
                if reuses_core {
                    None
                } else {
                    match prepared_tool.as_ref() {
                        Some(tool) => tool.output_schema().cloned(),
                        None => orch
                            .tools
                            .find_by_name(name)
                            .and_then(|tool| tool.output_schema().cloned()),
                    }
                    .and_then(|schema| {
                        crate::schema_validation::validate_tool_output_schema(&schema, result)
                            .err()
                    })
                    .map(|detail| {
                        format!(
                            "tool.call step resolved {name} with a result that does not match its output shape: {detail}"
                        )
                    })
                }
            } else {
                None
            };
            if let Some(message) = schema_error {
                let content = format!("<tool_use_error>{message}</tool_use_error>");
                if completed.lock().await.is_empty() {
                    orch.output.emit_tool_call(id, name, input).await;
                }
                let mut publication = ToolResultPublication::frame_only(
                    id,
                    name,
                    &content,
                    serde_json::json!({"error":message}),
                );
                publication.tool_use_result = Some(serde_json::json!(format!("Error: {message}")));
                publish_or_defer_tool_result(
                    orch,
                    publication_fence.as_deref(),
                    &mut all.publications,
                    publication,
                )
                .await;
                all.results.push(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: content.clone(),
                    is_error: Some(true),
                    provider_tool_use_id: provider_id.clone(),
                    content_blocks: None,
                });
                append_mod_tool_context(orch, &mut all, id, &answer, publication_fence.clone())
                    .await;
                all.post_tool_batch_calls
                    .push(hooks::events::PostToolBatchCall {
                        tool_name: name.clone(),
                        tool_input: input.clone(),
                        tool_use_id: id.clone(),
                        tool_response: Some(serde_json::Value::String(content)),
                    });
                continue;
            }
            // Native DVt handles `deny` before looking at `ref`. A deny therefore
            // wins even when the middleware also returns a valid run reference.
            if answer_value.get("deny").is_none() {
                let mut completed = completed.lock().await;
                if let Some(index) = answer_value
                    .get("ref")
                    .and_then(|reference| tool_call_ref_index(reference, completed.len()))
                {
                    let mut core = completed.remove(index);
                    drop(completed);
                    let mut replacement = None;
                    if let Some(ContentBlock::ToolResult {
                        content,
                        content_blocks,
                        is_error,
                        ..
                    }) = core.dispatched.results.first_mut()
                    {
                        let original = content.clone();
                        let original_raw = core
                            .stage
                            .tool_use_result
                            .clone()
                            .unwrap_or_else(|| serde_json::Value::String(original.clone()));
                        let changed_result = answer_value
                            .get("result")
                            .is_some_and(|result| result != &original_raw);
                        // Claude 2.1.287 `oLn` reuses core's message verbatim when
                        // `ref` names a run and `result` is unchanged. `text` is
                        // descriptive data on `next(e)`, not a replacement channel.
                        if changed_result {
                            let raw = answer_value.get("result").cloned().unwrap_or(original_raw);
                            let (new_text, mapped_blocks, mapped_error) =
                                map_mod_result_for_model(orch, name, &raw, prepared_tool.as_ref());
                            *content = new_text.clone();
                            *is_error = Some(mapped_error.unwrap_or_else(|| {
                                answer_value
                                    .get("isError")
                                    .and_then(serde_json::Value::as_bool)
                                    .unwrap_or(false)
                            }));
                            *content_blocks = mapped_blocks;
                            for call in &mut core.dispatched.post_tool_batch_calls {
                                if call.tool_use_id == *id {
                                    call.tool_response =
                                        Some(serde_json::Value::String(new_text.clone()));
                                }
                            }
                            replacement = Some((raw, new_text));
                        }
                    }
                    for call in &mut core.dispatched.post_tool_batch_calls {
                        if call.tool_use_id == *id {
                            call.tool_input = input.clone();
                        }
                    }
                    finish_mod_result_stage(
                        orch,
                        publication_fence.as_deref(),
                        &mut all,
                        id,
                        name,
                        core.stage,
                        replacement,
                    )
                    .await;
                    append_mod_dispatch(&mut all, core.dispatched);
                    append_mod_tool_context(orch, &mut all, id, &answer, publication_fence.clone())
                        .await;
                    continue;
                }
            }
            let (content, content_blocks, is_error, raw_result, frame_result) = if let Some(
                reason,
            ) =
                answer_value.get("deny").and_then(serde_json::Value::as_str)
            {
                let content = format!("<tool_use_error>{reason}</tool_use_error>");
                (
                    content.clone(),
                    None,
                    true,
                    serde_json::Value::String(content),
                    serde_json::json!({"error":reason}),
                )
            } else if let Some(result) = answer_value.get("result") {
                let (text, blocks, mapped_error) =
                    map_mod_result_for_model(orch, name, result, prepared_tool.as_ref());
                (
                    text,
                    blocks,
                    mapped_error.unwrap_or(false),
                    result.clone(),
                    result.clone(),
                )
            } else {
                tracing::warn!(tool = %name, "mod tool.call returned an invalid result; executing original call");
                let completed_core = completed.lock().await.pop();
                if let Some(core) = completed_core {
                    finish_mod_result_stage(
                        orch,
                        publication_fence.as_deref(),
                        &mut all,
                        id,
                        name,
                        core.stage,
                        None,
                    )
                    .await;
                    append_mod_dispatch(&mut all, core.dispatched);
                    continue;
                }
                let single = vec![(id.clone(), name.clone(), input.clone(), provider_id.clone())];
                append_mod_dispatch(
                    &mut all,
                    dispatch_tool_uses_tracked_deferred_core(
                        orch,
                        &single,
                        cancel.clone(),
                        assistant_message_id,
                        per_tool_facts.clone(),
                        tool_uses.len(),
                        None,
                        None,
                        prepared_tool.clone(),
                        inherited_context.clone(),
                        publication_fence.clone(),
                    )
                    .await?,
                );
                continue;
            };
            if completed.lock().await.is_empty() {
                orch.output.emit_tool_call(id, name, input).await;
            }
            let publication = ToolResultPublication {
                tool_use_id: id.clone(),
                tool_use_result: Some(raw_result),
                mcp_meta: None,
                turn_end: None,
                denial_kind: None,
                permission_denial: None,
                frame: Some(ToolResultFramePublication {
                    tool: name.clone(),
                    model_text: content.clone(),
                    result: frame_result,
                    denial_kind: None,
                }),
            };
            publish_or_defer_tool_result(
                orch,
                publication_fence.as_deref(),
                &mut all.publications,
                publication,
            )
            .await;
            let tool_response = serde_json::Value::String(content.clone());
            all.results.push(ContentBlock::ToolResult {
                tool_use_id: id.clone(),
                content,
                is_error: Some(is_error),
                provider_tool_use_id: provider_id.clone(),
                content_blocks,
            });
            append_mod_tool_context(orch, &mut all, id, &answer, publication_fence.clone()).await;
            all.post_tool_batch_calls
                .push(hooks::events::PostToolBatchCall {
                    tool_name: name.clone(),
                    tool_input: input.clone(),
                    tool_use_id: id.clone(),
                    tool_response: Some(tool_response),
                });
        }
        Ok(all)
    })
}

fn map_mod_result_for_model(
    orch: &ConversationOrchestrator,
    name: &str,
    result: &serde_json::Value,
    prepared_tool: Option<&std::sync::Arc<dyn tool_api::tool_trait::Tool>>,
) -> (String, Option<Vec<serde_json::Value>>, Option<bool>) {
    let tool = prepared_tool
        .map(std::sync::Arc::clone)
        .or_else(|| orch.tools.find_by_name(name));
    let mapped_text = tool
        .as_ref()
        .and_then(|tool| tool.map_result_text(result))
        .unwrap_or_else(|| {
            result
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| tool_result_to_model_text(result))
        });
    let blocks = match tool.as_ref() {
        Some(tool) if tool.is_mcp() => tool_api::tool_result_media::media_content_blocks(result),
        _ if matches!(name, "Bash" | "Read" | "computer") => {
            tool_api::tool_result_media::media_content_blocks_for_tool(name, result, &mapped_text)
        }
        _ => tool_api::tool_result_media::image_content_blocks(result),
    };
    let text = blocks
        .as_ref()
        .and_then(|_| tool_api::tool_result_media::ephemeral_summary(result))
        .unwrap_or(mapped_text);
    let is_error = tool
        .as_ref()
        .and_then(|tool| tool.map_result_is_error(result));
    (text, blocks, is_error)
}

async fn append_mod_tool_context(
    orch: &ConversationOrchestrator,
    all: &mut DeferredToolDispatch,
    tool_use_id: &ToolUseId,
    answer: &Utf16JsonProjection,
    publication_fence: Option<std::sync::Arc<dyn HookPublicationGuard>>,
) {
    if answer.value.get("deny").is_some() {
        return;
    }
    let Ok(context_projection) = answer.subprojection("/context") else {
        return;
    };
    let Some(context) = context_projection.value.as_array() else {
        return;
    };
    let lines = context
        .iter()
        .enumerate()
        .filter_map(|(index, value)| {
            let text = value.as_str()?;
            if text.is_empty() {
                return None;
            }
            let code_units = context_projection
                .string_units(&format!("/{index}"))
                .unwrap_or_else(|| text.encode_utf16().collect());
            Some((text.to_owned(), code_units))
        })
        .collect::<Vec<_>>();
    if lines.is_empty() {
        return;
    }
    let exact_lines = lines
        .iter()
        .map(|(display, utf16_code_units)| hooks::ExactHookText {
            display: display.clone(),
            utf16_code_units: utf16_code_units.clone(),
        })
        .collect::<Vec<_>>();
    let attachment = hooks::additional_context_attachment(
        "tool.call",
        &format!("{}-context", tool_use_id.as_str()),
        "PostToolUse",
        &exact_lines,
    );
    orch.queue_hook_attachment(tool_use_id, attachment, publication_fence.clone())
        .await;
    let body = hooks::ExactHookText::join(&exact_lines, "\n");
    let reminder = hooks::ExactHookText::wrapped(
        "<system-reminder>\ntool.call hook additional context: ",
        &body,
        "\n</system-reminder>",
    );
    let message_id = MessageId::new();
    let message = ConversationMessage::user_meta_js_utf16(
        message_id,
        reminder.display,
        reminder.utf16_code_units,
    );
    let register = async {
        orch.register_mod_persisted_attachment(
            &message,
            "hook_additional_context",
            serde_json::json!({"kind":"plugin","event":"tool.call"}),
        )
        .await;
    };
    if let Some(fence) = publication_fence {
        fence.commit_if_current(Box::pin(register)).await;
    } else {
        register.await;
    }
    all.injected_messages.push((message, tool_use_id.clone()));
}

fn append_mod_dispatch(all: &mut DeferredToolDispatch, one: DeferredToolDispatch) {
    all.results.extend(one.results);
    all.publications.extend(one.publications);
    all.prevent_continuation |= one.prevent_continuation;
    all.injected_messages.extend(one.injected_messages);
    all.context_modifiers.extend(one.context_modifiers);
    all.post_tool_batch_calls.extend(one.post_tool_batch_calls);
}

pub(super) async fn register_mod_persisted_attachment_if_visible(
    orch: &ConversationOrchestrator,
    message: &ConversationMessage,
    kind: &str,
    origin: serde_json::Value,
    publication_guard: Option<&dyn hooks::attachment::HookPublicationGuard>,
) {
    if !active_mod_result_stage_is_virtual().await {
        let registration = orch.register_mod_persisted_attachment(message, kind, origin);
        if let Some(guard) = publication_guard {
            guard.commit_if_current(Box::pin(registration)).await;
        } else {
            registration.await;
        }
    }
}

async fn dispatch_tool_uses_tracked_deferred_core(
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
    dispatch_facts: Option<ToolUseDispatchFacts>,
    original_batch_tool_count: usize,
    permission_consent: Option<String>,
    agent_spawn_provenance: Option<lingxi_core::host::subagent_spawn::AgentSpawnProvenance>,
    prepared_tool: Option<std::sync::Arc<dyn tool_api::tool_trait::Tool>>,
    inherited_context: Option<ToolUseContext>,
    publication_fence: Option<std::sync::Arc<dyn HookPublicationGuard>>,
) -> Result<DeferredToolDispatch, OrchestratorError> {
    // The core holds the full hook/permission/execution state machine. Keep
    // that state on the heap instead of embedding it in every dispatch caller.
    super::boxed_turn_future(|| {
        dispatch_tool_uses_tracked_deferred_core_impl(
            orch,
            tool_uses,
            cancel,
            assistant_message_id,
            dispatch_facts,
            original_batch_tool_count,
            permission_consent,
            agent_spawn_provenance,
            prepared_tool,
            inherited_context,
            publication_fence,
        )
    })
    .await
}

async fn dispatch_tool_uses_tracked_deferred_core_with_tool(
    orch: &ConversationOrchestrator,
    tool_uses: &[(ToolUseId, String, serde_json::Value, Option<String>)],
    cancel: Option<tokio_util::sync::CancellationToken>,
    assistant_message_id: Option<MessageId>,
    original_batch_tool_count: usize,
    permission_consent: Option<String>,
    agent_spawn_provenance: Option<lingxi_core::host::subagent_spawn::AgentSpawnProvenance>,
    prepared_tool: std::sync::Arc<dyn tool_api::tool_trait::Tool>,
    publication_fence: Option<std::sync::Arc<dyn HookPublicationGuard>>,
) -> Result<DeferredToolDispatch, OrchestratorError> {
    super::boxed_turn_future(|| {
        dispatch_tool_uses_tracked_deferred_core_impl(
            orch,
            tool_uses,
            cancel,
            assistant_message_id,
            None,
            original_batch_tool_count,
            permission_consent,
            agent_spawn_provenance,
            Some(prepared_tool),
            None,
            publication_fence,
        )
    })
    .await
}

async fn dispatch_tool_uses_tracked_deferred_core_impl(
    orch: &ConversationOrchestrator,
    tool_uses: &[(ToolUseId, String, serde_json::Value, Option<String>)],
    // PHASE-2 + DEFERRED-3: per-tool `CancellationToken` (a child of the streaming
    // executor's `tool_abort`) threaded into each tool's `ToolUseContext::cancel`.
    // It fires when the turn is discarded (streaming fallback) OR — because
    // `tool_abort` is parented to the turn's user-interrupt token — when the
    // USER interrupts. `None` is used by non-streaming callers.
    cancel: Option<tokio_util::sync::CancellationToken>,
    assistant_message_id: Option<MessageId>,
    dispatch_facts: Option<ToolUseDispatchFacts>,
    original_batch_tool_count: usize,
    permission_consent: Option<String>,
    agent_spawn_provenance: Option<lingxi_core::host::subagent_spawn::AgentSpawnProvenance>,
    prepared_tool: Option<std::sync::Arc<dyn tool_api::tool_trait::Tool>>,
    inherited_context: Option<ToolUseContext>,
    publication_fence: Option<std::sync::Arc<dyn HookPublicationGuard>>,
) -> Result<DeferredToolDispatch, OrchestratorError> {
    if publication_fence
        .as_ref()
        .is_some_and(|fence| !fence.is_current())
    {
        return Ok(DeferredToolDispatch::default());
    }
    let mut results = Vec::with_capacity(tool_uses.len());
    let mut publications = Vec::with_capacity(tool_uses.len());
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
    for (tool_index, (tool_use_id, name, input, provider_id)) in tool_uses.iter().enumerate() {
        let tool_dispatch_facts =
            dispatch_facts_for_tool_index(tool_uses, dispatch_facts.as_ref(), tool_index);
        let suppress_virtual_output = active_mod_result_stage_is_virtual().await;
        if !suppress_virtual_output {
            orch.output.emit_tool_call(tool_use_id, name, input).await;
        }

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
        let tool_handle = match prepared_tool.as_ref() {
            Some(prepared)
                if prepared.name() == name
                    || prepared
                        .aliases()
                        .iter()
                        .any(|alias| *alias == name.as_str()) =>
            {
                Some(std::sync::Arc::clone(prepared))
            }
            Some(_) => None,
            None => orch.find_tool_for_dispatch(name),
        };
        let Some(tool_handle) = tool_handle else {
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
            let mut publication = ToolResultPublication::frame_only(
                tool_use_id,
                name,
                &model_text,
                serde_json::json!({ "error": format!("tool not found: {name}") }),
            );
            publication.tool_use_result = Some(serde_json::Value::String(format!(
                "Error: No such tool available: {name}{suffix}"
            )));
            publish_or_defer_tool_result(
                orch,
                publication_fence.as_deref(),
                &mut publications,
                publication,
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
        // A retained scheduler context may belong to a previous sibling call.
        // Only an explicitly associated current-call carrier is admitted here.
        let mut input_projection = match inherited_context
            .as_ref()
            .filter(|context| context.tool_use_id.as_ref() == Some(tool_use_id))
        {
            Some(context) => context
                .projected_input(input)
                .map_err(|error| OrchestratorError::Internal(error.to_string()))?,
            None => Utf16JsonProjection::plain(input.clone()),
        };
        let coerced_input = tool_handle.coerce_input(input);
        let input: &serde_json::Value = coerced_input.as_ref().map_or(input, |c| &c.input);
        let normalized_input = tool_handle.parse_native_input(input).and_then(Result::ok);
        let input = normalized_input.as_ref().unwrap_or(input);
        input_projection
            .rebase_display_value(input.clone())
            .map_err(|error| OrchestratorError::Internal(error.to_string()))?;

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
                is_error: Some(true),
                provider_tool_use_id: provider_id.clone(),
                content_blocks: None,
            };
            // Native .270 persists raw ZodError.message, while only the model
            // block uses the enriched grouped diagnostic (Yge).
            let mut publication = ToolResultPublication::frame_only(
                tool_use_id,
                name,
                &model_text,
                serde_json::json!({ "error": detail }),
            );
            publication.tool_use_result = Some(serde_json::Value::String(format!(
                "InputValidationError: {}",
                schema_error.raw
            )));
            publish_or_defer_tool_result(
                orch,
                publication_fence.as_deref(),
                &mut publications,
                publication,
            )
            .await;
            results.push(result_block);
            continue;
        }

        // Build the per-call physical context from the frozen request history,
        // then overlay call identity/siblings/cancellation on the actor's fixed
        // shared base. Context modifiers are folded only over that shared base.
        let messages = {
            let session = orch.session.lock().await;
            tool_dispatch_facts.as_ref().map_or_else(
                || session.model_context_history(),
                |facts| facts.query_history.clone(),
            )
        };
        let mut ctx = streaming_tool_context_base(orch, messages).await;
        if let Some(consent) = permission_consent.as_ref() {
            ctx.messages
                .push(ConversationMessage::user(MessageId::new(), consent.clone()));
        }
        ctx.tool_use_id = Some(tool_use_id.clone());
        ctx.assistant_message_id = assistant_message_id;
        ctx.assistant_message = tool_dispatch_facts
            .as_ref()
            .map(|facts| facts.assistant_message.clone());
        ctx.same_turn_tool_uses = tool_dispatch_facts
            .as_ref()
            .map(|facts| facts.same_turn_tool_uses.clone())
            .unwrap_or_default();
        ctx.agent_spawn_provenance = agent_spawn_provenance.clone().unwrap_or_default();
        ctx.cancel = cancel.clone();
        if let Some(mut inherited) = inherited_context.clone() {
            inherited.messages = ctx.messages;
            inherited.tool_use_id = ctx.tool_use_id;
            inherited.assistant_message_id = ctx.assistant_message_id;
            inherited.assistant_message = ctx.assistant_message;
            inherited.same_turn_tool_uses = ctx.same_turn_tool_uses;
            inherited.cancel = ctx.cancel;
            inherited.agent_spawn_provenance = ctx.agent_spawn_provenance;
            ctx = inherited;
        }
        ctx.input_projection = Some(input_projection);

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
                is_error: Some(true),
                provider_tool_use_id: provider_id.clone(),
                content_blocks: None,
            };
            // O1: claude's validate_input arm (2.1.220 BIN off 235407190)
            // stamps `` toolUseResult: `Error: ${T.message}` `` — the unwrapped
            // twin of the `<tool_use_error>` model text.
            let mut publication = ToolResultPublication::frame_only(
                tool_use_id,
                name,
                &model_text,
                serde_json::json!({ "error": msg }),
            );
            publication.tool_use_result = Some(serde_json::Value::String(format!("Error: {msg}")));
            publish_or_defer_tool_result(
                orch,
                publication_fence.as_deref(),
                &mut publications,
                publication,
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
                is_error: Some(true),
                provider_tool_use_id: provider_id.clone(),
                content_blocks: None,
            };
            // Denial provenance: claude-code HARDCODES `toolDenialKind:"cancelled"`
            // at this site (binary offset 235399713) rather than routing through
            // its `YDd` abort-reason classifier, so this needs no abort-reason
            // plumbing to be faithful. `cancelled` is not one of the five kinds
            // the permission classifier emits, but it IS an ordinary
            // `toolDenialKind` value that produces a `tool_result_meta` entry.
            let mut publication = ToolResultPublication::frame_only(
                tool_use_id,
                name,
                CANCEL_MESSAGE,
                serde_json::json!({ "error": CANCEL_MESSAGE }),
            );
            publication.denial_kind = Some("cancelled".into());
            publication.tool_use_result = Some(serde_json::Value::String(CANCEL_MESSAGE.into()));
            if let Some(frame) = publication.frame.as_mut() {
                frame.denial_kind = Some("cancelled".into());
            }
            publish_or_defer_tool_result(
                orch,
                publication_fence.as_deref(),
                &mut publications,
                publication,
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
            model_selection: Some(hooks::HookModelSelection {
                model: ctx.options.main_loop_model.clone(),
                model_profile: ctx.options.model_profile.clone(),
            }),
            inherit: orch.hook_agent_inheritance.clone(),
            agent_depth: Some(ctx.depth),
            session_id,
            cwd: orch.current_cwd(),
            transcript_path,
            prompt_id,
            permission_mode,
            trace_context: telemetry::otel::capture_current_trace_context(),
            publication_guard: publication_fence.clone(),
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
        let pre_agg =
            super::boxed_turn_future(|| orch.hooks.execute(pre_event, hook_ctx.clone())).await;
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
        apply_terminal_sequence(
            orch,
            name,
            pre_agg.terminal_sequence.as_deref(),
            publication_fence.clone(),
        )
        .await;
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
            let attachment = hooks::additional_context_attachment(
                &format!("PreToolUse:{name}"),
                tool_use_id.as_str(),
                "PreToolUse",
                &pre_hook_messages,
            );
            orch.queue_hook_attachment(tool_use_id, attachment, publication_fence.clone())
                .await;
            let body = hooks::ExactHookText::join(&pre_hook_messages, "\n");
            let wrapped = hooks::ExactHookText::wrapped(
                &format!("<system-reminder>\nPreToolUse:{name} hook additional context: "),
                &body,
                "\n</system-reminder>",
            );
            // O3: the model-facing rendering is `zr({content: Ww(…),
            // isMeta:true})` (renderer table BIN off 238107100) and is
            // EPHEMERAL — built from the attachment at API-normalization time
            // and never persisted. `user_meta` marks it so both drivers skip
            // persisting it; the attachment above IS the on-disk record.
            let message = ConversationMessage::user_meta_js_utf16(
                MessageId::new(),
                wrapped.display,
                wrapped.utf16_code_units,
            );
            register_mod_persisted_attachment_if_visible(
                orch,
                &message,
                "hook_additional_context",
                serde_json::json!({"kind":"hook","event":"PreToolUse"}),
                publication_fence.as_ref().map(|fence| fence.as_ref()),
            )
            .await;
            Some(message)
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
            let batch_tool_count = original_batch_tool_count;
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
                let attachment = hooks::deferred_tool_attachment(
                    tool_use_id.as_str(),
                    name,
                    &deferred_input,
                    &hook_name,
                    permission_mode,
                    hook_ctx
                        .trace_context
                        .as_ref()
                        .map(|context| context.traceparent.as_str()),
                );
                let persistence =
                    orch.persist_hook_attachment_to_jsonl(attachment, Default::default());
                if let Some(fence) = publication_fence.as_deref() {
                    fence.commit_if_current(Box::pin(persistence)).await;
                } else {
                    persistence.await;
                }
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
                is_error: Some(true),
                provider_tool_use_id: provider_id.clone(),
                content_blocks: None,
            };
            let publication = ToolResultPublication::frame_only(
                tool_use_id,
                name,
                &model_text,
                serde_json::json!({ "error": model_text.clone() }),
            );
            publish_or_defer_tool_result(
                orch,
                publication_fence.as_deref(),
                &mut publications,
                publication,
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
        if pre_agg.modified_input.is_some() {
            // A hook response is a new source; it cannot inherit original
            // exact units simply because its lossy display text happens to match.
            ctx.replace_input(Utf16JsonProjection::plain(effective_input.clone()))
                .map_err(|error| OrchestratorError::Internal(error.to_string()))?;
        }
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
        // HOOK.3 resolution: capture the mode-less rule/safety result once
        // before the tool-owned permission check. A Mod wraps that exact result;
        // a no-Mod terminal Allow/Deny reuses it, while Ask enters the full
        // permission pipeline. The ordinary non-hook path still resolves through
        // the active gate and may delegate Ask to its prompt transport.
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
        // `bP` checks explicit rule/safety objections before invoking the
        // tool's own permission checker on a PreToolUse-approved call. Keep
        // the mode-less verdict available to the subsequent Mod chain.
        let tool_check_ceiling = tool_handle
            .tool_check_permission_ceiling(&effective_input)
            .await;
        let hook_preflight = if hook_allowed && !plan_mode {
            let policy_ctx = lingxi_core::host::permission_gate::PermissionCheckContext {
                input_projection: Some(
                    ctx.projected_input(&effective_input)
                        .map_err(|error| OrchestratorError::Internal(error.to_string()))?,
                ),
                tool_use_id: Some(tool_use_id.to_string()),
                tool_check_ceiling,
                requires_user_interaction,
                suppress_always_allow_rule: requires_user_interaction
                    || restricted_protected_mutation,
                ..Default::default()
            };
            Some(
                orch.perms
                    .resolve_after_hook_allow_mod_core(name, &effective_input, &policy_ctx)
                    .await
                    .map_err(|abort| OrchestratorError::PermissionAbort {
                        message: abort.message,
                    })?,
            )
        } else {
            None
        };
        // PreToolUse-approved and forced recovery calls have their own
        // resolver branches below. Ordinary calls defer the tool-owned check
        // along with policy resolution until a Mod invokes core via `next(e)`.
        let eager_tool_check = hook_allowed
            && !plan_mode
            && !hook_preflight.as_ref().is_some_and(|preflight| {
                matches!(&preflight.resolution, PermissionResolution::Deny { .. })
            });
        let mut tool_permission_result = if eager_tool_check {
            Some(tool_handle.check_permissions(&effective_input, &ctx).await)
        } else {
            None
        };
        let mut tool_ask_is_protected = tool_permission_result
            .as_ref()
            .is_some_and(|result| tool_permission_ask_is_protected(tool_handle.as_ref(), result));
        // Native `LNo` routes a PreToolUse allow whose rule/safety preflight
        // asks (or whose protected tool check asks) through `canUseTool`'s full
        // lazy ToolCheck pipeline. A clean/deny preflight instead uses `nue`
        // with that captured decision as the Mod core.
        let hook_allow_needs_full_permission_pipeline = hook_allowed
            && !plan_mode
            && (hook_preflight.as_ref().is_some_and(|preflight| {
                matches!(
                    &preflight.resolution,
                    PermissionResolution::Ask | PermissionResolution::AskWithContext { .. }
                )
            }) || (tool_ask_is_protected
                && tool_permission_result.as_ref().is_some_and(|result| {
                    matches!(result, permission::PermissionResult::Ask { .. })
                })));
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
        let mut mod_changed_tool_check_decision = false;
        let decision = if let Some(forced) = forced_decision {
            // `oVo` wraps the ordinary permission evaluator with `LPr` even
            // when orphan recovery supplies a saved canUseTool response. A
            // direct Mod answer can therefore avoid both policy evaluation and
            // the tool-owned check before the saved answer is considered.
            decision_otel_source = "unknown";
            let (checked, _, _, updated_input, _, mod_changed_decision) =
                apply_mod_tool_check_lazy_normal(
                    orch,
                    name,
                    &effective_input,
                    tool_use_id,
                    tool_handle.as_ref(),
                    &ctx,
                    tool_check_ceiling,
                    plan_mode,
                    hook_ask,
                    requires_user_interaction,
                    restricted_protected_mutation,
                    publication_fence.clone(),
                )
                .await?;
            mod_changed_tool_check_decision = mod_changed_decision;
            if let Some(updated) = updated_input {
                effective_input = updated;
                ctx.rebase_input(&effective_input)
                    .map_err(|error| OrchestratorError::Internal(error.to_string()))?;
            }
            // The lazy core already composed tool-owned deny/ask. Reapplying
            // that result below would undo a Mod's allowed override.
            tool_permission_result = None;
            tool_ask_is_protected = false;
            match checked {
                PermissionResolution::Deny {
                    reason,
                    decision_reason_type,
                    decision_reason,
                    behavior_ask,
                    ..
                } => {
                    if mod_changed_decision {
                        decision_otel_source = "hook";
                    }
                    denial_kind = tool_denial_kind(
                        behavior_ask,
                        decision_reason_type.as_deref(),
                        decision_reason.as_deref(),
                    );
                    PermissionDecision::Deny { reason }
                }
                PermissionResolution::Allow { .. }
                | PermissionResolution::Ask
                | PermissionResolution::AskWithContext { .. } => forced,
            }
        } else if hook_allowed && !plan_mode && !hook_allow_needs_full_permission_pipeline {
            let tool_permission_result = tool_permission_result.as_ref();
            // Carry the REAL tool_use_id so a hook-allow→ask-rule re-check emits a
            // byte-faithful stdio `can_use_tool` (correlatable id + decision_reason).
            let permission_ctx = lingxi_core::host::permission_gate::PermissionCheckContext {
                input_projection: Some(
                    ctx.projected_input(&effective_input)
                        .map_err(|error| OrchestratorError::Internal(error.to_string()))?,
                ),
                tool_use_id: Some(tool_use_id.to_string()),
                requires_user_interaction,
                suppress_always_allow_rule: requires_user_interaction
                    || restricted_protected_mutation,
                ..Default::default()
            };
            // The captured preflight is the rule/safety result for this direct
            // hook-approved call. An installed Mod wraps that exact result; when
            // no Mod host exists, the outcome arm below consumes the captured
            // terminal Allow/Deny rather than querying live policy again after
            // the tool-owned check. Captured Ask uses the full pipeline above.
            let has_mod_host = if let Some(registry) = &orch.lifecycle_runtime.hook_registry {
                registry.read().await.mod_host().is_some()
            } else {
                false
            };
            let mod_override = if has_mod_host {
                let preflight = hook_preflight
                    .as_ref()
                    .expect("eligible PreToolUse allow has a captured Mod core");
                let mut core = preflight.resolution.clone();
                let tool_owned_binding = matches!(
                    tool_permission_result,
                    Some(permission::PermissionResult::Deny { .. })
                ) || (tool_ask_is_protected
                    && matches!(
                        tool_permission_result,
                        Some(permission::PermissionResult::Ask { .. })
                    ));
                if let Some(permission::PermissionResult::Deny {
                    reason,
                    explanation,
                    ..
                }) = tool_permission_result
                {
                    core = tool_permission_deny_resolution(name, reason, explanation.as_deref());
                } else if tool_ask_is_protected
                    && matches!(
                        tool_permission_result,
                        Some(permission::PermissionResult::Ask { .. })
                    )
                {
                    core = PermissionResolution::Ask;
                }
                let checked = apply_mod_tool_check(
                    orch,
                    name,
                    &effective_input,
                    tool_use_id,
                    core.clone(),
                    tool_permission_result,
                    preflight.evaluation.as_ref(),
                    restricted_protected_mutation,
                    ctx.agent_id.as_ref().map(ToString::to_string),
                    tool_check_ceiling,
                    publication_fence.clone(),
                )
                .await;
                let mod_changed = checked != core;
                let captured_preflight_is_terminal = matches!(
                    &core,
                    PermissionResolution::Allow { .. } | PermissionResolution::Deny { .. }
                );
                (tool_owned_binding || mod_changed || captured_preflight_is_terminal)
                    .then_some((checked, mod_changed))
            } else {
                None
            };
            let mod_changed_hook_decision =
                mod_override.as_ref().is_some_and(|(_, changed)| *changed);
            mod_changed_tool_check_decision = mod_changed_hook_decision;
            let hook_outcome = match mod_override.map(|(resolution, _)| resolution) {
                Some(PermissionResolution::Allow { .. }) => {
                    lingxi_core::host::permission_gate::PermissionOutcome::Allow {
                        updated_input: None,
                        permission_updates: Vec::new(),
                        decision_classification: None,
                    }
                }
                Some(PermissionResolution::Deny {
                    reason,
                    decision_reason_type,
                    decision_reason,
                    behavior_ask,
                    ..
                }) => {
                    denial_kind = tool_denial_kind(
                        behavior_ask,
                        decision_reason_type.as_deref(),
                        decision_reason.as_deref(),
                    );
                    lingxi_core::host::permission_gate::PermissionOutcome::Deny { reason }
                }
                Some(PermissionResolution::Ask) => {
                    orch.perms
                        .ask_via_transport(name, &effective_input, &permission_ctx)
                        .await
                }
                Some(PermissionResolution::AskWithContext {
                    decision_reason_type,
                    decision_reason,
                }) => {
                    let ask_ctx = lingxi_core::host::permission_gate::PermissionCheckContext {
                        decision_reason_type,
                        decision_reason,
                        ..permission_ctx.clone()
                    };
                    orch.perms
                        .ask_via_transport(name, &effective_input, &ask_ctx)
                        .await
                }
                None => {
                    // There is no Mod host to wrap a fresh core query, so keep the
                    // exact terminal decision captured before the tool-owned check.
                    // Re-querying PolicyPermissionGate here could observe a live
                    // permission update after the hook-approved call had already
                    // passed its preflight. Ask results have already been routed to
                    // the full permission pipeline above.
                    let preflight = hook_preflight
                        .as_ref()
                        .expect("direct PreToolUse allow has a captured permission core");
                    match &preflight.resolution {
                        PermissionResolution::Allow { .. } => {
                            lingxi_core::host::permission_gate::PermissionOutcome::Allow {
                                updated_input: None,
                                permission_updates: Vec::new(),
                                decision_classification: None,
                            }
                        }
                        PermissionResolution::Deny { reason, .. } => {
                            lingxi_core::host::permission_gate::PermissionOutcome::Deny {
                                reason: reason.clone(),
                            }
                        }
                        PermissionResolution::Ask | PermissionResolution::AskWithContext { .. } => {
                            unreachable!(
                                "captured Ask results use the full hook-allow permission pipeline"
                            )
                        }
                    }
                }
            };
            let mut hook_decision_classification = None;
            let hook_decision = match hook_outcome {
                lingxi_core::host::permission_gate::PermissionOutcome::Allow {
                    updated_input,
                    decision_classification,
                    permission_updates: _,
                } => {
                    hook_decision_classification = decision_classification;
                    if let Some(updated) = updated_input {
                        effective_input = ctx
                            .replace_input(updated)
                            .map_err(|error| OrchestratorError::Internal(error.to_string()))?;
                    }
                    PermissionDecision::Allow
                }
                lingxi_core::host::permission_gate::PermissionOutcome::AllowAuto {
                    updated_input,
                } => {
                    if let Some(updated) = updated_input {
                        effective_input = ctx
                            .replace_input(updated)
                            .map_err(|error| OrchestratorError::Internal(error.to_string()))?;
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
                            lingxi_core::host::permission_gate::ToolDecisionClassification::UserTemporary,
                        );
                    }
                    PermissionDecision::Allow
                }
                lingxi_core::host::permission_gate::PermissionOutcome::Deny { reason } => {
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
            decision_otel_source = if mod_changed_hook_decision {
                "hook"
            } else if matches!(hook_decision, PermissionDecision::Allow) {
                hook_decision_classification.map_or(
                    "hook",
                    lingxi_core::host::permission_gate::ToolDecisionClassification::as_str,
                )
            } else {
                "config"
            };
            hook_decision
        } else {
            // NORMAL permission path. The Mod chain wraps a lazy core callback:
            // a hook that answers directly need not run the rule/classifier
            // resolver, while `next(e)` computes its sourced decision before
            // any PermissionRequest or dialog handling below.
            let (
                resolution,
                tool_ask_reason,
                normal_tool_permission,
                updated_input,
                normal_tool_check_evaluation,
                mod_changed_decision,
            ) = apply_mod_tool_check_lazy_normal(
                orch,
                name,
                &effective_input,
                tool_use_id,
                tool_handle.as_ref(),
                &ctx,
                tool_check_ceiling,
                plan_mode,
                hook_ask,
                requires_user_interaction,
                restricted_protected_mutation,
                publication_fence.clone(),
            )
            .await?;
            mod_changed_tool_check_decision = mod_changed_decision;
            if let Some(updated) = updated_input {
                effective_input = updated;
                ctx.rebase_input(&effective_input)
                    .map_err(|error| OrchestratorError::Internal(error.to_string()))?;
            }
            tool_permission_result = normal_tool_permission;
            tool_ask_is_protected = tool_permission_result.as_ref().is_some_and(|result| {
                tool_permission_ask_is_protected(tool_handle.as_ref(), result)
            });
            let tool_ask_blocks_hook_rescue =
                tool_permission_result.as_ref().is_some_and(|result| {
                    tool_permission_ask_blocks_hook_rescue(tool_handle.as_ref(), result)
                });
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
                    decision_otel_source = if mod_changed_decision {
                        "hook"
                    } else {
                        rule_decision_otel_source(rule_source.as_deref(), true)
                    };
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
                    decision_otel_source = if mod_changed_decision {
                        "hook"
                    } else {
                        rule_decision_otel_source(rule_source.as_deref(), false)
                    };
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
                        let denied_agg = super::boxed_turn_future(|| {
                            orch.hooks.execute(denied_event, hook_ctx.clone())
                        })
                        .await;
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
                    let sysmsg_ctx = lingxi_core::host::permission_gate::PermissionCheckContext {
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
                    // PermissionRequest hook FIRST. A rewritten/required
                    // interaction allow uses the dedicated rewritten resolver;
                    // a standing allow is honored directly. A hook deny denies;
                    // without a hook decision, delegate to the inner transport.
                    let req_event = HookEvent::PermissionRequest {
                        tool_name: name.clone(),
                        tool_input: effective_input.clone(),
                        reason: format!("Tool {name} requires permission"),
                    };
                    let req_agg = super::boxed_turn_future(|| {
                        orch.hooks.execute(req_event, hook_ctx.clone())
                    })
                    .await;
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
                                effective_input = ctx
                                    .replace_input(Utf16JsonProjection::plain(updated))
                                    .map_err(|error| {
                                        OrchestratorError::Internal(error.to_string())
                                    })?;
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
                                let permission_ctx =
                                    lingxi_core::host::permission_gate::PermissionCheckContext {
                                        input_projection: Some(
                                            ctx.projected_input(&effective_input).map_err(
                                                |error| {
                                                    OrchestratorError::Internal(error.to_string())
                                                },
                                            )?,
                                        ),
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
                                let outcome = if tool_ask_reason.is_some()
                                    || ask_reason_context.0.as_deref() == Some("hook")
                                {
                                    orch.perms
                                        .ask_via_transport(name, &effective_input, &permission_ctx)
                                        .await
                                } else if let Some(evaluation) =
                                    normal_tool_check_evaluation.as_ref()
                                {
                                    orch.perms
                                        .ask_tool_check_via_transport(
                                            name,
                                            &effective_input,
                                            &permission_ctx,
                                            evaluation,
                                            &resolution,
                                        )
                                        .await
                                } else {
                                    orch.perms
                                        .check_with_context(name, &effective_input, &permission_ctx)
                                        .await
                                };
                                match outcome {
                                    lingxi_core::host::permission_gate::PermissionOutcome::Allow {
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
                                        lingxi_core::host::permission_gate::ToolDecisionClassification::as_str,
                                    );
                                        if let Some(u) = updated_input {
                                            effective_input = ctx.replace_input(u).map_err(|error| OrchestratorError::Internal(error.to_string()))?;
                                        }
                                        PermissionDecision::Allow
                                    }
                                    lingxi_core::host::permission_gate::PermissionOutcome::AllowAuto {
                                        updated_input,
                                    } => {
                                        if let Some(u) = updated_input {
                                            effective_input = ctx.replace_input(u).map_err(|error| OrchestratorError::Internal(error.to_string()))?;
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
                                    lingxi_core::host::permission_gate::PermissionOutcome::Deny { reason } => {
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
        // existing gate entrypoints. A tool-local DENY still binds. Keep an
        // unchanged tool-owned ASK on its normal prompt path, while a changed
        // Mod verdict follows Native's separate exact hard-hold predicate in
        // the ToolCheck helper instead of being vetoed by `is_mcp()` here. A
        // headless owner fails closed rather than handing an unchanged Ask to
        // a transport that cannot represent the prompt.
        let decision = if !non_normal_permission_path {
            decision
        } else {
            match tool_permission_result.as_ref() {
                Some(permission::PermissionResult::Deny { explanation, .. }) => match decision {
                    PermissionDecision::Deny { .. } => decision,
                    PermissionDecision::Allow
                        if mod_changed_tool_check_decision && !restricted_protected_mutation =>
                    {
                        decision
                    }
                    PermissionDecision::Allow => PermissionDecision::Deny {
                        reason: explanation.as_deref().map_or_else(
                            || format!("Permission to use {name} has been denied."),
                            str::to_string,
                        ),
                    },
                },
                Some(permission::PermissionResult::Ask { reason, .. })
                    if tool_ask_is_protected && !mod_changed_tool_check_decision =>
                {
                    if matches!(decision, PermissionDecision::Deny { .. }) {
                        decision
                    } else if !orch.config.interactive_permissions {
                        PermissionDecision::Deny {
                            reason: format!("Permission to use {name} has been denied."),
                        }
                    } else {
                        let (decision_reason_type, decision_reason) =
                            tool_ask_reason_context(reason);
                        let ask_ctx = lingxi_core::host::permission_gate::PermissionCheckContext {
                            input_projection: Some(
                                ctx.projected_input(&effective_input).map_err(|error| {
                                    OrchestratorError::Internal(error.to_string())
                                })?,
                            ),
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
                            lingxi_core::host::permission_gate::PermissionOutcome::Allow {
                                updated_input,
                                ..
                            } => {
                                if let Some(updated) = updated_input {
                                    effective_input =
                                        ctx.replace_input(updated).map_err(|error| {
                                            OrchestratorError::Internal(error.to_string())
                                        })?;
                                }
                                PermissionDecision::Allow
                            }
                            lingxi_core::host::permission_gate::PermissionOutcome::AllowAuto {
                                updated_input,
                            } => {
                                if let Some(updated) = updated_input {
                                    effective_input =
                                        ctx.replace_input(updated).map_err(|error| {
                                            OrchestratorError::Internal(error.to_string())
                                        })?;
                                }
                                PermissionDecision::Allow
                            }
                            lingxi_core::host::permission_gate::PermissionOutcome::Deny {
                                reason,
                            } => PermissionDecision::Deny { reason },
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
                    is_error: Some(true),
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
                let mut publication = ToolResultPublication::frame_only(
                    tool_use_id,
                    name,
                    &reason,
                    serde_json::json!({ "error": reason }),
                );
                publication.denial_kind = Some(denial_kind.to_owned());
                publication.permission_denial = Some((name.to_owned(), effective_input.clone()));
                publication.tool_use_result =
                    Some(serde_json::Value::String(format!("Error: {reason}")));
                if let Some(frame) = publication.frame.as_mut() {
                    frame.denial_kind = Some(denial_kind.to_owned());
                }
                publish_or_defer_tool_result(
                    orch,
                    publication_fence.as_deref(),
                    &mut publications,
                    publication,
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
        // subagent's work renders under its Agent cell. Other tools send nothing,
        // so this is a no-op for them. The consumer exits when the tool drops
        // `progress_tx` (call returns).
        let (progress_tx, mut progress_rx) =
            tokio::sync::mpsc::channel::<tool_api::progress::ToolProgress>(64);
        let progress_output = orch.output.clone();
        let progress_fence = publication_fence.clone();
        // The spawning Agent tool_use_id — stamped as `parent_tool_use_id` on any
        // forwarded subagent assistant frame (`--forward-subagent-text`). Uses
        // the same `ToolUseId::as_str` form the stream-json tool_use block id
        // carries, so a forwarded child frame correlates to its parent Agent call.
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
                            if !suppress_virtual_output {
                                let publication = forward_tool_progress(
                                    progress_output.as_ref(),
                                    &progress_parent_tool_use_id,
                                    progress,
                                );
                                if let Some(fence) = progress_fence.as_ref() {
                                    fence.publish_if_current(Box::pin(publication)).await;
                                } else {
                                    publication.await;
                                }
                            }
                        }
                        break;
                    }
                    progress = progress_rx.recv() => {
                        let Some(progress) = progress else { break; };
                        if !suppress_virtual_output {
                            let publication = forward_tool_progress(
                                progress_output.as_ref(),
                                &progress_parent_tool_use_id,
                                progress,
                            );
                            if let Some(fence) = progress_fence.as_ref() {
                                fence.publish_if_current(Box::pin(publication)).await;
                            } else {
                                publication.await;
                            }
                        }
                    }
                }
            }
        });
        // Periodic tool heartbeat for long-running calls: transports that care
        // can surface "still running" state between ToolCall and ToolResult,
        // while sinks that ignore it keep the default no-op behavior.
        let heartbeat_output = orch.output.clone();
        let heartbeat_fence = publication_fence.clone();
        let heartbeat_id = tool_use_id.clone();
        let heartbeat_tool = name.to_string();
        let _heartbeat_cancel = cancel.clone();
        let heartbeat_done = event_tasks_done.clone();
        let heartbeat_started = std::time::Instant::now();
        if !suppress_virtual_output {
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
                        let publication = heartbeat_output
                            .emit_tool_heartbeat(&heartbeat_id, &heartbeat_tool, elapsed_ms);
                        let current = if let Some(fence) = heartbeat_fence.as_ref() {
                            fence.publish_if_current(publication).await
                        } else {
                            publication.await;
                            true
                        };
                        if !current {
                            break;
                        }
                    }
                }
            }
            });
        }

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
        let result_context = ctx.clone();
        let (execution_record, tool_outcome) = {
            // Hooks and approval may rewrite parameters after the initial
            // gates. Validate the exact input that is about to execute.
            let final_validation = match crate::schema_validation::validate_tool_schema_detailed(
                tool_handle.as_ref(),
                &effective_input,
            ) {
                Err(error) => Err(tool_api::ToolError::InvalidInput(format!(
                    "InputValidationError: {}",
                    error.display
                ))),
                Ok(()) => tool_handle
                    .validate_input(&effective_input, &ctx)
                    .await
                    .map_err(|tool_api::ValidationError(message)| {
                        tool_api::ToolError::InvalidInput(message)
                    }),
            };
            let execution_record = match final_validation {
                Ok(()) => {
                    Box::pin(crate::native_computer::before_execution(
                        orch,
                        tool_use_id,
                        &tool_handle,
                        &effective_input,
                        &ctx,
                    ))
                    .await
                }
                Err(error) => Err(error),
            };
            match execution_record {
                Err(error) => (None, Err(error)),
                Ok(record) => {
                    // Keep the call future in this inner scope. When cancellation wins,
                    // leaving the scope drops the non-cooperative future (and its
                    // progress sender) before we await the progress consumer below.
                    let tool_call = tool_handle.call(effective_input.clone(), ctx, progress_tx);
                    tokio::pin!(tool_call);
                    let outcome = match (interrupt_behavior, dispatch_cancel) {
                        (tool_api::tool_trait::InterruptBehavior::Cancel, Some(cancel)) => {
                            tokio::select! {
                                biased;
                                () = cancel.cancelled() => Err(tool_api::ToolError::Aborted),
                                outcome = &mut tool_call => outcome,
                            }
                        }
                        _ => tool_call.await,
                    };
                    (record, outcome)
                }
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
        // Persist the execution fact before PostToolUse can replace its output.
        // A failed terminal write leaves Started unresolved and must stop the
        // turn so recovery cannot replay an input with an unknown outcome.
        Box::pin(crate::native_computer::after_execution(
            orch,
            execution_record,
            &tool_outcome,
        ))
        .await
        .map_err(|error| OrchestratorError::Internal(error.to_string()))?;
        Box::pin(crate::native_computer::note_computer_result(
            orch,
            &tool_handle,
            &result_context,
            tool_outcome.as_ref().is_ok_and(|result| !result.is_error),
        ))
        .await;
        #[allow(clippy::cast_possible_truncation)]
        let tool_duration_ms = tool_started.elapsed().as_millis() as u64;

        let (content, is_error, emit_payload, is_abort, tool_use_result, mcp_meta, turn_end) =
            match tool_outcome {
                Ok(result) => {
                    // The 2.1.286 AGENTS plugin wraps successful Read calls, not
                    // text-read state. Its context is a durable tool.call hook
                    // attachment, rendered beside the result and retained in live
                    // history by the existing injected-message pipeline.
                    if name == "Read" && !result.is_error {
                        let contexts = orch.agents_context_after_read(&effective_input).await;
                        if !contexts.is_empty() {
                            let exact_contexts = contexts
                                .iter()
                                .cloned()
                                .map(hooks::ExactHookText::from_text)
                                .collect::<Vec<_>>();
                            let attachment = hooks::additional_context_attachment(
                                "tool.call",
                                &format!("{}-context", tool_use_id.as_str()),
                                "PostToolUse",
                                &exact_contexts,
                            );
                            orch.queue_hook_attachment(
                                tool_use_id,
                                attachment,
                                publication_fence.clone(),
                            )
                            .await;
                            let body = hooks::ExactHookText::join(&exact_contexts, "\n");
                            let wrapped = hooks::ExactHookText::wrapped(
                                "<system-reminder>\ntool.call hook additional context: ",
                                &body,
                                "\n</system-reminder>",
                            );
                            injected_messages.push((
                                ConversationMessage::user_meta_js_utf16(
                                    MessageId::new(),
                                    wrapped.display,
                                    wrapped.utf16_code_units,
                                ),
                                tool_use_id.clone(),
                            ));
                        }
                    }
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
                    // O1: carry the RAW structured result and MCP sidecars to Tn;
                    // Native publishes them only after the executor accepts the row.
                    let raw_result = result.data.clone();
                    let mcp_meta = result.mcp_meta.clone();
                    // `is_error` rides on the result (set by MCP tools from the
                    // server's `isError`; `false` for every native success). A native
                    // FAILURE is an `Err` handled below — this Ok arm only flags an
                    // MCP logical-error RESULT.
                    (
                        text,
                        result.is_error,
                        result.data,
                        false,
                        Some(raw_result),
                        mcp_meta,
                        turn_end,
                    )
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
                    let emit_payload = match &err {
                        tool_api::ToolError::SubagentFailed {
                            agent_id,
                            terminal_hooks_owned,
                            ..
                        } => {
                            serde_json::json!({
                                "error": bare,
                                "agentId": agent_id.as_uuid().to_string(),
                                "subagentStopHooksOwned": terminal_hooks_owned,
                            })
                        }
                        _ => serde_json::json!({ "error": bare }),
                    };
                    // O1: on the ERROR arm claude stores the plain STRING
                    // `` `Error: ${ae}` `` in `toolUseResult` (2.1.220 BIN off
                    // 235424595), NOT a structured object. Carry both wires to Tn.
                    (
                        text.clone(),
                        true,
                        emit_payload,
                        is_abort,
                        Some(serde_json::Value::String(text)),
                        None,
                        None,
                    )
                }
            };

        // Native carries W1's result through the actor and publishes it only
        // when Tn accepts the row. A discarded generation drops this payload.
        let denial_kind = is_abort.then(|| "interrupted".to_owned());
        let mut publication =
            ToolResultPublication::frame_only(tool_use_id, name, &content, emit_payload.clone());
        publication.tool_use_result = tool_use_result;
        publication.mcp_meta = mcp_meta;
        publication.turn_end = turn_end;
        publication.denial_kind = denial_kind.clone();
        if let Some(frame) = publication.frame.as_mut() {
            frame.denial_kind = denial_kind;
        }
        publish_or_defer_tool_result(
            orch,
            publication_fence.as_deref(),
            &mut publications,
            publication,
        )
        .await;

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
        let post_agg =
            super::boxed_turn_future(|| orch.hooks.execute(post_event, hook_ctx.clone())).await;
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
            post_additional_contexts.push(notice.text.into());
        }

        // #40 terminalSequence apply for the post-dispatch aggregate (claude-code
        // `szn` runs per hook result, all event types). Same as the PreToolUse
        // side: validate, warn on rejection, and write accepted bytes through
        // the active terminal bridge.
        apply_terminal_sequence(
            orch,
            name,
            post_agg.terminal_sequence.as_deref(),
            publication_fence.clone(),
        )
        .await;

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
                Utf16JsonProjection::plain(hooks::blocking_error_attachment(
                    &hooks::HookAttachmentIdentity {
                        hook_name: post_hook_name.clone(),
                        hook_event: post_hook_event.to_string(),
                        tool_use_id: tool_use_id.as_str().to_string(),
                    },
                    &err,
                )),
                publication_fence.clone(),
            )
            .await;
            let body = hooks::blocking_error_prose(&post_hook_name, &err);
            let message = ConversationMessage::user_meta(
                MessageId::new(),
                format!("<system-reminder>\n{body}\n</system-reminder>"),
            );
            register_mod_persisted_attachment_if_visible(
                orch,
                &message,
                "hook_blocking_error",
                serde_json::json!({"kind":"hook","event":post_hook_event}),
                publication_fence.as_ref().map(|fence| fence.as_ref()),
            )
            .await;
            injected_messages.push((message, tool_use_id.clone()));
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
                Utf16JsonProjection::plain(hooks::stopped_continuation_attachment(
                    &hooks::HookAttachmentIdentity {
                        hook_name: post_hook_name.clone(),
                        hook_event: post_hook_event.to_string(),
                        tool_use_id: tool_use_id.as_str().to_string(),
                    },
                    &reason,
                )),
                publication_fence.clone(),
            )
            .await;
            let message = ConversationMessage::user_meta(
                MessageId::new(),
                format!(
                    "<system-reminder>\n{post_hook_name} hook stopped continuation: {reason}\n</system-reminder>"
                ),
            );
            register_mod_persisted_attachment_if_visible(
                orch,
                &message,
                "hook_stopped_continuation",
                serde_json::json!({"kind":"hook","event":post_hook_event}),
                publication_fence.as_ref().map(|fence| fence.as_ref()),
            )
            .await;
            injected_messages.push((message, tool_use_id.clone()));
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
        // (BIN off 234726655); its renderer joins entries with `\n`. Keep the
        // original attachment in the transcript and screen its single outgoing
        // message through prompt.attachment.
        if !post_additional_contexts.is_empty() {
            let attachment = hooks::additional_context_attachment(
                &post_hook_name,
                tool_use_id.as_str(),
                post_hook_event,
                &post_additional_contexts,
            );
            orch.queue_hook_attachment(tool_use_id, attachment, publication_fence.clone())
                .await;
            let body = hooks::ExactHookText::join(&post_additional_contexts, "\n");
            let wrapped = hooks::ExactHookText::wrapped(
                &format!("<system-reminder>\n{post_hook_name} hook additional context: "),
                &body,
                "\n</system-reminder>",
            );
            // `user_meta`: the rendering is `zr({isMeta:true})` and is
            // ephemeral — the attachment line above is the on-disk record.
            let message = ConversationMessage::user_meta_js_utf16(
                MessageId::new(),
                wrapped.display,
                wrapped.utf16_code_units,
            );
            register_mod_persisted_attachment_if_visible(
                orch,
                &message,
                "hook_additional_context",
                serde_json::json!({"kind":"hook","event":post_hook_event}),
                publication_fence.as_ref().map(|fence| fence.as_ref()),
            )
            .await;
            injected_messages.push((message, tool_use_id.clone()));
        }

        // PostToolUse `updatedToolOutput` (#38, all-tools) + `updatedMCPToolOutput`
        // (legacy, MCP-only) may REPLACE a SUCCESSFUL tool's output. claude yields
        // all-tools first, MCP second so MCP overrides (BIN off 202157140); applies
        // only if `outputSchema` is absent or validates (BIN off 202169384), else
        // keeps original + emits `hook_error_during_execution` (BIN off 202465455).
        // The substituted JSON passes through the tool's own result mapper.
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
                    Ok(()) => (
                        tool_handle
                            .map_result_text(&new_output)
                            .unwrap_or_else(|| tool_result_to_model_text(&new_output)),
                        true,
                    ),
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
                            Utf16JsonProjection::plain(hooks::error_during_execution_attachment(
                                &msg,
                                &format!("PostToolUse:{name}"),
                                tool_use_id.as_str(),
                                "PostToolUse",
                            )),
                            publication_fence.clone(),
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
            let _wt_agg =
                super::boxed_turn_future(|| orch.hooks.execute(wt_event, hook_ctx.clone())).await;
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
        // The Agent tool surfaces the allocated child's identity on success and
        // in a typed terminal error. Admission failures have no child, so they
        // must not manufacture lifecycle events or evaluate parent history.
        //
        // SINGLE-FIRE (R7): the real tool's runner ALREADY fires SubagentStart
        // (runAgent.ts:530-555) + the child's frontmatter SubagentStop, marking
        // `data.subagentHooksFired`. So: skip the chokepoint SubagentStart when the
        // runner fired it; fire only the COMPLEMENT
        // SubagentStop via `execute_excluding_agent(child_id)` (omits the re-fired
        // frontmatter bucket, race-free vs `clear_agent_hooks`).
        if name == AGENT_TOOL_NAME
            && emit_payload
                .get("isAsync")
                .and_then(serde_json::Value::as_bool)
                != Some(true)
            && emit_payload
                .get("subagentStopHooksOwned")
                .and_then(serde_json::Value::as_bool)
                != Some(true)
        {
            let subagent_type = effective_input
                .get("subagent_type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            let real_agent_id = emit_payload
                .get("agentId")
                .and_then(serde_json::Value::as_str)
                .and_then(lingxi_core::types::AgentId::parse_prefixed);
            // Completed results report the start lifecycle already owned by the
            // runner. A failed allocated child may have stopped before startup;
            // never manufacture a start event after that terminal failure.
            let runner_fired_start = emit_payload
                .get("subagentHooksFired")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            // Fusion results describe multiple panels and also lack one child
            // identity; each panel owns its lifecycle hooks.
            if let Some(child_id) = real_agent_id {
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
                // A failed startup never reaches the runner's start hook.
                if !runner_fired_start && !is_error {
                    let start_event = HookEvent::SubagentStart {
                        agent_id: child_id,
                        agent_type: subagent_type,
                        parent_agent_id: None,
                    };
                    let _start_agg = super::boxed_turn_future(|| {
                        orch.hooks.execute(start_event, sa_ctx.clone())
                    })
                    .await;
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
                    "fired chokepoint SubagentStart (if runner didn't) + session/plugin SubagentStop after Agent tool completed",
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
        } else if name == "computer" {
            tool_api::tool_result_media::media_content_blocks_for_tool(
                name,
                &emit_payload,
                &final_content,
            )
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
                    .map(lingxi_core::types::js_utf16::tool_result_sidecar),
            )
        } else {
            (persistence.content, content_blocks)
        };
        results.push(ContentBlock::ToolResult {
            tool_use_id: tool_use_id.clone(),
            content: final_content,
            is_error: Some(is_error),
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
            orch.queue_hook_attachment(
                tool_use_id,
                Utf16JsonProjection::plain(attachment),
                publication_fence.clone(),
            )
            .await;
            register_mod_persisted_attachment_if_visible(
                orch,
                &msg,
                "hook_stopped_continuation",
                serde_json::json!({"kind":"hook","event":"PreToolUse"}),
                publication_fence.as_ref().map(|fence| fence.as_ref()),
            )
            .await;
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
        publications,
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
    let dispatched = dispatch_tool_uses_tracked_deferred(orch, tool_uses, cancel, None).await?;
    finish_direct_tool_dispatch(orch, dispatched).await
}

/// Complete direct dispatch and orphan replay through the same batch hook owner.
pub(crate) async fn finish_direct_tool_dispatch(
    orch: &ConversationOrchestrator,
    mut dispatched: DeferredToolDispatch,
) -> Result<
    (
        Vec<ContentBlock>,
        bool,
        Vec<(ConversationMessage, ToolUseId)>,
        Vec<ContextModifier>,
    ),
    OrchestratorError,
> {
    if !dispatched.prevent_continuation {
        let batch_outcome = run_post_tool_batch_hooks(
            orch,
            super::PostToolBatchDispatch::unguarded(dispatched.post_tool_batch_calls),
        )
        .await;
        dispatched.prevent_continuation |= batch_outcome.prevent_continuation;
        dispatched
            .injected_messages
            .extend(batch_outcome.injected_messages);
    }
    Ok((
        dispatched.results,
        dispatched.prevent_continuation,
        dispatched.injected_messages,
        dispatched.context_modifiers,
    ))
}

#[cfg(test)]
mod final_input_validation_tests {
    use super::*;
    use crate::test_support::{
        MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
        noop_hook_executor,
    };
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};
    use tool_api::tool_trait::{
        DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    };

    struct InputSpy(Arc<Mutex<Vec<Value>>>, Option<CooperativeCleanup>);

    struct CooperativeCleanup {
        started: Arc<tokio::sync::Notify>,
        finished: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait::async_trait]
    impl Tool for InputSpy {
        fn name(&self) -> &str {
            "input_spy"
        }
        fn input_schema(&self) -> &Value {
            static SCHEMA: std::sync::LazyLock<Value> = std::sync::LazyLock::new(|| {
                json!({
                    "type":"object",
                    "properties":{"action":{"type":"string"}},
                    "required":["action"],
                    "additionalProperties":false
                })
            });
            &SCHEMA
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
        async fn validate_input(
            &self,
            input: &Value,
            _: &ToolUseContext,
        ) -> Result<(), tool_api::ValidationError> {
            if input["action"] == "forbidden" {
                Err(tool_api::ValidationError("forbidden action".into()))
            } else {
                Ok(())
            }
        }
        async fn check_permissions(
            &self,
            _: &Value,
            _: &ToolUseContext,
        ) -> permission::PermissionResult {
            permission::PermissionResult::Allow {
                reason: permission::PermissionDecisionReason::Other {
                    reason: "test".into(),
                },
                updated_input: None,
                update_destination: None,
                metadata: Default::default(),
            }
        }
        async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
            "test".into()
        }
        async fn prompt(&self, _: &PromptOptions) -> String {
            "test".into()
        }
        async fn call(
            &self,
            input: Value,
            ctx: ToolUseContext,
            _: tool_api::progress::ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            self.0.lock().unwrap().push(input.clone());
            if let Some(cleanup) = &self.1 {
                cleanup.started.notify_one();
                ctx.cancel
                    .as_ref()
                    .expect("cancelable driver supplies its token")
                    .cancelled()
                    .await;
                // Require another poll after cancellation to complete release.
                tokio::task::yield_now().await;
                cleanup
                    .finished
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                return Err(ToolError::Aborted);
            }
            Ok(ToolCallResult::from_data(input))
        }
    }

    struct RewriteInput(Value);

    #[async_trait::async_trait]
    impl hooks::BuiltinHookHandler for RewriteInput {
        fn id(&self) -> &str {
            "rewrite-input"
        }
        async fn handle(&self, _: &HookEvent, _: &HookContext) -> hooks::HookResult {
            hooks::HookResult {
                outcome: hooks::HookOutcome::Success,
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
                response: Some(hooks::HookResponse {
                    updated_input: Some(self.0.clone()),
                    ..Default::default()
                }),
            }
        }
    }

    async fn dispatch_rewritten_input(updated: Value) -> (Vec<ContentBlock>, Vec<Value>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut registry = tool_api::registry::ToolRegistry::new();
        registry.register_builtin(Arc::new(InputSpy(seen.clone(), None)));
        let mut executor = Arc::try_unwrap(noop_hook_executor())
            .unwrap_or_else(|_| panic!("test hook executor is uniquely owned"));
        executor.register_builtin(Arc::new(RewriteInput(updated)));
        let executor = Arc::new(executor);
        let orch = ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(registry),
            executor.clone(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        );
        let session_id = orch.session.lock().await.session_id;
        executor
            .upsert_session_named_hook(
                session_id,
                "rewrite-input".into(),
                hooks::HookDefinition {
                    id: lingxi_core::types::HookId::new(),
                    name: "rewrite-input".into(),
                    events: vec![hooks::events::HookEventType::PreToolUse],
                    if_condition: None,
                    executor: hooks::HookExecutor::Builtin {
                        handler_id: "rewrite-input".into(),
                    },
                    source: hooks::HookSource::Session,
                    blocking: true,
                    timeout: None,
                    priority: 0,
                    once: false,
                    status_message: None,
                    async_rewake: false,
                    async_timeout: None,
                    rewake_message: None,
                },
            )
            .await;
        let results = dispatch_tool_uses(
            &orch,
            &[(
                ToolUseId::new(),
                "input_spy".into(),
                json!({"action":"original"}),
                None,
            )],
        )
        .await
        .unwrap();
        let inputs = seen.lock().unwrap().clone();
        (results, inputs)
    }

    #[tokio::test]
    async fn pretool_hook_cannot_introduce_schema_invalid_input() {
        let (results, inputs) =
            dispatch_rewritten_input(json!({"action":"valid", "unexpected":true})).await;
        assert!(
            inputs.is_empty(),
            "schema rejection must produce zero calls"
        );
        assert!(matches!(
            &results[0],
            ContentBlock::ToolResult {
                is_error: Some(true),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn pretool_hook_cannot_bypass_tool_semantic_validation() {
        let (results, inputs) = dispatch_rewritten_input(json!({"action":"forbidden"})).await;
        assert!(
            inputs.is_empty(),
            "semantic rejection must produce zero calls"
        );
        assert!(matches!(
            &results[0],
            ContentBlock::ToolResult {
                is_error: Some(true),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn pretool_hook_executes_final_valid_input() {
        let final_input = json!({"action":"updated"});
        let (results, inputs) = dispatch_rewritten_input(final_input.clone()).await;
        assert_eq!(inputs, [final_input]);
        assert!(matches!(
            &results[0],
            ContentBlock::ToolResult {
                is_error: Some(false),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn cancelable_batched_driver_waits_for_block_tool_cleanup_and_persists_result() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio_util::sync::CancellationToken;

        let started = Arc::new(tokio::sync::Notify::new());
        let finished = Arc::new(AtomicBool::new(false));
        let mut registry = tool_api::registry::ToolRegistry::new();
        registry.register_builtin(Arc::new(InputSpy(
            Arc::new(Mutex::new(Vec::new())),
            Some(CooperativeCleanup {
                started: started.clone(),
                finished: finished.clone(),
            }),
        )));
        let response = crate::test_support::mock_message_response(
            vec![llm_runtime::ContentBlock::ToolCall {
                id: "toolu_cooperative".into(),
                name: "input_spy".into(),
                input: json!({"action":"wait"}),
            }],
            Some("tool_use"),
        );
        let orch = ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![response])),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        );
        let cancel = CancellationToken::new();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let (result, ()) =
                tokio::join!(orch.run_turn_with_cancel("wait", cancel.clone()), async {
                    started.notified().await;
                    cancel.cancel();
                },);
            result
        })
        .await
        .expect("cooperative cancellation completes")
        .unwrap();
        assert!(matches!(
            result,
            crate::conversation::TurnOutcome::Cancelled
        ));
        assert!(
            finished.load(Ordering::SeqCst),
            "cleanup completed before return"
        );
        assert!(
            orch.session.lock().await.history.iter().any(|message| {
                matches!(message, ConversationMessage::User { content, .. }
                if content.iter().any(|block| matches!(block,
                    ContentBlock::ToolResult { is_error: Some(true), .. })))
            }),
            "cancelled tool result is retained in ordinary history"
        );
    }
}

#[cfg(test)]
mod native_tool_check_metadata_tests {
    use super::*;

    #[test]
    fn mod_tool_check_event_emits_trusted_agent_and_ask_ceiling_only() {
        let tool_use_id = ToolUseId::new();
        let event = mod_tool_check_event(
            "mcp__srv__tool",
            &serde_json::json!({"x":1}),
            &tool_use_id,
            Some("agent-trusted-7"),
            Some(lingxi_core::host::McpPermissionCeiling::Ask),
        );
        assert_eq!(
            event,
            serde_json::json!({
                "tool":"mcp__srv__tool",
                "input":{"x":1},
                "tool_use_id":tool_use_id.as_str(),
                "agentId":"agent-trusted-7",
                "ceiling":"ask"
            })
        );

        let ordinary = mod_tool_check_event(
            "Read",
            &serde_json::json!({}),
            &tool_use_id,
            None,
            Some(lingxi_core::host::McpPermissionCeiling::Allow),
        );
        assert_eq!(
            ordinary,
            serde_json::json!({
                "tool":"Read",
                "input":{},
                "tool_use_id":tool_use_id.as_str()
            }),
            "Native omits agentId when absent and only exposes the ask ceiling"
        );
    }

    #[tokio::test]
    async fn pretool_allow_mod_uses_the_captured_rule_after_live_overlay_changes() {
        use std::sync::Arc;

        use crate::test_support::{
            MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
            noop_hook_executor,
        };
        use lingxi_core::host::permission_gate::{
            HookAllowModCoreEvaluation, PermissionCheckContext, PermissionGate,
            PermissionResolution,
        };
        use permission::{
            PermissionBehavior, PermissionPolicy, PermissionRule, PermissionRuleSource,
            PermissionRuleValue, PolicyPermissionGate,
        };

        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("check.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('tool.check', { tool: 'Agent' }, async ($, event, next) => {
                const core = await next(event);
                return core.rule === 'Agent(general-purpose)'
                  ? { decision: 'allow' }
                  : { decision: 'deny', reason: 'fresh overlay leaked into core' };
              });
            }
            "#,
        )
        .unwrap();
        let host = hooks::mods::ModHost::start(None).await.unwrap();
        host.load(
            "captured-preflight",
            dir.path(),
            &module,
            serde_json::json!({}),
        )
        .await
        .unwrap();
        let mut hook_registry = hooks::HookRegistry::new();
        hook_registry.set_mod_host(host);

        let roots = permission::FsRoots {
            cwd: dir.path().to_path_buf(),
            home: Some(dir.path().to_path_buf()),
            lingxi_home: dir.path().join(branding::DOT_DIR),
        };
        let policy = Arc::new(
            PermissionPolicy::from_rules(
                permission::PermissionMode::Default,
                vec![PermissionRule {
                    value: PermissionRuleValue::from_rule_string("Agent(general-purpose)"),
                    behavior: PermissionBehavior::Ask,
                    source: PermissionRuleSource::Session,
                }],
            )
            .with_roots(roots),
        );
        let gate = Arc::new(PolicyPermissionGate::new(
            policy,
            Arc::new(NoOpPermissionGate),
        ));
        let orch = ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            gate.clone(),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            dir.path().to_path_buf(),
        )
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(hook_registry)));
        let input = serde_json::json!({"subagent_type":"general-purpose"});
        let ctx = PermissionCheckContext::default();
        let preflight: HookAllowModCoreEvaluation = gate
            .resolve_after_hook_allow_mod_core("Agent", &input, &ctx)
            .await
            .expect("PolicyPermissionGate returns the mode-less preflight evaluation");
        assert!(matches!(&preflight.resolution, PermissionResolution::Ask));
        assert_eq!(
            preflight
                .evaluation
                .as_ref()
                .unwrap()
                .verdict
                .rule
                .as_deref(),
            Some("Agent(general-purpose)")
        );

        gate.apply_permission_update(&serde_json::json!({
            "type":"replaceRules",
            "behavior":"ask",
            "destination":"session",
            "rules":[{"toolName":"Agent"}]
        }));
        let fresh = gate
            .check_mod_query("Agent", &input, &ctx)
            .await
            .expect("the real live overlay is queryable after replacement");
        assert_eq!(fresh.verdict.rule.as_deref(), Some("Agent"));

        let checked = apply_mod_tool_check(
            &orch,
            "Agent",
            &input,
            &ToolUseId::new(),
            preflight.resolution,
            None,
            preflight.evaluation.as_ref(),
            false,
            None,
            None,
            None,
        )
        .await;
        assert!(
            matches!(checked, PermissionResolution::Allow { .. }),
            "Mod must receive the captured rule from the preflight snapshot, not the replacement overlay"
        );
    }
}
