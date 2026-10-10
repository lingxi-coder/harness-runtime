//! `PermissionGate` trait — promoted from the M5-02 in-orchestrator local.
//!
//! This is the workspace-wide authoritative trait. `lingxi-permission` and
//! `lingxi-orchestrator` both re-export it. The orchestrator's old
//! `pub trait PermissionGate` in `test_support.rs` is now a `pub use`
//! re-export of this type — see M5-05 Task 2.
//!
//! **Plan deviation (M5-05):** The plan called for `check` to return
//! `Result<PermissionDecision, PermErr>`. The current orchestrator + M5-02
//! `NoOpPermissionGate` use the simpler `-> PermissionDecision` signature.
//! Keeping the simpler signature avoids touching every M5-02 / M5-04 call
//! site; the interactive gate (lingxi-permission) absorbs its own IO /
//! retry errors into [`PermissionDecision::Deny`].
#![forbid(unsafe_code)]

use crate::host::McpPermissionCeiling;
use crate::types::utf16_json::{Utf16JsonProjection, Utf16JsonProjectionError};
use crate::types::{ContentBlock, ConversationMessage};
use async_trait::async_trait;
use serde_json::Value;
use std::path::Path;

/// One tool call refused by the permission layer, as reported in the
/// `--output-format stream-json` `result` frame's `permission_denials`.
///
/// Oracle schema `LF` (`src_166572870.js`):
/// `{tool_name: string, tool_use_id: string, tool_input: Record<string, unknown>}`.
///
/// This is the AUTHORITATIVE denial record. claude-code's own schema doc for the
/// `permission_denied` system event says so explicitly, and lists what that
/// event does not cover: PreToolUse hook denies, deny-rule overrides of a hook
/// allow/ask decision, and Read/Edit/Write calls refused by a path-scoped deny
/// rule. Building this list from the event stream would inherit those holes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PermissionDenial {
    /// The tool that was refused.
    pub tool_name: String,
    /// The `tool_use_id` of the refused call.
    pub tool_use_id: String,
    /// The input the model supplied, verbatim.
    pub tool_input: Value,
    /// Exact UTF-16 source associated with `tool_input`. Never a wire field.
    #[serde(skip)]
    pub tool_input_projection: Option<Utf16JsonProjection>,
}

/// Receives only time spent waiting on a permission prompt transport.
#[derive(Clone)]
pub struct PermissionPauseObserver(std::sync::Arc<dyn Fn(u64) + Send + Sync>);

impl PermissionPauseObserver {
    pub fn new(callback: impl Fn(u64) + Send + Sync + 'static) -> Self {
        Self(std::sync::Arc::new(callback))
    }

    pub fn begin(&self) -> PermissionPauseGuard {
        PermissionPauseGuard {
            observer: self.clone(),
            started: std::time::Instant::now(),
        }
    }
}
impl std::fmt::Debug for PermissionPauseObserver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PermissionPauseObserver")
    }
}
impl PartialEq for PermissionPauseObserver {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for PermissionPauseObserver {}

/// Drop records cancellation as well as allowed/denied prompts (oracle Ge finally).
pub struct PermissionPauseGuard {
    observer: PermissionPauseObserver,
    started: std::time::Instant,
}
impl Drop for PermissionPauseGuard {
    fn drop(&mut self) {
        (self.observer.0)(self.started.elapsed().as_millis().min(u64::MAX as u128) as u64);
    }
}

/// Identity of the SUBAGENT / in-process-teammate worker a permission prompt is
/// being raised on behalf of, so the prompt UI can ATTRIBUTE it.
///
/// claude-code 2.1.186 surfaces a background worker's permission prompt in the
/// main session attributed to the asking agent (`${agent_id} needs permission
/// for ${tool_name}` + the `● @name` worker badge). Threaded from
/// [`crate::host::tool_invoker::SubagentInvocationContext`] through the dispatch
/// invoker into the prompt-building gate. `None` for a main-thread / leader
/// tool call (the turn loop's own [`PermissionGate::check`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptWorker {
    /// Worker DISPLAY name (claude-code `getAgentName()` — e.g. `"researcher"`).
    pub name: String,
    /// Team the worker belongs to (claude-code `getTeammateContext()?.teamName`).
    pub team: Option<String>,
    /// Whether the worker runs ASYNC (backgrounded).
    pub is_async: bool,
}

/// Outcome of a [`PermissionGate::check`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    /// The tool call is permitted.
    Allow,
    /// The tool call is rejected.
    Deny {
        /// Reason surfaced to the model as the `tool_result`.
        reason: String,
    },
}

/// Terminal permission outcome for a prompt-avoiding agent session.
///
/// Unlike [`PermissionDecision::Deny`], this stops the owning agent loop rather
/// than producing a recoverable tool-result denial. The policy gate emits it
/// only when the auto-mode classifier denial breaker trips in a non-interactive
/// context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionAbort {
    /// Byte-exact terminal message surfaced by the owning agent or turn loop.
    pub message: String,
}

/// Synchronous, prompt-free authorization used while expanding embedded shell
/// commands from slash-command/skill prompt text. `None` from the trait method
/// means the gate has no local rule layer and the caller should use its static
/// fallback policy; a rule-evaluating gate returns one of these decisions from
/// its live mode/rule snapshot without consulting an interactive prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NonInteractivePermissionDecision {
    /// The command is permitted.
    Allow,
    /// The command is rejected. The policy may intentionally omit explanatory
    /// text, matching `PermissionResult::Deny::explanation`.
    Deny {
        /// Optional user-facing denial reason.
        reason: Option<String>,
    },
}

/// Extra inputs for the tool-dispatch permission check beyond `name` + `input`,
/// so a stdio `can_use_tool` control_request can be byte-faithful and an allow
/// can carry the host's `updatedInput` rewrite.
///
/// Threaded from the orchestrator's tool dispatch into
/// [`PermissionGate::check_with_context`]. All fields are optional; the default
/// (`PermissionCheckContext::default()`) behaves exactly like
/// [`PermissionGate::check_with_worker`] with no worker.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PermissionCheckContext {
    /// Exact input for this physical permission check. Its display value must
    /// match the check's `input`; it must never be inherited from another call.
    /// Transports use this carrier when constructing `can_use_tool` frames.
    pub input_projection: Option<Utf16JsonProjection>,
    pub pause_observer: Option<PermissionPauseObserver>,
    /// Native MCP `mcpInfo.effectiveMaxPermission === "ask"` metadata for the
    /// current tool check. Only trusted host configuration may populate this;
    /// it is not inferred from tool identity or request arguments.
    pub tool_check_ceiling: Option<McpPermissionCeiling>,
    /// Subagent/teammate worker attribution (as in [`PermissionGate::check_with_worker`]).
    pub worker: Option<PromptWorker>,
    /// The assistant message's `tool_use` block id this check is for — the REAL
    /// id a stdio `can_use_tool` request should carry (claude-code
    /// `createCanUseTool(toolUseID)`), so the host can correlate + dedup. `None`
    /// ⇒ the gate mints a fresh id.
    pub tool_use_id: Option<String>,
    /// The policy Ask's human-readable decision reason, forwarded as the
    /// `decision_reason` field of a stdio `can_use_tool` request. `None` ⇒
    /// omitted (claude-code `serializeDecisionReason` returns `undefined` for
    /// rule/mode/subcommandResults/permissionPromptTool, so the common ask
    /// omits it). Populated by the `PolicyPermissionGate` Ask path from the
    /// policy result's [`PermissionDecisionReason`].
    pub decision_reason: Option<String>,
    /// The policy Ask's decision-reason TYPE discriminant, forwarded as the
    /// `decision_reason_type` field of a stdio `can_use_tool` request (claude-code
    /// `decision_reason_type: decisionReason?.type` — `rule`/`mode`/
    /// `subcommandResults`/`permissionPromptTool`/`hook`/`asyncAgent`/`workingDir`/
    /// `safetyCheck`/`sandboxOverride`/`classifier`/`other`). An SDK host reads
    /// this to classify Ask reasons where the free-text [`Self::decision_reason`]
    /// is omitted. `None` ⇒ the key is OMITTED. Populated by the
    /// `PolicyPermissionGate` Ask path alongside `decision_reason`.
    pub decision_reason_type: Option<String>,
    /// Whether every safety-check reason contributing to this Ask may be
    /// approved by the automated classifier.  `None` means the decision has no
    /// safety-check reason, so the stdio key is omitted.  For composite
    /// subcommand reasons the policy gate sets this to `false` if any nested
    /// safety check is not classifier-approvable.
    pub classifier_approvable: Option<bool>,
    /// Whether the tool itself requires interaction while it runs (for example
    /// `AskUserQuestion`).  The tool registry is the authoritative producer;
    /// the stdio transport emits the key only when this is `true`, matching the
    /// upstream `requiresUserInteraction?.() || undefined` shape.
    pub requires_user_interaction: bool,
    /// Whether this request originates from an agent context rather than the
    /// session's main thread.  Plan approval is a main-session operation, so
    /// policy-backed gates use this to withhold the session-wide Auto action
    /// from nested agent calls.  Defaults to `false` for existing callers.
    pub is_agent_context: bool,
    /// Whether this prompt must not offer or persist an "allow always" rule.
    ///
    /// This is set for tools whose own permission result requires a human at
    /// every invocation (for example an MCP tool with
    /// `requiresUserInteraction: true`).  It is deliberately separate from
    /// [`Self::requires_user_interaction`]: the latter is tool metadata sent to
    /// transports, while this flag is the end-to-end persistence guard.  The
    /// default is `false` so ordinary permission asks retain their existing
    /// AllowAlways behavior.
    pub suppress_always_allow_rule: bool,
    /// Engine-computed eligibility for offering the one-shot "switch to Auto"
    /// action. Transports must render this value but must not infer eligibility
    /// from tool names, modes, or request payloads. `None` is fail-closed.
    pub auto_mode_prompt: Option<AutoModePrompt>,
    /// Explicit engine provenance for this permission request. Unknown or
    /// absent provenance must not enable source-restricted prompt actions.
    pub request_source: Option<PermissionRequestSource>,
    /// The explicit Ask rule that matched this call, if the policy result
    /// retains one.  Kept transport-neutral here because this crate sits below
    /// the permission rule implementation.
    pub matched_ask_rule: Option<MatchedAskRule>,
    /// `true` when a PreToolUse hook returned `ask`, establishing claude-code's
    /// `hookAskFloor` (BIN 224697675/225722419). With the floor set, an Auto-mode
    /// classifier ALLOW must NOT silently defeat the hook's ask — the ask is kept
    /// (prompts interactively, denies headless), matching CC's floor
    /// (`if(hookAskFloor && !interactive && shouldAvoidPermissionPrompts) return
    /// asyncAgent-deny; else the classifier callback preserves behavior "ask"`).
    /// The 2.1.207 classifier-allow-over-hook-ask behavior 2.1.211/215 removed.
    /// Additive default `false`, so a non-hook-ask check is unaffected.
    pub hook_ask_floor: bool,
    /// The policy Ask's permission-rule SUGGESTIONS, forwarded as the
    /// `permission_suggestions` field of a stdio `can_use_tool` request
    /// (claude-code `mainPermissionResult.suggestions` — a `PermissionUpdate[]`).
    /// Carried as the RAW wire array so this crate need not name
    /// `lingxi-permission`'s `PermissionUpdate` (dep direction, see the note at
    /// the bottom of this file). `None` ⇒ the key is OMITTED from the request.
    ///
    /// Producers populate only suggestions they can derive without widening a
    /// rule (currently the exact matched-Ask-rule session update). Tool-specific
    /// suggestion builders may leave this absent.
    pub permission_suggestions: Option<Value>,
    /// The policy Ask's BLOCKED PATH, forwarded as the `blocked_path` field of a
    /// stdio `can_use_tool` request (claude-code `mainPermissionResult.blockedPath`
    /// — the filesystem path a path-scoped ask is gated on). `None` ⇒ the key is
    /// OMITTED.
    ///
    /// Path validators populate this only when they retain a structured concrete
    /// path; non-path and expansion-only asks leave it absent.
    pub blocked_path: Option<String>,
    /// A PER-CALL permission mode OVERRIDE as a WIRE string
    /// (`default`/`plan`/`acceptEdits`/`bypassPermissions`/`dontAsk`/`auto`).
    /// `Some` ⇒ the rule-evaluating gate authorizes THIS call under that mode
    /// instead of its live/boot mode — the seam a spawned subagent uses to run its
    /// tool dispatch under a clamped spawn mode (claude-code 2.1.207 Agent `mode` →
    /// the child's `toolPermissionContext.mode`, threaded from
    /// [`crate::host::tool_invoker::SubagentInvocationContext::mode_override`]). `None` ⇒
    /// the gate's live/boot mode applies (main loop / no override), byte-identical
    /// to before. Only the rule-evaluating `PolicyPermissionGate` consults it;
    /// other transports ignore it.
    pub mode_override: Option<String>,
    /// Whether the owning agent session cannot surface permission prompts.
    ///
    /// This is the per-dispatch equivalent of Claude Code's
    /// `shouldAvoidPermissionPrompts`. It is deliberately carried on the call
    /// context rather than stored process-globally so synchronous and async
    /// subagents can inherit the correct owner semantics without races.
    pub is_non_interactive_session: bool,
    /// This call's ORDERED permission-LAYER stack — 1:1 with claude-code
    /// `toolUseContext.permissionLayers`, folded onto the session's base
    /// permission context by `gn(toolUseContext)` (2.1.238 binary @287028951)
    /// on EVERY permission check.
    ///
    /// Carried as the RAW wire array so this crate need not name
    /// `lingxi-permission`'s `PermissionLayer` (dep direction — same precedent
    /// as [`Self::permission_suggestions`]). Each element is the upstream
    /// discriminated union, keyed by `kind`:
    ///
    /// | `kind` | payload | effect |
    /// |---|---|---|
    /// | `allowed_tools` | `allowedTools: string[]` | extra `command`-source ALLOW rules |
    /// | `disallowed_tools` | `disallowedTools: string[]` | extra `command`-source DENY rules |
    /// | `bash_command_clamp` | `rules: string[]` | one clamp GROUP; shell use is restricted to forms EVERY group admits |
    /// | `avoid_prompts` | — | `shouldAvoidPermissionPrompts = true` |
    /// | `permission_mode` | `mode: string` | per-call mode (bypass is refused when unavailable/killswitched) |
    /// | `working_directory` | `directory: string` | one extra working dir (LAST such layer wins) |
    /// | `effort` / `model` / `max_thinking_tokens` / `flag_settings` | — | inert for permissions |
    ///
    /// EMPTY for every dispatch that attaches no layers, in which case the fold
    /// is a no-op and the gate behaves exactly as it did before layers existed.
    /// Only the rule-evaluating `PolicyPermissionGate` consults it (it folds the
    /// stack in `check_with_context_or_abort` / `resolve_detailed_or_abort`);
    /// other transports ignore it.
    ///
    /// [`Self::mode_override`] and [`Self::is_non_interactive_session`] are the
    /// pre-existing FLAT spellings of the `permission_mode` and `avoid_prompts`
    /// layers; the gate hoists both to the head of this stack before folding, so
    /// a layer here can still override them.
    pub permission_layers: Vec<Value>,
    /// (Finding 22) `true` only when this dispatch was issued through a
    /// [`crate::host::tool_invoker::ToolInvoker`] the composition root built
    /// EXCLUSIVELY for a background task with no owning interactive turn — as
    /// of writing this is set only for the `/fusion` background task's
    /// invoker (`RegistryToolInvoker::with_background_owned`,
    /// `runtime/src/desktop/mod.rs`'s `fusion_invoker` wiring), never for
    /// the interactive turn's own direct tool call. It exists so a transport
    /// that queues/serializes interactive prompts (the TUI) can distinguish
    /// "this ask belongs to the turn a user is cancelling" from "this ask
    /// belongs to a background run that Ctrl-C does not touch" WITHOUT
    /// misusing [`Self::worker`] (worker attribution is a display concern —
    /// `can_show_permission_prompts` — and is `false` for the pool spawns a
    /// Fusion panel and every other one-shot subagent use, so it is always
    /// `None` on exactly the requests this field needs to mark). Default
    /// `false` preserves today's behavior for every other caller.
    pub background_owned: bool,
}

impl PermissionCheckContext {
    /// Return this check's exact input, rejecting stale or malformed sidecars.
    pub fn projected_input(
        &self,
        input: &Value,
    ) -> Result<Utf16JsonProjection, Utf16JsonProjectionError> {
        let Some(projection) = &self.input_projection else {
            return Ok(Utf16JsonProjection::plain(input.clone()));
        };
        if projection.value != *input {
            return Err(Utf16JsonProjectionError::InvalidProjection(
                "permission input projection belongs to different input",
            ));
        }
        projection.validate()?;
        Ok(projection.clone())
    }
}

/// Wire-neutral description of a matched permission Ask rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchedAskRule {
    /// Raw settings source (`userSettings`, `projectSettings`, …).
    pub source: String,
    /// Canonical tool name stored in the rule.
    pub tool_name: String,
    /// Optional rule-specific content (command/path/domain pattern).
    pub rule_content: Option<String>,
}

/// A transport-neutral description of the Auto action available on a prompt.
/// The engine is responsible for setting this only after all source, mode,
/// policy, and payload-renderability checks have passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoModePrompt {
    /// A workflow-agent Bash call may approve this call and enter Auto mode.
    WorkflowBash,
    /// ExitPlanMode may approve the plan and continue in Auto mode.
    ExitPlanMode,
}

/// Provenance used by engine-side prompt eligibility. This is intentionally
/// explicit rather than inferred from a tool name or payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionRequestSource {
    /// A Bash call made by a workflow's spawned agent.
    WorkflowAgent,
    /// A remote execution source; never eligible for this local Auto row.
    Remote,
    /// A plugin-provided source; never eligible for this local Auto row.
    Plugin,
    /// The engine could not establish a trusted source.
    Unknown,
}

impl AutoModePrompt {
    /// Exact row label used by the TUI and compatible clients.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::WorkflowBash => "Yes, and switch to auto mode",
            Self::ExitPlanMode => "Yes, and use auto mode",
        }
    }

    /// Stable wire token used by client transports.
    #[must_use]
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::WorkflowBash => "workflow_bash",
            Self::ExitPlanMode => "exit_plan_mode",
        }
    }

    /// Parse a wire token, failing closed for unknown values.
    #[must_use]
    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "workflow_bash" => Some(Self::WorkflowBash),
            "exit_plan_mode" => Some(Self::ExitPlanMode),
            _ => None,
        }
    }
}

#[cfg(test)]
mod auto_mode_prompt_tests {
    use super::AutoModePrompt;

    #[test]
    fn auto_prompt_tokens_and_labels_are_wire_locked() {
        assert_eq!(
            AutoModePrompt::WorkflowBash.label(),
            "Yes, and switch to auto mode"
        );
        assert_eq!(
            AutoModePrompt::ExitPlanMode.label(),
            "Yes, and use auto mode"
        );
        assert_eq!(
            AutoModePrompt::from_wire("workflow_bash"),
            Some(AutoModePrompt::WorkflowBash)
        );
        assert_eq!(AutoModePrompt::from_wire("unknown"), None);
    }
}

/// Host-provided classification for a resolved interactive tool decision.
///
/// This is telemetry metadata only: it never changes whether the tool runs.
/// Unknown wire values are ignored by transports and therefore fall back to
/// the existing temporary/reject classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolDecisionClassification {
    /// Allowed for this invocation/session only.
    UserTemporary,
    /// Allowed and persisted by the host.
    UserPermanent,
    /// Explicitly rejected by the user.
    UserReject,
}

impl ToolDecisionClassification {
    /// Stable OTEL label used by Claude Code's `tool_decision` event.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserTemporary => "user_temporary",
            Self::UserPermanent => "user_permanent",
            Self::UserReject => "user_reject",
        }
    }
}

/// Richer outcome of [`PermissionGate::check_with_context`]: an allow may carry
/// the host/policy-rewritten tool input (`updatedInput`) the dispatcher should
/// run the tool with instead of the original.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionOutcome {
    /// Permitted. `updated_input` is the rewritten input to use instead of the
    /// original (claude-code `updatedInput` when it has keys), or `None` to keep
    /// the original.
    Allow {
        /// Host/policy-rewritten tool input, or `None` to keep the original.
        updated_input: Option<Utf16JsonProjection>,
        /// The host's `updatedPermissions` payload from a `can_use_tool` ALLOW
        /// response — the RAW wire array of permission-rule updates the host wants
        /// applied + persisted (claude-code `applyPermissionUpdates` +
        /// `persistPermissionUpdates`, fired in
        /// `permissionPromptToolResultToPermissionDecision`). Each element is a
        /// `permissionUpdateSchema` discriminated union (`addRules`,
        /// `replaceRules`, `removeRules`, `setMode`, and directory updates are
        /// supported). EMPTY for every gate but
        /// `StdioControlPermissionGate`. The value is a raw `serde_json::Value`
        /// (not the typed `permission::PermissionUpdate`) because this crate sits
        /// BELOW `lingxi-permission` in the dependency graph and cannot name that
        /// type — the permission-aware consumer parses + applies it.
        permission_updates: Vec<Value>,
        /// Optional host classification for telemetry attribution.
        decision_classification: Option<ToolDecisionClassification>,
    },
    /// Permitted for this invocation and requests an atomic session transition
    /// to Auto mode. This never carries/persists an AllowAlways rule.
    AllowAuto {
        /// Host/policy-rewritten tool input, or `None` to keep the original.
        updated_input: Option<Utf16JsonProjection>,
    },
    /// Rejected, with the reason surfaced to the model as the `tool_result`.
    Deny {
        /// Reason surfaced to the model.
        reason: String,
    },
}

/// Trusted tool metadata selecting classifier-only permission composition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassifierOnlyPolicy {
    /// How a reviewed report is handled when the classifier blocks it.
    pub on_block: ClassifierOnlyOnBlock,
}

/// A classifier-only tool cannot obtain a human prompt fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassifierOnlyOnBlock {
    /// Reject a blocked action.
    Deny,
    /// Admit a report with its invocation-local warning or availability note.
    Flag,
}

/// Immutable classifier input captured from the dispatching child.
///
/// These values come from the runner and trusted tool implementation. The
/// report's model-supplied JSON cannot select a transcript or invent approval.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassifierOnlyReviewRequest {
    /// The child's actual conversation, never the main session's history.
    pub transcript: Vec<ConversationMessage>,
    /// The tool's native classifier action serialization.
    pub action: String,
}

/// Permission and report review bound to this single invocation.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassifierOnlyOutcome {
    /// Whether this invocation may execute.
    pub permission: PermissionOutcome,
    /// The actual classifier result; absent when review did not run.
    pub review: Option<super::handback::ReportReview>,
}

/// What produced a [`PermissionGate`] decision — lets the turn loop fire the
/// source-gated permission hooks the way claude-code does: `PermissionRequest`
/// on an about-to-ask, `PermissionDenied` ONLY on an auto-mode classifier deny
/// (`toolExecution.ts:1075`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecisionSource {
    /// An explicit allow/deny/ask RULE matched (claude-code decisionReason 'rule').
    Rule,
    /// The active permission MODE produced the decision (no rule matched).
    Mode,
    /// An auto-mode CLASSIFIER produced the decision (claude-code 'classifier').
    /// `PermissionDenied` hooks fire only on a deny from this source.
    Classifier,
    /// No richer source is available — a gate with no rule/mode layer, or a
    /// decision class that carries no distinguishable source.
    Unspecified,
}

/// A [`PermissionGate`] resolution that carries its [`PermissionDecisionSource`]
/// and distinguishes an about-to-ASK (which the turn loop surfaces to a
/// `PermissionRequest` hook BEFORE delegating to the prompt transport) from a
/// resolved allow/deny. Returned by [`PermissionGate::resolve_detailed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionResolution {
    /// Permitted outright (rule/mode allow, or a read-only auto-allow).
    Allow {
        /// Raw `SettingSource` token of the RULE that produced this allow
        /// (`userSettings`, `localSettings`, `session`, …), or `None` when no
        /// rule matched (a mode allow, a read-only auto-allow, or a gate with no
        /// rule layer). Feeds the OTEL decision-source label — claude-code
        /// `ZX_(decisionReason.rule.source, behavior)`.
        rule_source: Option<String>,
        /// An LLM classifier has already evaluated this action and approved it.
        /// Kept separate from rule provenance so tool-local ordinary asks are
        /// not mistaken for new evidence after classification.
        classifier_approved: bool,
    },
    /// Rejected, with the rendered deny reason and its source.
    Deny {
        /// Reason surfaced to the model as the `tool_result`.
        reason: String,
        /// Where the denial came from (gates the `PermissionDenied` hook).
        source: PermissionDecisionSource,
        /// Raw `SettingSource` token of the matched deny RULE, mirroring
        /// [`Self::Allow::rule_source`]. `None` for every non-rule denial.
        rule_source: Option<String>,
        /// GATE-SYSMSG-01: the discriminated reason kind
        /// (`decisionReason?.type` — `rule`/`mode`/`safetyCheck`/…), pre-computed
        /// where the full [`crate`]-external `PermissionDecisionReason` is in scope
        /// (that type lives in the `permission` crate, which depends on this one,
        /// so it cannot be a field here). Feeds the `permission_denied` system
        /// message the turn loop emits on the main-conversation deny path. `None`
        /// for producers without a structured reason.
        decision_reason_type: Option<String>,
        /// GATE-SYSMSG-01: the `oin(decisionReason)`-filtered reason TEXT (only
        /// the free-text reason kinds surface it). Pre-computed alongside
        /// [`Self::Deny::decision_reason_type`]; `None` otherwise.
        decision_reason: Option<String>,
        /// `true` when the underlying `permissionDecision.behavior === 'ask'`
        /// (a rejection that came from an ASK prompt the user declined), vs a
        /// rule/mode `deny`. claude-code only appends the rejection's
        /// `contentBlocks` to the outgoing user message when the behavior is
        /// `ask` (`toolExecution.ts:1040-1043`), so the turn loop branches on
        /// this. `false` for every existing producer (rule/mode/classifier
        /// denies and the default `resolve_detailed`), keeping the common deny
        /// path byte-identical.
        behavior_ask: bool,
        /// Image (or other non-text) content blocks the `ask`-behavior
        /// rejection supplied — `permissionDecision.contentBlocks`
        /// (`toolExecution.ts:1040`). claude-code appends these at the TOP LEVEL
        /// of the deny user message (alongside, NOT inside, the text-only
        /// `tool_result`, which rejects non-text when `is_error` is set). EMPTY
        /// for every existing producer; only consulted when
        /// [`Self::Deny::behavior_ask`] is also `true`. Dormant in the public
        /// build (no gate supplies them), faithful to claude-code where the
        /// `ask`+contentBlocks rejection path is itself gated.
        content_blocks: Vec<ContentBlock>,
    },
    /// The gate would PROMPT for this call — delegate to its inner transport in
    /// interactive mode, or auto-deny it in headless. The turn loop fires the
    /// `PermissionRequest` hook here before resolving via the transport.
    Ask,
    /// The gate would prompt, carrying the classifier breaker context that
    /// must be forwarded to the prompt transport without a global side-channel.
    /// This is emitted only for the interactive auto-mode denial-limit fallback.
    AskWithContext {
        /// Discriminant sent as `decision_reason_type`.
        decision_reason_type: Option<String>,
        /// Free-text reason sent as `decision_reason`.
        decision_reason: Option<String>,
    },
}

/// Declarative permission answer for a Mod `$.tool.check` query. A query does
/// not execute a tool, run PreToolUse, consult an LLM classifier, or prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModToolCheckVerdict {
    pub decision: ModToolCheckDecision,
    pub reason: Option<String>,
    pub rule: Option<String>,
}

/// Live Read-policy state needed by Native's changed-file reminder gate.
/// These are policy facts, not a prompt result or a generic tool-check verdict.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReadPathPolicyFacts {
    /// `nor(context)`: a Read deny can change whether a path-set pass is needed.
    pub read_deny_rules_active: bool,
    /// Native `ToolPermissionContext.restricted`.
    pub restricted: bool,
    /// Native `blockReadsOutsideWorkingDirectories`.
    pub block_reads_outside_working_directories: bool,
}

impl ReadPathPolicyFacts {
    /// Native `sy(context)` predicate controlling changed-file path expansion.
    #[must_use]
    pub const fn requires_path_recheck(self) -> bool {
        self.read_deny_rules_active
            || self.restricted
            || self.block_reads_outside_working_directories
    }
}

/// One Native Read-path policy predicate for a spelling already present in the
/// changed-file path Set. `Unavailable` is distinct from `NoMatch`: a real
/// policy can be active while lacking the roots needed to evaluate a path
/// rule or working-directory hold.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ReadPathPolicyMatch {
    /// The current policy snapshot evaluated the predicate and found no match.
    #[default]
    NoMatch,
    /// The current policy snapshot evaluated the predicate and found a match.
    Match,
    /// A required policy input is absent for this real policy snapshot.
    Unavailable,
}

/// Results of Native's independent Read-deny and held-outside checks for one
/// spelling already present in its changed-file path Set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReadPathPolicyCheck {
    pub denied_by_read_rule: ReadPathPolicyMatch,
    pub held_outside: ReadPathPolicyMatch,
}

/// Immutable view of the live policy used by one changed-file reminder pass.
/// A `PolicyPermissionGate` returns a snapshot of its current overlay; a
/// transport-only gate uses [`NoPolicyReadPathSnapshot`] explicitly.
pub trait ReadPathPolicySnapshot: Send + Sync {
    fn facts(&self) -> ReadPathPolicyFacts;
    fn check_path(&self, path: &Path) -> ReadPathPolicyCheck;
}

/// Explicit inactive facts for a permission transport with no policy layer.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoPolicyReadPathSnapshot;

impl ReadPathPolicySnapshot for NoPolicyReadPathSnapshot {
    fn facts(&self) -> ReadPathPolicyFacts {
        ReadPathPolicyFacts::default()
    }

    fn check_path(&self, _path: &Path) -> ReadPathPolicyCheck {
        ReadPathPolicyCheck::default()
    }
}

/// Complete policy result captured at the Mod `tool.check` boundary.
///
/// `core` is below `permission` in the crate graph, so this transport-neutral
/// representation is the typed seam for carrying the exact policy result
/// across that boundary. It intentionally preserves the nested reason tree and
/// optional fields instead of collapsing the result to the Mod DTO. The
/// permission crate converts it back to its native `PermissionResult` before
/// applying execution-phase classification after the Mod hook returns.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionToolCheckEvaluation {
    pub result: PermissionToolCheckResult,
    pub effective_mode: PermissionToolCheckMode,
    pub verdict: ModToolCheckVerdict,
    /// Trusted host ceiling projected by Native `tool.check`; currently only
    /// the explicit organization `ask` ceiling is exposed on that surface.
    pub ceiling: Option<McpPermissionCeiling>,
}

/// The captured result of the mode-less permission recheck for a PreToolUse
/// approval, paired with its execution-facing resolution. `evaluation` is
/// present only when a rule or safety result actually objected; a clean hook
/// allow carries `None`, matching Native's hook result passed as the Mod core.
#[derive(Debug, Clone, PartialEq)]
pub struct HookAllowModCoreEvaluation {
    pub resolution: PermissionResolution,
    pub evaluation: Option<PermissionToolCheckEvaluation>,
}

/// Permission modes needed to finish a captured ToolCheck result after Mod.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionToolCheckMode {
    Default,
    Plan,
    AcceptEdits,
    BypassPermissions,
    DontAsk,
    Bubble,
    Auto,
}

/// Typed, lossless bridge representation of permission::PermissionResult.
#[derive(Debug, Clone, PartialEq)]
pub enum PermissionToolCheckResult {
    Allow {
        reason: PermissionToolCheckReason,
        updated_input: Option<Value>,
        update_destination: Option<PermissionToolCheckUpdateDestination>,
        metadata: PermissionToolCheckMetadata,
    },
    Deny {
        reason: PermissionToolCheckReason,
        explanation: Option<String>,
        metadata: PermissionToolCheckMetadata,
    },
    Ask {
        reason: PermissionToolCheckReason,
        prompt: PermissionToolCheckPrompt,
        pending_classifier_check: Option<PermissionToolCheckPendingClassifier>,
        metadata: PermissionToolCheckMetadata,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum PermissionToolCheckReason {
    MatchedRule {
        rule: PermissionToolCheckRule,
    },
    PermissionMode {
        mode: PermissionToolCheckMode,
    },
    SubcommandResults {
        /// Ordered in the same order as the native insertion-ordered Map.
        reasons: Vec<(String, Box<PermissionToolCheckResult>)>,
    },
    PermissionPromptTool {
        tool_name: String,
    },
    ClassifierApproved {
        classifier: PermissionToolCheckClassifier,
        score: f64,
    },
    ClassifierRejected {
        classifier: PermissionToolCheckClassifier,
        score: f64,
        reason: String,
    },
    HookOverride {
        hook_id: String,
        source: Option<String>,
        reason: Option<String>,
    },
    AsyncAgent {
        reason: String,
    },
    SandboxOverride {
        reason: PermissionToolCheckSandboxOverride,
    },
    WorkingDirectory {
        reason: String,
    },
    SafetyCheck {
        reason: String,
        classifier_approvable: bool,
        circuit_breaker: Option<PermissionToolCheckCircuitBreaker>,
    },
    Other {
        reason: String,
    },
    DenialLimitExceeded,
    AutoModeFallback,
    BypassPermissions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionToolCheckRule {
    pub tool_name: String,
    pub rule_content: Option<String>,
    pub behavior: PermissionToolCheckBehavior,
    pub source: PermissionToolCheckRuleSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionToolCheckBehavior {
    Allow,
    Deny,
    Ask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionToolCheckRuleSource {
    UserSettings,
    ProjectSettings,
    LocalSettings,
    ManagedPolicy,
    FlagSettings,
    CliArg,
    Command,
    Session,
    ToolsNarrowing,
    McpServerPolicy,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PermissionToolCheckMetadata {
    pub matched_rules: Vec<String>,
    pub permission_suggestions: Option<Value>,
    pub blocked_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionToolCheckPrompt {
    pub title: String,
    pub message: String,
    pub options: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionToolCheckPendingClassifier {
    pub classifier: PermissionToolCheckClassifier,
    pub request_id: crate::types::RequestId,
    pub started_at: std::time::SystemTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionToolCheckClassifier {
    Yolo,
    Bash,
    Transcript,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionToolCheckSandboxOverride {
    ExcludedCommand,
    DangerouslyDisableSandbox,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionToolCheckCircuitBreaker {
    BackgroundOperator,
    DangerousRemoval,
    IsolatePeerMachines,
    OutsideReadsBlocked,
    RestrictedMode,
    SuspiciousWindowsPath,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionToolCheckUpdateDestination {
    UserSettings,
    ProjectSettings,
    LocalSettings,
    Session,
    CliArg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModToolCheckDecision {
    Allow,
    Ask,
    Deny,
}

/// The workspace-wide authorization gate consulted before every tool dispatch.
///
/// M5-02 introduced this trait as a `pub` item inside
/// `lingxi-orchestrator::test_support`. M5-05 promotes it to the traits
/// crate so that `lingxi-permission` can carry the real
/// `InteractivePromptingGate` impl without a circular dep.
/// What a subagent handoff review concluded — `EZe`'s `{warning, kind}` minus
/// the rendered copy, which belongs to the agent tool that prepends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandoffReview {
    /// `kind: "flagged"` — the classifier judged the work and blocked it. The
    /// work is still delivered, carrying the classifier's own reason.
    Flagged {
        /// The classifier's human-readable reason, interpolated into the copy.
        reason: String,
    },
    /// `kind: "refused"` — an upstream safety filter refused to perform the
    /// review. Not a verdict on the work; it reacts to the subagent's own
    /// transcript, which the subagent controls.
    Refused,
    /// `kind: "unavailable"` — the review could not run at all.
    Unavailable {
        /// The classifier model, named in the warning; empty when unknown.
        model: String,
        /// `q$t`'s parenthetical (`" (timed out)"`, …) or empty.
        detail: String,
    },
}

#[async_trait]
pub trait PermissionGate: Send + Sync {
    /// Authorize a tool call by `name` with `input`.
    ///
    /// Implementations may consult policy rules, prompt the user, or run
    /// permission classifiers. All error paths are folded into
    /// [`PermissionDecision::Deny`] — the caller never has to handle an
    /// error tier.
    async fn check(&self, name: &str, input: &Value) -> PermissionDecision;

    /// Review a classifier-only tool through a dedicated, prompt-free path.
    ///
    /// Dispatch must select this before saved rules, hook allows and safe-tool
    /// shortcuts can authorize the action. The default fails closed because a
    /// prompt-only or no-op gate has no classifier verdict to provide.
    async fn check_classifier_only_with_context_or_abort(
        &self,
        name: &str,
        input: &Value,
        ctx: &PermissionCheckContext,
        policy: ClassifierOnlyPolicy,
        request: &ClassifierOnlyReviewRequest,
    ) -> Result<ClassifierOnlyOutcome, PermissionAbort> {
        let _ = (input, ctx, policy, request);
        Ok(ClassifierOnlyOutcome {
            permission: PermissionOutcome::Deny {
                reason: format!(
                    "Only the auto-mode classifier can allow {name}: no classifier review is available"
                ),
            },
            review: None,
        })
    }

    /// Inspect the live declarative policy without a prompt or classifier.
    /// `None` means this gate has no policy inspection surface; callers must
    /// not substitute [`Self::check`] because it may prompt or classify.
    async fn check_mod_query(
        &self,
        _name: &str,
        _input: &Value,
        _ctx: &PermissionCheckContext,
    ) -> Option<PermissionToolCheckEvaluation> {
        None
    }

    /// Apply the execution-stage policy to the exact typed result captured by
    /// [`Self::check_mod_query`], after the Mod hook has returned. A policy gate
    /// must not re-authorize here: this stage adds execution-only behavior such
    /// as the `dontAsk` clamp and Auto classifier, while retaining the same
    /// result/rule snapshot that produced the Mod DTO.
    async fn resolve_tool_check_execution(
        &self,
        name: &str,
        _input: &Value,
        _ctx: &PermissionCheckContext,
        evaluation: &PermissionToolCheckEvaluation,
    ) -> Result<PermissionResolution, PermissionAbort> {
        Ok(match evaluation.verdict.decision {
            ModToolCheckDecision::Allow => PermissionResolution::Allow {
                rule_source: None,
                classifier_approved: false,
            },
            ModToolCheckDecision::Ask => PermissionResolution::AskWithContext {
                decision_reason_type: None,
                decision_reason: evaluation.verdict.reason.clone(),
            },
            ModToolCheckDecision::Deny => PermissionResolution::Deny {
                reason: evaluation
                    .verdict
                    .reason
                    .clone()
                    .unwrap_or_else(|| format!("Permission to use {name} has been denied.")),
                source: PermissionDecisionSource::Rule,
                rule_source: None,
                decision_reason_type: Some("rule".into()),
                decision_reason: None,
                behavior_ask: false,
                content_blocks: Vec::new(),
            },
        })
    }

    /// Surface a captured policy Ask through the prompt transport after its
    /// execution-stage decision has remained Ask. Implementations that own the
    /// captured result use it directly so decision-reason, suggestions, blocked
    /// path, and updated-input presence are not lost to a second authorization.
    async fn ask_tool_check_via_transport(
        &self,
        name: &str,
        _input: &Value,
        _ctx: &PermissionCheckContext,
        _evaluation: &PermissionToolCheckEvaluation,
        _resolution: &PermissionResolution,
    ) -> PermissionOutcome {
        PermissionOutcome::Deny {
            reason: format!(
                "Permission to use {name} cannot be prompted from the captured ToolCheck result"
            ),
        }
    }
    /// Resolve a tool call synchronously and without prompting, after injecting
    /// transient allow rules such as a prompt command's frontmatter
    /// `allowed-tools`. This is the live-policy seam used by embedded `!cmd``
    /// expansion: an unresolved `Ask` is returned as `Deny`, exactly as Claude
    /// Code's prompt-shell path rejects every non-allow decision.
    ///
    /// The default returns `None`, preserving gates that are only prompt
    /// transports or no-ops; callers then evaluate their construction-time
    /// fallback policy. `PolicyPermissionGate` overrides this and reads its live
    /// mode plus `updatedPermissions` overlay on every call.
    fn check_noninteractive_with_allow_rules(
        &self,
        name: &str,
        input: &Value,
        transient_allow_rules: &[String],
    ) -> Option<NonInteractivePermissionDecision> {
        let _ = (name, input, transient_allow_rules);
        None
    }

    /// Replace — or UNION into — the deny rules contributed by the slash
    /// command / skill that is running, claude-code `Tbt`
    /// (`setToolPermissionContext`, writing `alwaysDenyRules.command`).
    ///
    /// Upstream calls this from exactly two places, and the two modes are not
    /// interchangeable:
    ///
    /// - **replace** (`union == false`), once per processed user input, with
    ///   that input's `disallowedTools` (empty for a plain prompt). This is what
    ///   gives a skill's `disallowed-tools` a LIFETIME: it lasts until the next
    ///   user message and is then cleared, rather than leaking into the rest of
    ///   the session.
    /// - **union** (`union == true`), when the Skill TOOL activates a skill
    ///   mid-turn, so the skill's denies are added to whatever the current input
    ///   already established.
    ///
    /// ⚠️ Union mode DROPS any spec matching `/^(?:Read|Edit)\((?:\.\/)?!/`
    /// (upstream `MWn`). A `!`-prefixed path rule is a gitignore NEGATION that
    /// CANCELS deny rules in its own source — see
    /// `permission::filesystem::RulePatternMatch` — so letting a skill union one
    /// in would turn a "deny more" declaration into a way to un-deny. Replace
    /// mode has no such filter upstream and none here: it overwrites the whole
    /// command source rather than adding to it.
    ///
    /// Default: a no-op, for gates that are only prompt transports.
    /// `PolicyPermissionGate` overrides it.
    fn set_command_input_denies(&self, specs: &[String], union: bool) {
        let _ = (specs, union);
    }

    /// Return the current `Read`-deny patterns rebased to `cwd` for search
    /// result filtering. `None` means this gate has no live policy layer and
    /// the caller should use its construction-time fallback.
    ///
    /// `PolicyPermissionGate` overrides this so `updatedPermissions` changes
    /// take effect in `Glob`/`Grep` on the next call, matching Claude Code's
    /// per-call read of `toolPermissionContext`.
    fn read_deny_exclude_globs(&self, cwd: &Path) -> Option<Vec<String>> {
        let _ = cwd;
        None
    }

    /// Current policy view used by Native's changed-file Read recheck. The
    /// default is for transport-only gates with no policy layer; a gate that
    /// owns rules or filesystem restrictions must override it. This is an
    /// explicit inactive view, not an inferred permission decision.
    fn read_path_policy_snapshot(&self) -> std::sync::Arc<dyn ReadPathPolicySnapshot> {
        std::sync::Arc::new(NoPolicyReadPathSnapshot)
    }

    /// Like [`Self::check`], but carrying the identity of the SUBAGENT/teammate
    /// worker the call originates from so a prompt-building gate can ATTRIBUTE
    /// the prompt to that worker — claude-code 2.1.186 surfaces a background
    /// worker's permission prompt in the main session with a `● @name` badge
    /// (`${agent_id} needs permission for ${tool_name}`). The subagent dispatch
    /// invoker ([`crate::host::tool_invoker::ToolInvoker`]) calls this; the main turn
    /// loop uses [`Self::check`] (no worker).
    ///
    /// Additive DEFAULTED (frozen-trait safe): the default IGNORES `worker` and
    /// delegates to [`Self::check`], so every existing impl is unchanged. Only
    /// the prompt-building gates (`TuiPermissionGate` / `AdapterPermissionGate`)
    /// and the wrapping `PolicyPermissionGate` (which forwards it to its inner
    /// transport) OVERRIDE it.
    async fn check_with_worker(
        &self,
        name: &str,
        input: &Value,
        worker: Option<PromptWorker>,
    ) -> PermissionDecision {
        let _ = worker;
        self.check(name, input).await
    }

    /// Resolve an ASK the CALLER already decided on — the tool's own
    /// `Tool::check_permissions` — WITHOUT re-deriving a verdict from the rule /
    /// mode layer.
    ///
    /// claude-code composes the two the other way round: `a6e`
    /// (`permissions.ts`, 2.1.238 BIN off **290296046**) calls
    /// `l = await e.checkPermissions(input, ctx)` INSIDE the policy evaluation
    /// and then lets `l.behavior === "ask"` survive the allow-rule /
    /// bypassPermissions arms when its `decisionReason` is one of the protected
    /// kinds (`sandboxOverride` among them:
    /// `if(l?.behavior==="ask"&&(f||!p&&(J$(l.decisionReason)||
    ///  l.decisionReason?.type==="sandboxOverride"||Cxf(l.decisionReason))))return l`).
    ///
    /// LingXi's policy layer is a separate crate that cannot call back into a
    /// `Tool`, so the turn loop performs that composition instead: it resolves
    /// the policy verdict first and, when the tool's own hook escalates an
    /// otherwise-permitted call to `ask`, hands the escalation here so it
    /// reaches the PROMPT rather than being re-authorized (and re-allowed) by
    /// [`Self::check_with_context`].
    ///
    /// DEFAULT: delegate to [`Self::check_with_context`]. For a leaf transport
    /// (`TuiPermissionGate`, the stdio gate, `DenyOnAskGate`) that IS the prompt,
    /// so the default is already correct; only a POLICY-evaluating wrapper needs
    /// to override it, and `PolicyPermissionGate` does. Frozen-trait safe.
    async fn ask_via_transport(
        &self,
        name: &str,
        input: &Value,
        ctx: &PermissionCheckContext,
    ) -> PermissionOutcome {
        self.check_with_context(name, input, ctx).await
    }

    /// Like [`Self::check_with_worker`], but carrying a [`PermissionCheckContext`]
    /// (real `tool_use_id`, decision reason) and returning a [`PermissionOutcome`]
    /// that may carry the host's `updatedInput` rewrite.
    ///
    /// The tool-dispatch path calls this so the stdio `can_use_tool` gate can
    /// emit a byte-faithful request (real tool_use_id) and apply the host's
    /// rewritten input. The default IGNORES the extra context and delegates to
    /// [`Self::check_with_worker`], mapping `Allow`→`Allow{updated_input:None}`
    /// — so every existing impl is unchanged (frozen-trait safe). Only
    /// `StdioControlPermissionGate` (the stdio transport) and the wrapping
    /// `PolicyPermissionGate` OVERRIDE it.
    async fn check_with_context(
        &self,
        name: &str,
        input: &Value,
        ctx: &PermissionCheckContext,
    ) -> PermissionOutcome {
        match self
            .check_with_worker(name, input, ctx.worker.clone())
            .await
        {
            PermissionDecision::Allow => PermissionOutcome::Allow {
                updated_input: None,
                permission_updates: Vec::new(),
                decision_classification: None,
            },
            PermissionDecision::Deny { reason } => PermissionOutcome::Deny { reason },
        }
    }

    /// Request approval for an `ExitPlanMode` call while preserving the plan
    /// payload for transports that render a plan-specific dialog.  This is a
    /// dedicated seam because a generic permission check would lose the plan
    /// body and incorrectly construct a `ToolUseConfirm` request.  The default
    /// keeps prompt-only gates source-compatible; a policy-backed gate
    /// overrides it to compute the engine-owned Auto eligibility and consume a
    /// selected `AllowAuto` response.
    async fn check_exit_plan_mode(
        &self,
        plan: &str,
        ctx: &PermissionCheckContext,
    ) -> PermissionOutcome {
        self.check_with_context("ExitPlanMode", &serde_json::json!({ "plan": plan }), ctx)
            .await
    }

    /// Review a finished subagent's work before its parent reads it — the port
    /// of `EZe`, the auto-mode classifier's second consumer.
    ///
    /// Returns the outcome to render as a prepended warning block, or `None`
    /// when there is nothing to say: not auto mode, no classifier bound, or the
    /// review came back clean. `transcript` is the child's persisted transcript
    /// path; `final_text` is the hand-back the parent would otherwise act on
    /// unreviewed.
    ///
    /// The default reviews nothing, so a gate without a classifier keeps
    /// today's behaviour instead of inventing a verdict.
    async fn review_subagent_handoff(
        &self,
        transcript: Option<&Path>,
        final_text: &str,
    ) -> Option<HandoffReview> {
        let _ = (transcript, final_text);
        None
    }

    /// Like [`Self::check_with_context`], but preserves a terminal
    /// [`PermissionAbort`] instead of folding every failure into a recoverable
    /// [`PermissionOutcome::Deny`].
    ///
    /// The default keeps existing gates source-compatible and never aborts.
    /// Rule-evaluating gates override this only for prompt-avoiding auto-mode
    /// denial-breaker trips.
    async fn check_with_context_or_abort(
        &self,
        name: &str,
        input: &Value,
        ctx: &PermissionCheckContext,
    ) -> Result<PermissionOutcome, PermissionAbort> {
        Ok(self.check_with_context(name, input, ctx).await)
    }

    /// Apply permission-context updates to the live in-memory gate state.
    ///
    /// The default is a no-op for prompt-only transports. A rule-evaluating
    /// gate overrides this so normal and orphaned permission responses update
    /// the same live context.
    fn apply_permission_updates(&self, _updates: &[Value]) {}

    /// Persist permission updates through the transport that owns the settings
    /// roots. This is separate from live application because an outer policy
    /// gate owns the former while its inner stdio transport owns the latter.
    async fn persist_permission_updates(&self, _updates: &[Value]) {}

    /// Enable or disable permission persistence for this prompt transport.
    /// Managed policy calls this with `false` when only centrally managed rules
    /// are permitted. The default is a no-op for transports without persistence.
    fn set_permission_persistence_enabled(&self, _enabled: bool) {}

    /// Whether this call is a restricted-mode protected mutation. Policy-backed
    /// gates override this using their existing path/mutation provenance so the
    /// turn loop can suppress persistent "allow always" approvals. The default
    /// is false for prompt-only transports and preserves the frozen trait ABI.
    fn is_restricted_protected_mutation(&self, name: &str, input: &Value) -> bool {
        let _ = (name, input);
        false
    }

    /// GATE-SYSMSG-01: notify the transport that a tool call was DENIED by the
    /// local policy pre-check, so a stdio/SDK transport can emit a
    /// `permission_denied` system message on its output stream — 1:1 with
    /// claude-code `createCanUseTool`'s deny arm (`{type:"system",
    /// subtype:"permission_denied", tool_name, tool_use_id, agent_id,
    /// decision_reason_type, decision_reason, message, uuid, session_id}`).
    ///
    /// `decision_reason_type` is the discriminated reason kind
    /// (`rule`/`mode`/`safetyCheck`/…, the `decisionReason?.type`), and
    /// `decision_reason` is the `oin(decisionReason)`-filtered reason text (only
    /// the classifier/hook/asyncAgent/sandboxOverride/workingDir/safetyCheck/other
    /// kinds surface text). `message` is the rendered deny message.
    ///
    /// Called by [`PolicyPermissionGate`] on its local Deny decision. Additive
    /// DEFAULTED no-op (frozen-trait safe): only the stdio control-plane gate
    /// overrides it to enqueue the frame; interactive/headless transports (which
    /// have no outbound NDJSON stream) keep the no-op.
    async fn on_permission_denied(
        &self,
        name: &str,
        ctx: &PermissionCheckContext,
        decision_reason_type: Option<&str>,
        decision_reason: Option<&str>,
        message: &str,
    ) {
        let _ = (name, ctx, decision_reason_type, decision_reason, message);
    }

    /// Inspect the rule/safety result that a PreToolUse `allow` would feed to
    /// the permission pipeline, without opening its prompt. A Mod's real
    /// `tool.check` wraps this decision before any interactive resolution.
    async fn resolve_after_hook_allow_mod_core(
        &self,
        name: &str,
        input: &Value,
        ctx: &PermissionCheckContext,
    ) -> Result<HookAllowModCoreEvaluation, PermissionAbort> {
        let _ = (name, input, ctx);
        // A transport-only gate has no policy owner to recheck. Its native
        // hook-allow core is nevertheless a real clean Allow, not an absent
        // result that lets dispatch bypass an installed Mod `tool.check`.
        Ok(HookAllowModCoreEvaluation {
            resolution: PermissionResolution::Allow {
                rule_source: None,
                classifier_approved: false,
            },
            evaluation: None,
        })
    }

    /// Resolve a PermissionRequest hook allow with updatedInput or a required
    /// interaction. Rule-evaluating gates override this to recheck the rewritten
    /// input and preserve the native hard-deny-on-Ask behavior. A prompt-only
    /// transport has no rule layer to recheck, so its current default is clean
    /// Allow.
    async fn check_after_hook_allow_rewritten(
        &self,
        name: &str,
        input: &Value,
    ) -> PermissionDecision {
        let _ = (name, input);
        PermissionDecision::Allow
    }

    /// `Fxy`'s STANDING PermissionRequest-hook `allow` — the arm where the hook
    /// supplied no `updatedInput` and the tool does not require user interaction,
    /// so the oracle's re-check gate `if(a.updatedInput||e.requiresUserInteraction
    /// ?.())` is false and the allow returns UNCHECKED
    /// (`return {behavior:"allow", updatedInput:l, decisionReason:{type:"hook",…}}`).
    ///
    /// Unlike the captured PreToolUse core path
    /// ([`Self::resolve_after_hook_allow_mod_core`]) and
    /// [`Self::check_after_hook_allow_rewritten`] (`Fxy` WITH a re-check), this
    /// runs NO rule/mode verdict at all: an ordinary ask rule — the very reason
    /// the gate resolved `Ask` and fired the PermissionRequest hook — must not be
    /// re-evaluated, or the headless rescue is defeated in its primary use case.
    /// A rule-evaluating gate still records its per-allow auto-mode bookkeeping.
    ///
    /// The default is a wholesale [`PermissionDecision::Allow`], correct for
    /// gates with no rule layer.
    async fn honour_hook_allow(&self, name: &str, input: &Value) -> PermissionDecision {
        let _ = (name, input);
        PermissionDecision::Allow
    }

    /// Resolve permission when the session is in PLAN mode — i.e. the model has
    /// run `EnterPlanMode` and not yet exited.
    ///
    /// claude-code reads `toolPermissionContext.mode = 'plan'` LIVE on every
    /// permission check, so entering plan mode immediately activates the
    /// mutation backstop (plan-safe reads stay frictionless; un-ruled mutations
    /// are asked/denied). LingXi instead builds its `PermissionPolicy` once at
    /// boot with a fixed mode and holds it behind a shared `Arc`, so the boot
    /// mode would otherwise ignore a runtime `EnterPlanMode`. This method is the
    /// seam the turn loop calls (instead of [`Self::check`]) whenever the live
    /// `SessionState.plan_mode` flag is set.
    ///
    /// The default impl delegates to [`Self::check`]: a gate with no rule/mode
    /// layer (the interactive / no-op / adapter prompt transports) has nothing
    /// extra to enforce under plan mode, so it behaves identically. The
    /// rule-evaluating `PolicyPermissionGate` OVERRIDES this to authorize under
    /// [`crate`]'s `PermissionMode::Plan` via `authorize_with_mode`.
    ///
    /// The default delegates to [`Self::check`].
    async fn check_in_plan_mode(&self, name: &str, input: &Value) -> PermissionDecision {
        self.check(name, input).await
    }

    /// Source-first twin of [`Self::check_in_plan_mode`] carrying dispatch
    /// context and preserving a terminal [`PermissionAbort`].
    ///
    /// The turn loop uses this richer seam so plan-mode asks can keep the same
    /// `PermissionRequest` hook ordering and headless fail-closed behavior as
    /// the ordinary source-first path, while still authorizing under the live
    /// Plan mode overlay. The default preserves the frozen trait ABI by
    /// projecting the legacy 2-valued [`Self::check_in_plan_mode`] result into a
    /// sourced [`PermissionResolution`], and therefore never aborts.
    async fn resolve_detailed_in_plan_mode_or_abort(
        &self,
        name: &str,
        input: &Value,
        ctx: &PermissionCheckContext,
    ) -> Result<PermissionResolution, PermissionAbort> {
        let _ = ctx;
        Ok(match self.check_in_plan_mode(name, input).await {
            PermissionDecision::Allow => PermissionResolution::Allow {
                rule_source: None,
                classifier_approved: false,
            },
            PermissionDecision::Deny { reason } => PermissionResolution::Deny {
                reason,
                source: PermissionDecisionSource::Unspecified,
                rule_source: None,
                decision_reason_type: None,
                decision_reason: None,
                behavior_ask: false,
                content_blocks: Vec::new(),
            },
        })
    }

    /// Resolve a tool call to a SOURCED [`PermissionResolution`] WITHOUT yet
    /// consulting the inner prompt transport.
    ///
    /// This lets the turn loop fire the source-gated permission hooks the way
    /// claude-code does: `PermissionRequest` on an [`PermissionResolution::Ask`]
    /// (before the prompt), and `PermissionDenied` only on a
    /// [`PermissionResolution::Deny`] whose source is
    /// [`PermissionDecisionSource::Classifier`] (`toolExecution.ts:1075`).
    ///
    /// The default impl derives from [`Self::check`] — `Allow` → `Allow`, `Deny`
    /// → `Deny { source: Unspecified }` — so a gate with NO rule/source layer
    /// (the no-op / adapter transports that the turn loop uses directly when
    /// enforcement is off) never yields an `Ask` here; it has already resolved.
    /// The rule-evaluating `PolicyPermissionGate` OVERRIDES this to authorize
    /// WITHOUT delegating, returning the rule/mode/classifier source and an `Ask`
    /// for a would-be prompt. (Inner prompt transports — interactive / TUI — are
    /// never the turn loop's gate directly; they sit behind the policy gate, so
    /// the default's `check` call never triggers a prompt for the real gate
    /// types.) Additive DEFAULTED (frozen-trait safe).
    async fn resolve_detailed(&self, name: &str, input: &Value) -> PermissionResolution {
        match self.check(name, input).await {
            PermissionDecision::Allow => PermissionResolution::Allow {
                rule_source: None,
                classifier_approved: false,
            },
            PermissionDecision::Deny { reason } => PermissionResolution::Deny {
                reason,
                source: PermissionDecisionSource::Unspecified,
                rule_source: None,
                // The default has only a rendered String reason (no structured
                // `PermissionDecisionReason`), so the system-message discriminants
                // are unavailable here.
                decision_reason_type: None,
                decision_reason: None,
                behavior_ask: false,
                content_blocks: Vec::new(),
            },
        }
    }

    /// Like [`Self::resolve_detailed`], but carries dispatch context and
    /// preserves a terminal [`PermissionAbort`].
    ///
    /// The main turn loop uses this source-first surface so permission hooks
    /// retain their ordering. The default delegates to the legacy method and
    /// therefore never aborts.
    async fn resolve_detailed_or_abort(
        &self,
        name: &str,
        input: &Value,
        ctx: &PermissionCheckContext,
    ) -> Result<PermissionResolution, PermissionAbort> {
        let _ = ctx;
        Ok(self.resolve_detailed(name, input).await)
    }

    /// Tool-name targets of every TOOL-WIDE deny rule the gate enforces (a deny
    /// rule with NO rule-content — a blanket strip of that tool).
    ///
    /// The orchestrator filters the wire `tools` array by these names BEFORE the
    /// model sees them, 1:1 with claude-code `filterToolsByDenyRules` /
    /// `getDenyRuleForTool` (`tools.ts:262-269`): a tool whose name matches one of
    /// these (exact, OR an `mcp__server` prefix covering `mcp__server__tool`) is
    /// dropped from the advertised set. Each returned string is matched against an
    /// advertised tool's name by the SAME matcher the runtime check uses
    /// (`permission::tool_wide_name_matches`).
    ///
    /// The default returns `Vec::new()` — a gate with no rule layer (the
    /// interactive / no-op / adapter prompt transports) denies nothing tool-wide,
    /// so the wire-tool set is UNCHANGED (byte-identical / regression-safe). Only
    /// the rule-evaluating `PolicyPermissionGate` OVERRIDES this to surface its
    /// policy's tool-wide deny names. Additive DEFAULTED (frozen-trait safe).
    async fn tool_wide_deny_names(&self) -> Vec<String> {
        Vec::new()
    }

    /// If `Agent(<agent_type>)` is DENIED by a permission rule, the rule's source
    /// identifier (claude-code `SettingSource` raw string — e.g. `localSettings`,
    /// `projectSettings`, `cliArg`), else `None`.
    ///
    /// 1:1 with claude-code `getDenyRuleForAgent` (`o5e(ctx, "Agent", type)`):
    /// finds a DENY rule whose `toolName === "Agent"` and whose `ruleContent`
    /// equals the agent type exactly. The Agent tool uses this to reject a model
    /// selection of a denied subagent type with the byte-exact message
    /// `Agent type '<t>' has been denied by permission rule 'Agent(<t>)' from
    /// <source>.` (`AgentTypeError`). The deny rule keys on the `"Agent"` tool
    /// name.
    ///
    /// The default returns `None` — a gate with no rule layer denies no agent
    /// type. Only `PolicyPermissionGate` OVERRIDES it. Additive DEFAULTED
    /// (frozen-trait safe).
    async fn agent_type_deny(&self, agent_type: &str) -> Option<String> {
        let _ = agent_type;
        None
    }

    /// The set of agent-type names that are denied by a CONTENT-ful `Agent(<x>)`
    /// deny rule — the listing-filter set.
    ///
    /// 1:1 with claude-code `Pxe(list, ctx, "Agent")`: collects every DENY rule
    /// whose `toolName === "Agent"` and that carries a (non-undefined)
    /// `ruleContent`, so the advertised agent catalog and the `Available agents:`
    /// error lists exclude denied types — the 2.1.186 Agent(type)-restriction
    /// change that filters the prompt/list the model sees.
    ///
    /// The default returns `Vec::new()` — no agent types filtered. Only
    /// `PolicyPermissionGate` OVERRIDES it. Additive DEFAULTED (frozen-trait
    /// safe).
    async fn agent_deny_content_types(&self) -> Vec<String> {
        Vec::new()
    }

    /// Set the LIVE session permission mode (claude-code `set_permission_mode`
    /// control_request / `handleSetPermissionMode`). `mode` is the wire string
    /// (`default`/`plan`/`acceptEdits`/`bypassPermissions`/`dontAsk`/`auto`); the
    /// gate parses + validates it and applies it to subsequent checks.
    ///
    /// Returns `Ok(())` on success, `Err(msg)` for an invalid or disallowed mode
    /// (e.g. `bypassPermissions` disabled by settings). The mode is passed as a
    /// `&str` rather than a `PermissionMode` because this crate is below
    /// `lingxi-permission` in the dependency graph and cannot name that enum.
    ///
    /// The default is a no-op `Ok(())`: a gate with no rule/mode layer (the
    /// interactive / no-op / adapter prompt transports) has no mode to mutate.
    /// Only the rule-evaluating `PolicyPermissionGate` OVERRIDES this with an
    /// interior-mutable mode cell read per check. Additive DEFAULTED
    /// (frozen-trait safe).
    async fn set_permission_mode(&self, mode: &str) -> Result<(), String> {
        let _ = mode;
        Ok(())
    }

    /// Apply a live administrator-managed `disableAutoMode` setting.
    ///
    /// The settings watcher calls this after firing the `ConfigChange` hook
    /// for a managed-settings file.  Rule-free prompt transports have no
    /// local auto-mode state, so the default is deliberately a no-op.  A
    /// policy-backed gate updates its thread-safe kill switch and evicts an
    /// active Auto mode; clearing the setting only re-opens future requests
    /// and never silently enters Auto again.
    fn update_auto_mode_disabled(&self, disabled: bool) {
        let _ = disabled;
    }

    /// Whether the host may present `bypassPermissions` as a selectable mode.
    /// A policy gate keeps this false when an administrator/settings killswitch
    /// disables the mode; a host-side risk acknowledgement can still be needed
    /// before [`Self::set_permission_mode`] accepts the transition.
    ///
    /// The default is `false` so prompt-only gates never advertise a bypass
    /// control they cannot enforce.
    fn can_request_bypass_permissions(&self) -> bool {
        false
    }

    /// Whether the engine may offer a per-prompt transition to Auto mode.
    /// Prompt transports must not derive this from local mode/tool heuristics;
    /// policy-backed gates override it with their live availability checks.
    fn can_request_auto_mode(&self) -> bool {
        false
    }

    /// Record an explicit interactive acknowledgement before allowing a live
    /// transition into `bypassPermissions`.
    ///
    /// The default rejects the request: only policy-backed gates can safely
    /// retain this acknowledgement and apply it to subsequent authorization.
    fn confirm_bypass_permissions(&self) -> Result<(), String> {
        Err("Bypass Permissions mode is not available for this session".to_string())
    }

    /// Set or clear the LIVE per-MCP-server permission-mode override used by
    /// Claude's control-channel `set_mcp_permission_mode_override` surface.
    ///
    /// `mode == Some("default")` or `Some("auto")` stores a tighten-only
    /// override for `server_name`; `mode == None` clears it. The default is a
    /// no-op `Ok(())`: only `PolicyPermissionGate` carries a live per-server mode
    /// layer. Additive DEFAULTED (frozen-trait safe).
    async fn set_mcp_permission_mode_override(
        &self,
        server_name: &str,
        mode: Option<&str>,
    ) -> Result<(), String> {
        let _ = (server_name, mode);
        Ok(())
    }

    /// The LIVE effective permission mode as a wire string
    /// (`default`/`plan`/`acceptEdits`/`bypassPermissions`/`dontAsk`/`auto`), or
    /// `None` for a gate with no mode layer (the interactive / no-op / adapter
    /// transports). Read when snapshotting session state across the in-process
    /// `/resume`, `/branch`, and `/rewind` re-mount so the user's mid-session
    /// Shift+Tab mode is carried into the rebuilt runtime instead of resetting to
    /// the CLI/config default. Only [`PolicyPermissionGate`] overrides this;
    /// additive DEFAULTED (frozen-trait safe), returning `None`.
    fn permission_mode(&self) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod input_projection_tests {
    use super::*;

    #[test]
    fn permission_context_preserves_exact_input_and_rejects_stale_association() {
        let input = Utf16JsonProjection::parse(r#"{"text":"\ud800","\ud901":"value"}"#).unwrap();
        let ctx = PermissionCheckContext {
            input_projection: Some(input.clone()),
            ..Default::default()
        };
        assert_eq!(ctx.projected_input(&input.value).unwrap(), input);
        assert!(
            ctx.projected_input(&serde_json::json!({"text":"different"}))
                .is_err()
        );
    }

    #[test]
    fn permission_context_rejects_invalid_projection_pointers() {
        let mut input = Utf16JsonProjection::parse(r#"{"text":"\ud800"}"#).unwrap();
        input.strings[0].pointer = "/missing".into();
        let ctx = PermissionCheckContext {
            input_projection: Some(input.clone()),
            ..Default::default()
        };
        assert!(ctx.projected_input(&input.value).is_err());
    }

    #[test]
    fn permission_outcome_clone_retains_utf16_updated_input() {
        let updated = Utf16JsonProjection::parse(r#"{"text":"\udfff","\ud901":"value"}"#).unwrap();
        let outcome = PermissionOutcome::Allow {
            updated_input: Some(updated.clone()),
            permission_updates: Vec::new(),
            decision_classification: None,
        };
        let PermissionOutcome::Allow {
            updated_input: Some(cloned),
            ..
        } = outcome.clone()
        else {
            panic!("allow")
        };
        assert_eq!(
            cloned.to_json_string().unwrap(),
            updated.to_json_string().unwrap()
        );
    }
}
