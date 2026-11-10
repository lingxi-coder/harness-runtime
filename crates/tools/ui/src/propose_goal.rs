//! Claude Code 2.1.322 model-proposed session goals.
//!
//! Approval and kickoff delivery belong to the session host. A successful tool
//! result only records a queued proposal; the current goal remains in force
//! until the ordinary `/goal` command consumes its kickoff.

use async_trait::async_trait;
use lingxi_core::host::goal::{GoalProposalHost, ModelProposedGoals};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    generated_builtin_result, generated_builtin_schema, DescriptionOptions, PromptOptions, Tool,
    ToolCallResult, ToolError, ToolInputRepairMetadata, ToolInputRepairSchema,
    ToolInputSchemaProvenance, ToolStaticContext,
};
use tool_api::BuiltinToolContext;

pub use lingxi_core::host::goal::canonicalize_goal_proposal_condition;

/// Native tool name.
pub const PROPOSE_GOAL_TOOL_NAME: &str = "ProposeGoal";
/// Native feature flag; proposals are hidden unless explicitly enabled.
pub const PROPOSE_GOAL_FLAG: &str = "tengu_propose_goal";
/// Native input and canonicalized display cap, measured in UTF-16 units.
pub const MAX_CONDITION_UNITS: usize = 500;
/// Native search hint.
pub const SEARCH_HINT: &str =
    "propose a session goal condition for the user to approve with one keypress";
/// Native short description.
pub const DESCRIPTION: &str = "Propose a session goal condition, with one-keypress user approval; once set, Claude keeps working until a separate evaluator confirms it is met";
/// Native model-facing prompt.
pub const PROMPT: &str = "Propose a completion condition for this session's work — a goal that keeps you working until a separate evaluator confirms it is met. Non-blocking: the proposal renders alongside your work, so keep working while it is handled.

ask_user true (the default) asks the user first, with a one-keypress approval dialog. If they decline you will not be notified — do not ask about the decision and do not re-propose the same or a reworded condition. Set ask_user false — which sets the goal directly, with no dialog — ONLY when the user's own words in this conversation stated this outcome as what they want; if you inferred it from their intent or the task's shape — or are in doubt — ask. Either path confirms a set goal with a kickoff message; until that message arrives, no new goal is active.

Propose only when the user has asked for an outcome with a verifiable end state (\"make the tests pass\", \"migrate every call site\") and the work spans multiple turns. Not for one-off tasks, and never to widen scope: the condition must follow from their request.

The evaluator verifies the condition from the conversation alone — it cannot run commands or read files — so state one measurable end state with its check (e.g. \"bun test exits 0\"), in at most 500 characters. One goal is active at a time; a newly approved or directly set proposal replaces the current one.";

/// Native result when approval is requested.
pub const RESULT_APPROVAL: &str = "Sent the goal proposal to the user for approval. Continue working — do not wait for their decision. If they approve, the proposed goal is set and you will receive a kickoff message; until then, no new goal is active — any previously set goal remains in effect. If they decline you will not be notified — do not ask about the decision or re-propose the same condition.";
/// Native result when a direct kickoff is queued.
pub const RESULT_DIRECT: &str = "Setting the goal now, without an approval dialog — the user sees it being set and can clear it with /goal clear. It becomes active at the end of this turn, when you will receive a kickoff message confirming it; until that message arrives, any previously set goal remains in effect. Continue working — do not wait for the kickoff.";

/// Native guard refusals, retained separately for host and oracle checks.
pub const ERR_AGENT: &str = "ProposeGoal cannot be used in agent contexts";
/// Refusal outside an interactive local main session.
pub const ERR_SESSION: &str = "Goal proposals are only available in interactive local sessions.";
/// Refusal after display canonicalization removes every visible character.
pub const ERR_EMPTY: &str = "The goal condition is empty once whitespace and invisible characters are removed. Provide a visible condition.";
/// Refusal when canonical display expansion exceeds the approval dialog cap.
pub const ERR_CANONICAL_LENGTH: &str = "The goal condition exceeds 500 characters once canonicalized for display (tabs expand to spaces). Shorten the condition — the user must be able to read all of it in the approval dialog.";
/// Refusal when a proposal attempts to clear a goal.
pub const ERR_CLEAR: &str = "ProposeGoal only proposes a new goal; it cannot clear one. The user can clear an active goal with /goal clear.";
/// Refusal while the enforcing permission mode is plan.
pub const ERR_PLAN: &str = "Plan mode is active, so a goal cannot be proposed yet. Keep planning; propose the goal after the plan is approved.";
/// Refusal when trusted settings disable model proposals.
pub const ERR_DISABLED: &str = "The user has disabled model-proposed goals in their settings. Do not propose goals; the user can set one themselves with /goal.";
/// Refusal when no real approval transport is mounted.
pub const ERR_DIALOG: &str = "Goal proposals need an interactive session to render the approval prompt; none is available here.";
/// Refusal while another proposal is awaiting a decision.
pub const ERR_PENDING: &str = "A goal proposal is already awaiting the user's decision. Keep working; if it is approved you will receive a kickoff message.";

/// Native strict input schema.
pub static INPUT_SCHEMA: Lazy<Utf16JsonProjection> = Lazy::new(|| {
    generated_builtin_schema(&json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "condition": {
                "type": "string", "minLength": 1, "maxLength": MAX_CONDITION_UNITS,
                "description": "The completion condition to propose, written so a separate evaluator can verify it from the conversation (e.g. \"all tests in test/auth pass (bun test exits 0)\"). At most 500 characters — the user must be able to read the whole condition in the approval dialog."
            },
            "ask_user": {
                "type": "boolean",
                "description": "Whether to ask the user for approval before the goal is set. Defaults to true — an approval dialog is shown. Set false ONLY when the user's own words in this conversation stated this outcome as what they want; the goal is then set directly, with a visible notice in the transcript, and the user can clear it with /goal clear."
            }
        },
        "required": ["condition"]
    }))
});

/// Native object output schema; its parser strips unknown fields.
pub static OUTPUT_SCHEMA: Lazy<Utf16JsonProjection> = Lazy::new(|| {
    generated_builtin_schema(&json!({
        "type": "object",
        "properties": {
            "condition": {
                "type": "string",
                "description": "The condition shown to the user for approval, or set directly when ask_user was false"
            },
            "askUser": {
                "type": "boolean",
                "description": "Whether the user was asked for approval (true) or the goal was set directly (false)"
            }
        },
        "required": ["condition", "askUser"]
    }))
});

// The shared flat parser intentionally rejects length vocabulary. Validate the
// Native strict shape with its supported subset, then add source-unit lengths.
static INPUT_SHAPE: Lazy<Utf16JsonProjection> = Lazy::new(|| {
    Utf16JsonProjection::parse(
        r#"{"type":"object","properties":{"condition":{"type":"string"},"ask_user":{"type":"boolean"}},"required":["condition"],"additionalProperties":false}"#,
    )
    .expect("locally generated strict input shape is valid JSON")
});

/// Source-preserving Native auto-classifier projection.
#[must_use]
pub fn proposal_classifier_input(input: &Utf16JsonProjection) -> Utf16JsonProjection {
    let ask = input
        .get_property("ask_user")
        .and_then(|value| value.as_bool())
        != Some(false);
    let mut units: Vec<u16> = format!("ask_user={ask}: ").encode_utf16().collect();
    if let Some(condition) = input.string_units("/condition") {
        units.extend(condition);
    }
    tool_api::tool_trait::source_text_projection_units(&units)
}

/// Tool implementation over an optional authoritative session host.
pub struct ProposeGoalTool {
    host: Option<Arc<dyn GoalProposalHost>>,
    bus: Arc<telemetry::AnalyticsBus>,
}

impl ProposeGoalTool {
    /// Register even without a transport; catalog admission then hides it.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self {
            host: ctx.goal_proposal_host,
            bus: ctx.bus,
        }
    }
}

fn invalid(message: &str) -> ToolError {
    ToolError::InvalidInput(message.into())
}

#[async_trait]
impl Tool for ProposeGoalTool {
    fn name(&self) -> &str {
        PROPOSE_GOAL_TOOL_NAME
    }

    fn input_schema(&self) -> &Utf16JsonProjection {
        &INPUT_SCHEMA
    }

    fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
        ToolInputRepairMetadata::Known {
            schema: ToolInputRepairSchema::Advertised,
            schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }

    fn native_input_validation(&self) -> bool {
        true
    }

    fn parse_native_input(
        &self,
        input: &Utf16JsonProjection,
    ) -> Option<Result<Utf16JsonProjection, tool_api::native_schema::NativeSchemaError>> {
        use tool_api::native_schema::{
            collect_flat_issues, format_issues, format_raw_issues, normalize_flat_input,
            NativeSchemaError,
        };
        let mut issues = collect_flat_issues(&INPUT_SHAPE, input)?;
        if let Some(units) = input.string_units("/condition") {
            let issue = if units.is_empty() {
                Some(json!({
                    "origin": "string", "code": "too_small", "minimum": 1,
                    "inclusive": true, "path": ["condition"],
                    "message": "Too small: expected string to have >=1 characters"
                }))
            } else if units.len() > MAX_CONDITION_UNITS {
                Some(json!({
                    "origin": "string", "code": "too_big", "maximum": MAX_CONDITION_UNITS,
                    "inclusive": true, "path": ["condition"],
                    "message": "Too big: expected string to have <=500 characters"
                }))
            } else {
                None
            };
            if let Some(issue) = issue {
                issues.insert(0, generated_builtin_result(&issue));
            }
        }
        Some(if issues.is_empty() {
            Ok(normalize_flat_input(input).unwrap_or_else(|| input.clone()))
        } else {
            Err(NativeSchemaError {
                display: format_issues(self.name(), &issues),
                raw: format_raw_issues(&issues),
            })
        })
    }

    fn output_schema(&self) -> Option<&Utf16JsonProjection> {
        Some(&OUTPUT_SCHEMA)
    }

    async fn parse_output(
        &self,
        output: &Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ToolError> {
        let invalid = || invalid("ProposeGoal returned invalid output");
        if output.string_units("/condition").is_none()
            || output
                .get_property("askUser")
                .and_then(|value| value.as_bool())
                .is_none()
        {
            return Err(invalid());
        }
        Utf16JsonProjection::object(vec![
            (
                "condition".to_owned(),
                output.subprojection("/condition").map_err(|_| invalid())?,
            ),
            (
                "askUser".to_owned(),
                output.subprojection("/askUser").map_err(|_| invalid())?,
            ),
        ])
        .map_err(|_| invalid())
    }

    fn map_result_text(&self, result: &Utf16JsonProjection) -> Option<Utf16JsonProjection> {
        Some(
            if result
                .get_property("askUser")
                .and_then(|value| value.as_bool())
                == Some(false)
            {
                RESULT_DIRECT
            } else {
                RESULT_APPROVAL
            }
            .into(),
        )
    }

    fn is_enabled(&self, ctx: &ToolStaticContext) -> bool {
        ctx.feature_flags
            .get(PROPOSE_GOAL_FLAG)
            .copied()
            .unwrap_or_else(|| telemetry::flag_bool(PROPOSE_GOAL_FLAG, false))
            && self.host.as_ref().is_some_and(|host| host.is_available())
    }

    fn should_defer(&self) -> bool {
        true
    }

    fn search_hint(&self) -> Option<Utf16JsonProjection> {
        Some(SEARCH_HINT.into())
    }

    fn max_result_size_chars(&self) -> usize {
        1000
    }

    fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
        false
    }

    fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
        true
    }

    async fn check_permissions(
        &self,
        input: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "ProposeGoal owns its nonblocking approval transport".into(),
            },
            updated_input: Some(input.clone()),
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
        DESCRIPTION.into()
    }

    async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
        PROMPT.into()
    }

    async fn call(
        &self,
        input: Utf16JsonProjection,
        ctx: ToolUseContext,
        _: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        if ctx.agent_id.is_some() {
            return Err(invalid(ERR_AGENT));
        }
        let host = self.host.as_ref().ok_or_else(|| invalid(ERR_SESSION))?;
        let state = host.state().await;
        if !state.interactive_local || ctx.is_scheduled_turn {
            return Err(invalid(ERR_SESSION));
        }
        let units = input
            .string_units("/condition")
            .ok_or_else(|| invalid("ProposeGoal: condition must be a string"))?;
        let condition = canonicalize_goal_proposal_condition(&units);
        if condition.is_empty() {
            return Err(invalid(ERR_EMPTY));
        }
        let condition_units = condition.encode_utf16().count();
        if condition_units > MAX_CONDITION_UNITS {
            return Err(invalid(ERR_CANONICAL_LENGTH));
        }
        if ["clear", "stop", "off", "reset", "none", "cancel"]
            .contains(&condition.to_lowercase().as_str())
        {
            return Err(invalid(ERR_CLEAR));
        }
        if let Some(message) = state.evaluator_gate_error {
            return Err(invalid(&message));
        }
        if state.plan_mode {
            return Err(invalid(ERR_PLAN));
        }
        if state.setting == ModelProposedGoals::Disabled {
            return Err(invalid(ERR_DISABLED));
        }
        let requested_ask = input
            .get_property("ask_user")
            .and_then(|value| value.as_bool())
            != Some(false);
        let ask_user = state.setting == ModelProposedGoals::AlwaysAsk || requested_ask;
        if !state.dialog_available {
            return Err(invalid(ERR_DIALOG));
        }
        host.submit(condition.clone().into(), ask_user)
            .await
            .map_err(|message| invalid(&message))?;

        let mut metadata: LogEventMetadata = HashMap::new();
        metadata.insert(
            "promptLength".into(),
            AnalyticsValue::Int(condition_units as i64),
        );
        metadata.insert("askUser".into(), AnalyticsValue::Bool(ask_user));
        metadata.insert(
            "forcedAsk".into(),
            AnalyticsValue::Bool(ask_user && !requested_ask),
        );
        self.bus.log_event("tengu_goal_proposed", metadata).await;

        let data = Utf16JsonProjection::object(vec![
            ("condition".into(), condition.into()),
            ("askUser".into(), generated_builtin_result(&json!(ask_user))),
        ])
        .expect("generated proposal result is valid JSON");
        Ok(
            ToolCallResult::from_data(data).with_model_text(if ask_user {
                RESULT_APPROVAL
            } else {
                RESULT_DIRECT
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::goal::GoalProposalState;
    use mobile_linux_api::ProcessOutput;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use tool_api::test_support::{fresh_ctx, fresh_tx, shell_test_ctx};

    struct Host {
        state: Mutex<GoalProposalState>,
        submissions: Mutex<Vec<(Utf16JsonProjection, bool)>>,
        pending: AtomicBool,
    }

    impl Host {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                state: Mutex::new(GoalProposalState {
                    interactive_local: true,
                    dialog_available: true,
                    ..GoalProposalState::default()
                }),
                submissions: Mutex::new(vec![]),
                pending: AtomicBool::new(false),
            })
        }
    }

    #[async_trait]
    impl GoalProposalHost for Host {
        fn is_available(&self) -> bool {
            let state = self.state.lock().unwrap();
            state.interactive_local && state.setting != ModelProposedGoals::Disabled
        }

        async fn state(&self) -> GoalProposalState {
            self.state.lock().unwrap().clone()
        }

        async fn submit(
            &self,
            condition: Utf16JsonProjection,
            ask_user: bool,
        ) -> Result<(), String> {
            if self.pending.load(Ordering::SeqCst) {
                return Err(ERR_PENDING.into());
            }
            if ask_user {
                self.pending.store(true, Ordering::SeqCst);
            }
            self.submissions.lock().unwrap().push((condition, ask_user));
            Ok(())
        }
    }

    fn context(host: Option<Arc<dyn GoalProposalHost>>) -> BuiltinToolContext {
        let mut context = shell_test_ctx(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        });
        context.goal_proposal_host = host;
        context
    }

    fn tool(host: &Arc<Host>) -> ProposeGoalTool {
        ProposeGoalTool::new(context(Some(host.clone())))
    }

    fn input(condition: &str, ask: Option<bool>) -> Utf16JsonProjection {
        let mut fields = vec![("condition".into(), condition.into())];
        if let Some(ask) = ask {
            fields.push(("ask_user".into(), generated_builtin_result(&json!(ask))));
        }
        Utf16JsonProjection::object(fields).unwrap()
    }

    fn assert_error(result: Result<ToolCallResult, ToolError>, expected: &str) {
        match result {
            Err(error) => assert_eq!(error.model_facing_message().string_at(""), Some(expected)),
            Ok(_) => panic!("expected refusal: {expected}"),
        }
    }

    #[tokio::test]
    async fn default_proposal_returns_while_the_user_decision_is_pending() {
        let host = Host::new();
        let result = tool(&host)
            .call(input(" bun test exits 0 ", None), fresh_ctx(), fresh_tx())
            .await
            .unwrap();
        assert_eq!(
            result.data.string_at("/condition"),
            Some("bun test exits 0")
        );
        assert_eq!(
            result.data.get_property("askUser").unwrap().as_bool(),
            Some(true)
        );
        assert_eq!(
            result.model_content.unwrap().string_at(""),
            Some(RESULT_APPROVAL)
        );
        assert!(result.new_messages.is_empty());
        assert!(host.pending.load(Ordering::SeqCst));
        assert_eq!(host.submissions.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn direct_proposal_is_queued_without_a_new_conversation_message() {
        let host = Host::new();
        let result = tool(&host)
            .call(
                input("migrate all callers", Some(false)),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(
            result.data.get_property("askUser").unwrap().as_bool(),
            Some(false)
        );
        assert_eq!(
            result.model_content.unwrap().string_at(""),
            Some(RESULT_DIRECT)
        );
        assert!(result.new_messages.is_empty());
        assert!(!host.pending.load(Ordering::SeqCst));
        assert!(!host.submissions.lock().unwrap()[0].1);
    }

    #[tokio::test]
    async fn always_ask_overrides_a_direct_proposal() {
        let host = Host::new();
        host.state.lock().unwrap().setting = ModelProposedGoals::AlwaysAsk;
        let result = tool(&host)
            .call(input("tests pass", Some(false)), fresh_ctx(), fresh_tx())
            .await
            .unwrap();
        assert_eq!(
            result.data.get_property("askUser").unwrap().as_bool(),
            Some(true)
        );
        assert!(host.submissions.lock().unwrap()[0].1);
    }

    #[tokio::test]
    async fn no_transport_and_disabled_setting_never_queue_a_kickoff() {
        let host = Host::new();
        host.state.lock().unwrap().dialog_available = false;
        assert_error(
            tool(&host)
                .call(input("tests pass", Some(false)), fresh_ctx(), fresh_tx())
                .await,
            ERR_DIALOG,
        );
        host.state.lock().unwrap().setting = ModelProposedGoals::Disabled;
        assert_error(
            tool(&host)
                .call(input("tests pass", None), fresh_ctx(), fresh_tx())
                .await,
            ERR_DISABLED,
        );
        assert!(host.submissions.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn native_guard_order_prevents_proposals_in_plan_or_agent_contexts() {
        let host = Host::new();
        {
            let mut state = host.state.lock().unwrap();
            state.plan_mode = true;
            state.setting = ModelProposedGoals::Disabled;
            state.evaluator_gate_error = Some("evaluator held".into());
        }
        assert_error(
            tool(&host)
                .call(input("tests pass", None), fresh_ctx(), fresh_tx())
                .await,
            "evaluator held",
        );
        host.state.lock().unwrap().evaluator_gate_error = None;
        assert_error(
            tool(&host)
                .call(input("tests pass", None), fresh_ctx(), fresh_tx())
                .await,
            ERR_PLAN,
        );
        let mut agent = fresh_ctx();
        agent.agent_id = Some(lingxi_core::types::AgentId::new());
        assert_error(
            tool(&host).call(input("", None), agent, fresh_tx()).await,
            ERR_AGENT,
        );
        assert!(host.submissions.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn noninteractive_remote_or_scheduled_sessions_do_not_queue() {
        let host = Host::new();
        host.state.lock().unwrap().interactive_local = false;
        assert_error(
            tool(&host)
                .call(input("", None), fresh_ctx(), fresh_tx())
                .await,
            ERR_SESSION,
        );
        host.state.lock().unwrap().interactive_local = true;
        let mut scheduled = fresh_ctx();
        scheduled.is_scheduled_turn = true;
        assert_error(
            tool(&host)
                .call(input("tests pass", None), scheduled, fresh_tx())
                .await,
            ERR_SESSION,
        );
        let unavailable = ProposeGoalTool::new(context(None));
        assert_error(
            unavailable
                .call(input("tests pass", None), fresh_ctx(), fresh_tx())
                .await,
            ERR_SESSION,
        );
        assert!(host.submissions.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn pending_proposal_is_not_replaced_by_another_tool_call() {
        let host = Host::new();
        tool(&host)
            .call(input("first", None), fresh_ctx(), fresh_tx())
            .await
            .unwrap();
        assert_error(
            tool(&host)
                .call(input("second", Some(false)), fresh_ctx(), fresh_tx())
                .await,
            ERR_PENDING,
        );
        assert_eq!(host.submissions.lock().unwrap().len(), 1);
    }

    #[test]
    fn canonicalizer_matches_native_line_controls_and_display_tab_stops() {
        for (raw, expected) in [
            (" \tship\r\n\u{2028}\u{2029}it\t", "ship it"),
            ("界\tready", "界      ready"),
            ("😀\tready", "😀      ready"),
            ("e\u{301}\tready", "e\u{301}       ready"),
            ("a\t\tb", "a               b"),
            ("a\u{85}b\0c", "a\u{fffd}b\u{fffd}c"),
            ("a\u{200b}\u{e000}\u{378}\u{fe0f}\u{034f}b", "ab"),
            ("👨\u{200d}👩\u{200d}👧", "👨👩👧"),
        ] {
            assert_eq!(
                canonicalize_goal_proposal_condition(&raw.encode_utf16().collect::<Vec<_>>()),
                expected
            );
        }
        assert_eq!(
            canonicalize_goal_proposal_condition(&[0xd800, b'a' as u16, 0xdc00]),
            "a"
        );
    }

    #[tokio::test]
    async fn invisible_conditions_and_clear_tokens_are_rejected_before_policy() {
        let host = Host::new();
        host.state.lock().unwrap().plan_mode = true;
        for raw in ["\u{200b}\u{e000}", "  ", "\r\n"] {
            assert_error(
                tool(&host)
                    .call(input(raw, None), fresh_ctx(), fresh_tx())
                    .await,
                ERR_EMPTY,
            );
        }
        for token in ["clear", "STOP", "Off", "Reset", "none", "CANCEL"] {
            assert_error(
                tool(&host)
                    .call(input(token, None), fresh_ctx(), fresh_tx())
                    .await,
                ERR_CLEAR,
            );
        }
        assert!(host.submissions.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn canonicalized_length_uses_utf16_and_expanded_tabs() {
        let host = Host::new();
        let tool = tool(&host);
        let exact = "😀".repeat(250);
        assert!(tool
            .parse_native_input(&input(&exact, None))
            .unwrap()
            .is_ok());
        tool.call(input(&exact, Some(false)), fresh_ctx(), fresh_tx())
            .await
            .unwrap();
        let over = format!("{exact}x");
        assert!(tool
            .parse_native_input(&input(&over, None))
            .unwrap()
            .is_err());
        assert_error(
            tool.call(
                input(&format!("a{}x", "\t".repeat(63)), None),
                fresh_ctx(),
                fresh_tx(),
            )
            .await,
            ERR_CANONICAL_LENGTH,
        );
    }

    #[tokio::test]
    async fn strict_input_and_stripping_output_use_source_preserving_parsers() {
        let host = Host::new();
        let tool = tool(&host);
        for value in [
            json!({}),
            json!({"condition": ""}),
            json!({"condition": "a", "ask_user": null}),
            json!({"condition": "a", "unknown": 1}),
        ] {
            assert!(tool
                .parse_native_input(&generated_builtin_result(&value))
                .unwrap()
                .is_err());
        }
        let source =
            Utf16JsonProjection::parse(r#"{"condition":"\ud800","askUser":false,"extra":1}"#)
                .unwrap();
        let parsed = tool.parse_output(&source).await.unwrap();
        assert_eq!(parsed.string_units("/condition"), Some(vec![0xd800]));
        assert!(parsed.get_property("extra").is_none());
        for value in [
            json!({"condition": "a"}),
            json!({"condition": 1, "askUser": true}),
            json!({"condition": "a", "askUser": null}),
        ] {
            assert!(tool
                .parse_output(&generated_builtin_result(&value))
                .await
                .is_err());
        }
    }

    #[test]
    fn feature_flag_host_capability_and_policy_all_control_catalog_admission() {
        let host = Host::new();
        let tool = tool(&host);
        let mut enabled = ToolStaticContext::default();
        enabled.feature_flags.insert(PROPOSE_GOAL_FLAG.into(), true);
        assert!(tool.is_enabled(&enabled));
        enabled
            .feature_flags
            .insert(PROPOSE_GOAL_FLAG.into(), false);
        assert!(!tool.is_enabled(&enabled));
        enabled.feature_flags.insert(PROPOSE_GOAL_FLAG.into(), true);
        host.state.lock().unwrap().setting = ModelProposedGoals::Disabled;
        assert!(!tool.is_enabled(&enabled));
        assert!(!ProposeGoalTool::new(context(None)).is_enabled(&enabled));
    }

    #[test]
    fn classifier_projection_keeps_source_text_and_native_ask_default() {
        let source =
            Utf16JsonProjection::parse(r#"{"condition":"\ud800","ask_user":false}"#).unwrap();
        let result = proposal_classifier_input(&source);
        let mut expected: Vec<u16> = "ask_user=false: ".encode_utf16().collect();
        expected.push(0xd800);
        assert_eq!(result.string_units(""), Some(expected));
        assert_eq!(
            proposal_classifier_input(&input("ready", None)).string_at(""),
            Some("ask_user=true: ready")
        );
    }

    fn native_fixture() -> Utf16JsonProjection {
        Utf16JsonProjection::parse(include_str!(
            "../../../command-api/tests/fixtures/goal_2_1_322.json"
        ))
        .expect("native Goal fixture retains exact UTF-16 source")
    }

    #[test]
    fn native_322_canonicalization_matches_every_replayed_source_case() {
        let fixture = native_fixture();
        for index in 0..fixture.array_len("/normalization").unwrap() {
            let row = fixture
                .subprojection(&format!("/normalization/{index}"))
                .unwrap();
            let actual = canonicalize_goal_proposal_condition(&row.string_units("/input").unwrap());
            assert_eq!(
                actual.encode_utf16().collect::<Vec<_>>(),
                row.string_units("/expected").unwrap(),
                "{}",
                row.string_at("/name").unwrap()
            );
        }
    }

    #[tokio::test]
    async fn native_322_prompt_schema_metadata_results_and_classifier_bytes_match() {
        let fixture = native_fixture();
        let host = Host::new();
        let tool = tool(&host);
        assert_eq!(
            tool.description(
                &input("ready", None),
                &DescriptionOptions {
                    is_non_interactive_session: false
                }
            )
            .await
            .encode_utf16()
            .collect::<Vec<_>>(),
            fixture.string_units("/proposal/description").unwrap()
        );
        assert_eq!(
            tool.prompt(&PromptOptions::default())
                .await
                .string_units(""),
            fixture.string_units("/proposal/prompt")
        );
        assert_eq!(
            tool.search_hint().unwrap().string_units(""),
            fixture.string_units("/proposal/searchHint")
        );
        for (pointer, schema) in [
            ("/proposal/inputSchema", tool.input_schema()),
            ("/proposal/outputSchema", tool.output_schema().unwrap()),
        ] {
            let shape = fixture.subprojection(pointer).unwrap();
            for index in 0..shape.array_len("/fields").unwrap() {
                let field = shape.subprojection(&format!("/fields/{index}")).unwrap();
                let name = field.string_at("/name").unwrap();
                let actual = schema
                    .subprojection(&format!("/properties/{name}"))
                    .unwrap();
                for key in ["type", "description"] {
                    assert_eq!(
                        actual.string_units(&format!("/{key}")),
                        field.string_units(&format!("/{key}")),
                        "{name}.{key}"
                    );
                }
                for key in ["minLength", "maxLength"] {
                    assert_eq!(
                        actual.get_property(key).and_then(|value| value.as_number()),
                        field.get_property(key).and_then(|value| value.as_number()),
                        "{name}.{key}"
                    );
                }
                let required = (0..schema.array_len("/required").unwrap()).any(|required| {
                    schema.string_at(&format!("/required/{required}")) == Some(name)
                });
                assert_eq!(
                    required,
                    field
                        .get_property("optional")
                        .and_then(|value| value.as_bool())
                        != Some(true),
                    "{name} requiredness"
                );
            }
        }
        for (ask, key) in [(true, "approval"), (false, "direct")] {
            let result = generated_builtin_result(&json!({"askUser": ask}));
            assert_eq!(
                tool.map_result_text(&result).unwrap().string_units(""),
                fixture.string_units(&format!("/proposal/results/{key}"))
            );
        }
        for index in 0..fixture.array_len("/proposal/classifierInputs").unwrap() {
            let row = fixture
                .subprojection(&format!("/proposal/classifierInputs/{index}"))
                .unwrap();
            assert_eq!(
                proposal_classifier_input(&row.subprojection("/input").unwrap()).string_units(""),
                row.string_units("/expected")
            );
        }
    }

    #[tokio::test]
    async fn native_322_proposal_call_guards_and_accepted_conditions_match() {
        let fixture = native_fixture();
        for index in 0..fixture.array_len("/proposal/cases").unwrap() {
            let row = fixture
                .subprojection(&format!("/proposal/cases/{index}"))
                .unwrap();
            let facts = row.subprojection("/facts").unwrap();
            let fact = |pointer: &str| {
                facts.at_pointer(pointer).and_then(|value| value.as_bool()) == Some(true)
            };
            let host = Host::new();
            {
                let mut state = host.state.lock().unwrap();
                state.interactive_local =
                    !(fact("/nonInteractive") || fact("/remote") || fact("/thinClient"));
                state.plan_mode = fact("/planMode");
                state.setting = match facts.string_at("/setting") {
                    Some("alwaysAsk") => ModelProposedGoals::AlwaysAsk,
                    Some("disabled") => ModelProposedGoals::Disabled,
                    _ => ModelProposedGoals::Auto,
                };
                state.dialog_available = facts
                    .at_pointer("/dialogAvailable")
                    .and_then(|value| value.as_bool())
                    != Some(false);
                let gate_error = if fact("/gate/hooksRestricted") {
                    Some("restricted")
                } else if fact("/gate/hooksHeld") {
                    Some("held")
                } else if facts
                    .at_pointer("/gate/trusted")
                    .and_then(|value| value.as_bool())
                    == Some(false)
                {
                    Some("trust")
                } else {
                    None
                };
                state.evaluator_gate_error = gate_error.map(|key| {
                    fixture
                        .string_at(&format!("/gates/{key}"))
                        .unwrap()
                        .to_owned()
                });
            }
            host.pending.store(fact("/pending"), Ordering::SeqCst);
            let mut use_ctx = fresh_ctx();
            if fact("/agent") {
                use_ctx.agent_id = Some(lingxi_core::types::AgentId::new());
            }
            let result = tool(&host)
                .call(row.subprojection("/input").unwrap(), use_ctx, fresh_tx())
                .await;
            if let Some(expected) = row.string_units("/expected/error") {
                match result {
                    Err(error) => assert_eq!(
                        error.model_facing_message().string_units(""),
                        Some(expected),
                        "{}",
                        row.string_at("/name").unwrap()
                    ),
                    Ok(_) => panic!("{} should refuse", row.string_at("/name").unwrap()),
                }
                assert!(host.submissions.lock().unwrap().is_empty());
            } else {
                let result = result.unwrap();
                assert_eq!(
                    result.data.string_units("/condition"),
                    row.string_units("/expected/data/condition"),
                    "{}",
                    row.string_at("/name").unwrap()
                );
                assert_eq!(
                    result
                        .data
                        .get_property("askUser")
                        .and_then(|value| value.as_bool()),
                    row.at_pointer("/expected/data/askUser")
                        .and_then(|value| value.as_bool())
                );
                assert_eq!(host.submissions.lock().unwrap().len(), 1);
            }
        }
    }
}
