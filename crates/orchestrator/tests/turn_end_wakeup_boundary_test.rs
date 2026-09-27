//! Admission tests for PR 4, terminal-state family 1: lone `ScheduleWakeup` and
//! tool-requested end.
//!
//! §4's matrix keeps these rows deliberately different between the two drivers,
//! and warns against the two tidy-looking mappings that would erase them: a lone
//! wakeup must NOT become a generic `NaturalEnd` (that would give streaming a
//! Stop it does not run) and must NOT become `ReturnDirect` (that would cost it
//! the late drain and the epilogue).
//!
//! What this file pins is the part with a sharp edge — WHO CONSUMES THE FLAG.
//! `take_lone_wakeup_turn_end` swaps `loop_wakeup_armed_slot` to false as its
//! FIRST act, before it decides anything:
//!
//! ```text
//! let armed = slot.swap(false, SeqCst);
//! if !armed { return false; }
//! // …only now does it look at the tool names and the model gate
//! ```
//!
//! So the flag is consumed by CALLING the helper, not by the helper returning
//! true. Which makes the batched short-circuit load-bearing:
//!
//! ```text
//! !hook_prevent_continuation && !tool_requested_end_turn && take_lone_wakeup_turn_end(…)
//! ```
//!
//! When either earlier conjunct is false the helper never runs and the flag
//! SURVIVES into the next turn. §4: "共享 helper 不得为了清理状态而无条件 drain."
//! A shared end-handler that drains unconditionally — the obvious way to write
//! one — passes every test that only checks outcomes, and silently eats a
//! wakeup the session was supposed to keep.
//!
//! The `/loop` subsystem is what this protects: the flag surviving is how a
//! scheduled wake-up still fires after a turn that a hook stopped.

use llm_runtime::ContentBlock as LlmContentBlock;
use orchestrator::test_support::{
    mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, ConversationOutcome, OrchestratorConfig};
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use protocol::ToolUseId;
use serde_json::json;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{DescriptionOptions, PromptOptions, Tool, ToolStaticContext};
use tool_api::{ToolCallResult, ToolError, ValidationError};

fn run_with_large_stack<F, Fut>(build: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()>,
{
    let handle = std::thread::Builder::new()
        .name("turn-end-wakeup-boundary".into())
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build test runtime")
                .block_on(build());
        })
        .expect("spawn large-stack test thread");
    handle.join().expect("large-stack test thread panicked");
}

/// A tool that optionally asks the turn to end.
///
/// The request travels as the MCP `_meta` key the port reads
/// (`_meta.claude/endTurn`, classified by
/// `tool_api::tool_trait::tool_result_turn_end`; the native `result_ends_turn`
/// seam is a separate source this file does not exercise), which is the seam a
/// test can reach without a real MCP server.
struct StubTool {
    name: &'static str,
    ends_turn: bool,
}

#[async_trait::async_trait]
impl Tool for StubTool {
    fn name(&self) -> &str {
        self.name
    }
    fn input_schema(&self) -> &serde_json::Value {
        static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
            once_cell::sync::Lazy::new(|| json!({"type": "object"}));
        &SCHEMA
    }
    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024
    }
    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }
    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }
    async fn validate_input(
        &self,
        _input: &serde_json::Value,
        _ctx: &tool_api::context::ToolUseContext,
    ) -> Result<(), ValidationError> {
        Ok(())
    }
    async fn check_permissions(
        &self,
        _input: &serde_json::Value,
        _ctx: &tool_api::context::ToolUseContext,
    ) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "test".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }
    async fn description(&self, _input: &serde_json::Value, _opts: &DescriptionOptions) -> String {
        String::new()
    }
    async fn prompt(&self, _opts: &PromptOptions) -> String {
        String::new()
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: tool_api::context::ToolUseContext,
        _tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        Ok(ToolCallResult {
            data: json!({"ok": true}),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: self
                .ends_turn
                .then(|| json!({"_meta": {"claude/endTurn": true}})),
        })
    }
}

/// Every test here runs on a model the gate ACCEPTS.
///
/// `lone_wakeup_ends_turn_model` is
/// `has_capability(id, Fable5Mitigations) || normalize_model_id(id) ==
/// "claude-mythos-5"`, and the default `claude-opus-4-8` satisfies neither —
/// `platform-api`'s own `fable_5_carries_its_mitigations` pins that
/// `claude-fable-5-1` has the capability and `claude-opus-5` does not.
///
/// This matters beyond the one test that needs the turn to end: the batched
/// predicate is a conjunction, so a test running on a model the gate refuses is
/// testing the already-false side and cannot tell "not a lone round" from "not
/// an eligible model".
const GATED_MODEL: &str = "claude-fable-5-1";

fn wakeup_orch(
    responses: Vec<llm_runtime::LlmResponse>,
    tools: Vec<(&'static str, bool)>,
) -> (
    ConversationOrchestrator,
    Arc<AtomicBool>,
    Arc<MockApiClient>,
) {
    wakeup_orch_with_model(GATED_MODEL, responses, tools)
}

/// [`wakeup_orch`] on an explicit model, so the model-gate row can share this
/// wiring instead of copying the constructor (the two copies had to be kept in
/// sync by hand).
fn wakeup_orch_with_model(
    model: &str,
    responses: Vec<llm_runtime::LlmResponse>,
    tools: Vec<(&'static str, bool)>,
) -> (
    ConversationOrchestrator,
    Arc<AtomicBool>,
    Arc<MockApiClient>,
) {
    let mut registry = ToolRegistry::new();
    for (name, ends_turn) in tools {
        registry.register_builtin(Arc::new(StubTool { name, ends_turn }));
    }
    let api = Arc::new(MockApiClient::new(responses));
    let slot = Arc::new(AtomicBool::new(true)); // armed
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig {
            model: model.into(),
            ..OrchestratorConfig::default()
        },
        api.clone(),
        Arc::new(registry),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    )
    .with_loop_wakeup_armed_slot(slot.clone());
    (orch, slot, api)
}

fn tool_round(calls: &[(&str, ToolUseId)]) -> llm_runtime::LlmResponse {
    mock_message_response(
        calls
            .iter()
            .map(|(name, id)| LlmContentBlock::ToolCall {
                id: id.to_string(),
                name: (*name).into(),
                input: json!({}),
            })
            .collect(),
        Some("tool_use"),
    )
}

fn text_end(text: &str) -> llm_runtime::LlmResponse {
    mock_message_response(
        vec![LlmContentBlock::Text {
            text: text.into(),
            cache_control: None,
        }],
        Some("end_turn"),
    )
}

/// A round whose only tool was `ScheduleWakeup` ends the turn there.
///
/// The tool's own result already tells the model the harness will re-invoke it,
/// so feeding that result back buys one more model round to say so. One API
/// call, not two.
#[test]
fn a_lone_schedule_wakeup_ends_the_turn_and_consumes_the_flag() {
    run_with_large_stack(|| async {
        let (orch, slot, api) = wakeup_orch(
            vec![
                tool_round(&[("ScheduleWakeup", ToolUseId::new())]),
                text_end("should never be requested"),
            ],
            vec![("ScheduleWakeup", false)],
        );

        orch.run_turn("ping").await.expect("turn");

        assert_eq!(
            api.captured_msgs().await.len(),
            1,
            "a lone ScheduleWakeup ends the turn at that round; a second request means the \
             tool result was fed back for one more model round"
        );
        assert!(
            !slot.load(Ordering::SeqCst),
            "the wakeup flag must be consumed by the round that armed it"
        );
    });
}

/// The flag is consumed even when the verdict is "not a lone wakeup".
///
/// `take_lone_wakeup_turn_end` swaps the slot before it inspects anything, so a
/// round that armed a wakeup ALONGSIDE other tools still clears it — otherwise
/// the arming would leak into the next round and end THAT turn instead. The
/// turn itself continues, because the round was not lone.
#[test]
fn a_wakeup_beside_another_tool_still_consumes_the_flag_without_ending_the_turn() {
    run_with_large_stack(|| async {
        let (orch, slot, api) = wakeup_orch(
            vec![
                tool_round(&[
                    ("ScheduleWakeup", ToolUseId::new()),
                    ("Other", ToolUseId::new()),
                ]),
                text_end("second round"),
            ],
            vec![("ScheduleWakeup", false), ("Other", false)],
        );

        let outcome = orch.run_turn("ping").await.expect("turn");
        // `turn_count` is load-bearing here: with a no-op hook executor the
        // batched driver's only possible `Ok` variant is `EndTurn`, so
        // `matches!(.., EndTurn { .. })` alone can never fail. Pinning 2 — the
        // round that armed the wakeup plus the round that ends the turn — is
        // what lets this assertion fail.
        assert!(
            matches!(outcome, ConversationOutcome::EndTurn { turn_count: 2, .. }),
            "the turn continues past a non-lone round (two turn steps) and ends naturally, \
             got {outcome:?}"
        );
        assert_eq!(
            api.captured_msgs().await.len(),
            2,
            "a round with two tools is not a lone wakeup, so the results go back for another \
             model round"
        );
        assert!(
            !slot.load(Ordering::SeqCst),
            "the flag must still be consumed: the helper swaps it before judging the round, \
             precisely so an arming beside other tools cannot leak into the next one"
        );
    });
}

/// A tool-requested end SHORT-CIRCUITS the wakeup check, and the flag survives.
///
/// This is the row a shared end-handler is most likely to erase. The batched
/// predicate is `!hook_prevent && !tool_requested_end && take_lone_wakeup(…)`,
/// so a tool asking to end the turn means the helper is never called — and
/// since calling it is what consumes the flag, the arming stays live for the
/// next turn.
///
/// Draining unconditionally "to clean up state" would pass every
/// outcome-shaped assertion in this file and still eat a scheduled wake-up.
#[test]
fn a_tool_requested_end_leaves_the_wakeup_flag_armed_for_the_next_turn() {
    run_with_large_stack(|| async {
        let (orch, slot, api) = wakeup_orch(
            vec![
                tool_round(&[
                    ("ScheduleWakeup", ToolUseId::new()),
                    ("EndsTurn", ToolUseId::new()),
                ]),
                text_end("should never be requested"),
            ],
            vec![("ScheduleWakeup", false), ("EndsTurn", true)],
        );

        orch.run_turn("ping").await.expect("turn");

        assert_eq!(
            api.captured_msgs().await.len(),
            1,
            "a tool-requested end stops the turn at this round"
        );
        assert!(
            slot.load(Ordering::SeqCst),
            "the wakeup flag must SURVIVE. The short-circuit means take_lone_wakeup_turn_end \
             was never called, and calling it is what consumes the flag — see §4: 共享 helper \
             不得为了清理状态而无条件 drain. A handler that drains here eats a scheduled \
             wake-up and nothing about the turn's outcome would look wrong."
        );
    });
}

/// The model gate is real, and the flag is still consumed when it refuses.
///
/// `lone_wakeup_ends_turn_model` is checked LAST, after the swap. So on a model
/// outside the gate the turn continues — but the arming is gone, exactly as on
/// a model inside it. Pinning this keeps the gate from being "simplified" into
/// an early return that would also skip the consume.
#[test]
fn a_model_outside_the_gate_continues_the_turn_but_still_consumes_the_flag() {
    run_with_large_stack(|| async {
        // `claude-3-5-sonnet` carries no `Fable5Mitigations` and is not
        // `claude-mythos-5`, so `lone_wakeup_ends_turn_model` refuses it. The
        // stock default `claude-opus-4-8` would do just as well — which is why
        // the other tests here pin `GATED_MODEL` explicitly.
        let (orch, slot, api) = wakeup_orch_with_model(
            "claude-3-5-sonnet",
            vec![
                tool_round(&[("ScheduleWakeup", ToolUseId::new())]),
                text_end("second round"),
            ],
            vec![("ScheduleWakeup", false)],
        );

        orch.run_turn("ping").await.expect("turn");

        assert_eq!(
            api.captured_msgs().await.len(),
            2,
            "outside the model gate a lone wakeup does NOT end the turn"
        );
        assert!(
            !slot.load(Ordering::SeqCst),
            "the swap happens before the model gate is consulted, so the flag is consumed \
             either way"
        );
    });
}
