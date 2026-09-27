//! `ObserverReport` — the only way an observer agent can say anything, ported
//! from claude-code 2.1.270.
//!
//! An observer does not name a recipient. The destination comes from its
//! PAIRING, which is why every refusal below is about the pairing rather than
//! about the argument: an observer with no pairing, a pairing that has gone
//! terminal, and a target that is no longer running are three different
//! failures with three different messages, and the oracle words each one so the
//! model can tell them apart without retrying blindly.
//!
//! The name, the description, the schema's `describe`, the 1,000-char result
//! cap and all three refusals are locked byte for byte against
//! `test-harness/src/parity/fixtures/cc_2_1_270_observer_agent.json`.

use async_trait::async_trait;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use platform_api::observer_pairing::ObserverPairing;
use serde_json::{json, Value};
use std::sync::OnceLock;
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, InterruptBehavior, PromptOptions, Tool, ToolCallResult, ToolError,
    ToolStaticContext,
};

/// Oracle `se`.
pub const OBSERVER_REPORT_TOOL_NAME: &str = "ObserverReport";

/// Oracle `Je` / `Xe`. The oracle's `description()` and `prompt()` return the
/// same string; it is stored once here for the same reason.
pub const OBSERVER_REPORT_DESCRIPTION: &str = "Send a report to your report target — the agent you observe, or the coordinating agent that spawned the worker you observe. The target is resolved from your observer pairing — there is no recipient to name. Use this only when you have something genuinely useful: a mistake about to compound, a missed constraint, prior art the observed agent should see. The expected steady state is silence — if nothing warrants action, end your turn without calling this.";

/// Oracle `maxResultSizeChars`.
pub const OBSERVER_REPORT_MAX_RESULT_SIZE_CHARS: usize = 1000;

/// The main session has no pairing, so it is not an observer at all.
pub const ERR_NOT_AN_OBSERVER: &str = "ObserverReport is only available to an observer agent; the main session does not have an observed pairing.";

/// There is a pairing, but it has gone terminal.
pub const ERR_PAIRING_NOT_ARMED: &str = "Your observer pairing is not armed (stopped, retired, or never installed). The report was not delivered.";

/// The pairing is armed but the agent it reports to has gone.
#[must_use]
pub fn err_target_not_running(report_target_name: &str) -> String {
    format!(
        "The report target ({report_target_name}) is not running. The report was not delivered."
    )
}

/// Resolves whether a report target is still able to receive a report.
///
/// The oracle checks the task registry for a `running` task, or a `completed`
/// one that still has a live reader. Hosts that have no registry wired say so
/// by returning `true` — refusing here would make the tool unusable rather
/// than safe.
pub trait ReportTargetLiveness: Send + Sync {
    /// Whether `target` can still receive a report.
    fn is_running(&self, target: &protocol::AgentId) -> bool;
}

/// Delivers a report into the target agent's inbox.
#[async_trait]
pub trait ObserverReportSink: Send + Sync {
    /// Deliver `report` from `pairing`'s observer to its report target.
    async fn deliver(&self, pairing: &ObserverPairing, report: String) -> Result<(), String>;
}

/// The tool.
pub struct ObserverReportTool {
    liveness: Option<std::sync::Arc<dyn ReportTargetLiveness>>,
    sink: Option<std::sync::Arc<dyn ObserverReportSink>>,
}

impl ObserverReportTool {
    /// Build the tool with the host's liveness check and delivery sink.
    #[must_use]
    pub fn new(
        liveness: Option<std::sync::Arc<dyn ReportTargetLiveness>>,
        sink: Option<std::sync::Arc<dyn ObserverReportSink>>,
    ) -> Self {
        Self { liveness, sink }
    }

    /// Resolve the outcome of a call without performing delivery, so the
    /// decision table is testable on its own.
    fn resolve<'a>(
        &self,
        ctx: &'a ToolUseContext,
    ) -> Result<(ObserverPairing, &'a protocol::AgentId), String> {
        // Oracle: `let c = e.agentId; if (c === undefined) return …`
        let Some(agent) = ctx.agent_id.as_ref() else {
            return Err(ERR_NOT_AN_OBSERVER.to_string());
        };
        // Oracle `Wsr`: an ARMED pairing whose observer is this agent. A host
        // with no table wired lands here too, and gets the same "not armed"
        // wording — there is no pairing either way.
        let Some(table) = ctx.observer_pairings.as_ref() else {
            return Err(ERR_PAIRING_NOT_ARMED.to_string());
        };
        let Some(pairing) = table.armed_for_observer(agent) else {
            return Err(ERR_PAIRING_NOT_ARMED.to_string());
        };
        // Oracle: only a pairing WITH a report target checks liveness; a report
        // to the main session has no task to be running.
        if let Some(target) = pairing.report_target_task_id.as_ref() {
            let live = self
                .liveness
                .as_ref()
                .is_none_or(|probe| probe.is_running(target));
            if !live {
                return Err(err_target_not_running(&pairing.report_target_name));
            }
        }
        Ok((pairing, agent))
    }
}

fn schema() -> &'static Value {
    static SCHEMA: OnceLock<Value> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        json!({
            "type": "object",
            "properties": {
                "report": {
                    "type": "string",
                    "minLength": 1,
                    "description": "The report to deliver to your report target. Be concise and specific."
                }
            },
            "required": ["report"],
            "additionalProperties": false
        })
    })
}

fn failure(message: &str) -> Value {
    json!({ "success": false, "message": message })
}

#[async_trait]
impl Tool for ObserverReportTool {
    fn name(&self) -> &str {
        OBSERVER_REPORT_TOOL_NAME
    }
    fn input_schema(&self) -> &Value {
        schema()
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        // Oracle `isEnabled(){return!0}`: always registered. Reachability is
        // decided by the pairing, not by registration — an agent that is not an
        // observer gets a REASON rather than an unknown tool.
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        // Oracle `isReadOnly(){return!1}`.
        false
    }
    fn is_destructive(&self, _: &Value) -> bool {
        false
    }
    fn is_open_world(&self, _: &Value) -> bool {
        false
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        OBSERVER_REPORT_MAX_RESULT_SIZE_CHARS
    }
    fn interrupt_behavior(&self, _: &Value) -> InterruptBehavior {
        InterruptBehavior::Block
    }

    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        OBSERVER_REPORT_DESCRIPTION.to_string()
    }
    async fn prompt(&self, _: &PromptOptions) -> String {
        OBSERVER_REPORT_DESCRIPTION.to_string()
    }

    async fn check_permissions(&self, input: &Value, _: &ToolUseContext) -> PermissionResult {
        // Oracle: `async checkPermissions(m){return{behavior:"allow",
        // updatedInput:m}}`. An observer reporting to the agent that spawned it
        // crosses no boundary the pairing did not already cross.
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "ObserverReport delivers to the target its pairing already names".into(),
            },
            updated_input: Some(input.clone()),
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let (pairing, _agent) = match self.resolve(&ctx) {
            Ok(resolved) => resolved,
            Err(message) => {
                return Ok(ToolCallResult::from_data(failure(&message)));
            }
        };
        let report = input
            .get("report")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let Some(sink) = self.sink.as_ref() else {
            return Ok(ToolCallResult::from_data(failure(ERR_PAIRING_NOT_ARMED)));
        };
        match sink.deliver(&pairing, report).await {
            Ok(()) => Ok(ToolCallResult::from_data(json!({
                "success": true,
                "message": format!("Report delivered to {}.", pairing.report_target_name)
            }))),
            Err(error) => Ok(ToolCallResult::from_data(failure(&error))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_api::observer_pairing::{ObserverPairings, PairingState, MAIN_PAIRING_KEY};
    use platform_api::subagent_spawn::ObserverSpec;
    use protocol::AgentId;
    use std::sync::Arc;

    fn fixture() -> Value {
        serde_json::from_str(include_str!(
            "../../../test-harness/src/parity/fixtures/cc_2_1_270_observer_agent.json"
        ))
        .expect("observer fixture parses")
    }

    fn ctx_for(agent: Option<AgentId>, table: Option<Arc<ObserverPairings>>) -> ToolUseContext {
        let mut ctx = tool_api::test_support::fresh_ctx();
        ctx.agent_id = agent;
        ctx.observer_pairings = table;
        ctx
    }

    fn armed_table(observer: AgentId, target: Option<AgentId>) -> Arc<ObserverPairings> {
        let table = Arc::new(ObserverPairings::new());
        let mut p = platform_api::observer_pairing::ObserverPairing::armed(
            observer,
            &ObserverSpec::new("reviewer"),
            "worker-1".into(),
            "coordinator".into(),
        );
        p.report_target_task_id = target;
        table.insert(MAIN_PAIRING_KEY, p);
        table
    }

    #[test]
    fn name_description_and_result_cap_are_byte_identical_to_2_1_270() {
        let f = fixture();
        assert_eq!(OBSERVER_REPORT_TOOL_NAME, f["tool_name"].as_str().unwrap());
        assert_eq!(
            OBSERVER_REPORT_DESCRIPTION,
            f["tool_description"].as_str().unwrap()
        );
        // the oracle's `description()` and `prompt()` return the same string
        assert!(f["description_equals_prompt"].as_bool().unwrap());
        assert_eq!(
            OBSERVER_REPORT_DESCRIPTION,
            f["tool_prompt"].as_str().unwrap()
        );
        assert_eq!(
            OBSERVER_REPORT_MAX_RESULT_SIZE_CHARS as u64,
            f["max_result_size_chars"].as_u64().unwrap()
        );
    }

    #[test]
    fn the_schema_describe_is_byte_identical_to_2_1_270() {
        let f = fixture();
        assert_eq!(
            schema()["properties"]["report"]["description"]
                .as_str()
                .unwrap(),
            f["schema_report_describe"].as_str().unwrap()
        );
        // `o().min(1)` — an empty report is not a report
        assert_eq!(schema()["properties"]["report"]["minLength"], json!(1));
    }

    /// Three different failures, three different messages. The oracle words
    /// them apart so the model can tell "you are not an observer" from "your
    /// pairing died" from "the target is gone" — collapsing any two would make
    /// a retry look reasonable when it is not.
    #[test]
    fn each_refusal_is_byte_identical_and_distinct() {
        let f = fixture();
        assert_eq!(ERR_NOT_AN_OBSERVER, f["err_not_observer"].as_str().unwrap());
        assert_eq!(ERR_PAIRING_NOT_ARMED, f["err_not_armed"].as_str().unwrap());
        assert_eq!(
            err_target_not_running("coordinator"),
            f["err_target_not_running_template"]
                .as_str()
                .unwrap()
                .replace("{report_target_name}", "coordinator")
        );
        assert_ne!(ERR_NOT_AN_OBSERVER, ERR_PAIRING_NOT_ARMED);
    }

    #[test]
    fn the_main_session_is_told_it_is_not_an_observer() {
        let tool = ObserverReportTool::new(None, None);
        let err = tool.resolve(&ctx_for(None, None)).unwrap_err();
        assert_eq!(err, ERR_NOT_AN_OBSERVER);
    }

    #[test]
    fn a_terminal_pairing_refuses_with_the_not_armed_wording() {
        let observer = AgentId::new();
        let table = armed_table(observer, None);
        let tool = ObserverReportTool::new(None, None);
        assert!(tool
            .resolve(&ctx_for(Some(observer), Some(table.clone())))
            .is_ok());

        table.set_state(MAIN_PAIRING_KEY, PairingState::Stopped);
        let err = tool
            .resolve(&ctx_for(Some(observer), Some(table)))
            .unwrap_err();
        assert_eq!(err, ERR_PAIRING_NOT_ARMED);
    }

    /// A host with no pairing table gets the same "not armed" wording rather
    /// than a pretend pairing — there is no pairing either way.
    #[test]
    fn an_unwired_host_refuses_rather_than_inventing_a_pairing() {
        let tool = ObserverReportTool::new(None, None);
        let err = tool
            .resolve(&ctx_for(Some(AgentId::new()), None))
            .unwrap_err();
        assert_eq!(err, ERR_PAIRING_NOT_ARMED);
    }

    struct Dead;
    impl ReportTargetLiveness for Dead {
        fn is_running(&self, _: &protocol::AgentId) -> bool {
            false
        }
    }

    #[test]
    fn a_dead_report_target_is_named_in_the_refusal() {
        let observer = AgentId::new();
        let target = AgentId::new();
        let table = armed_table(observer, Some(target));
        let tool = ObserverReportTool::new(Some(Arc::new(Dead)), None);
        let err = tool
            .resolve(&ctx_for(Some(observer), Some(table)))
            .unwrap_err();
        assert_eq!(err, err_target_not_running("coordinator"));
    }

    /// A report to the MAIN session has no task to be running, so liveness is
    /// not consulted at all. If it were, every main-session pairing would be
    /// refused by a host whose probe says "unknown id is not running".
    #[test]
    fn a_main_session_target_does_not_consult_liveness() {
        let observer = AgentId::new();
        let table = armed_table(observer, None); // no report_target_task_id
        let tool = ObserverReportTool::new(Some(Arc::new(Dead)), None);
        assert!(tool.resolve(&ctx_for(Some(observer), Some(table))).is_ok());
    }
}
