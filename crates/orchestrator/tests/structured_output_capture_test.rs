//! E2E: a forced `StructuredOutput` tool call captures the model's result into
//! the shared slot and returns `endsTurn`, so the orchestrator consumes exactly
//! one model response without relying on the defensive 1-turn cap. This verifies
//! the runtime half of the structured-output mechanism that the print path drives.
#![allow(clippy::field_reassign_with_default)]

use async_trait::async_trait;
use llm_runtime::ContentBlock as LlmContentBlock;
use orchestrator::structured_output::{StructuredOutputSlot, StructuredOutputTool};
use orchestrator::test_support::{
    mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, ConversationOutcome, OrchestratorConfig};
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use protocol::ToolUseId;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
};
use tool_api::ToolUseContext;

struct InspectTool {
    schema: Value,
}

#[async_trait]
impl Tool for InspectTool {
    fn name(&self) -> &str {
        "Inspect"
    }

    fn input_schema(&self) -> &Value {
        &self.schema
    }

    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        1024
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    async fn check_permissions(&self, _input: &Value, _ctx: &ToolUseContext) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "structured-output fallback test".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _input: &Value, _opts: &DescriptionOptions) -> String {
        "Inspect before returning the final result".into()
    }

    async fn prompt(&self, _opts: &PromptOptions) -> String {
        String::new()
    }

    async fn call(
        &self,
        _input: Value,
        _ctx: ToolUseContext,
        _progress_tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        Ok(ToolCallResult {
            data: json!({"inspected": true}),
            model_content: None,
            new_messages: Vec::new(),
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

#[tokio::test]
async fn forced_structured_output_call_captures_and_does_not_loop() {
    let slot: StructuredOutputSlot = Arc::new(Mutex::new(None));
    // The model (forced via tool_choice in production) calls StructuredOutput with
    // its final result. ONLY ONE response is scripted: a clean single-call capture
    // proves the result itself ends the turn rather than requiring a follow-up.
    let r1 = mock_message_response(
        vec![LlmContentBlock::ToolCall {
            id: ToolUseId::new().to_string(),
            name: "StructuredOutput".into(),
            input: json!({ "answer": 4 }),
        }],
        Some("tool_use"),
    );
    let api = Arc::new(MockApiClient::new(vec![r1]));
    let bus = Arc::new(telemetry::AnalyticsBus::new());
    let sink = Arc::new(telemetry::InMemorySink::new());
    bus.attach_sink(sink.clone()).await;
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(StructuredOutputTool::new(
        json!({ "type": "object", "required": ["answer"] }),
        slot.clone(),
    )));

    let mut cfg = OrchestratorConfig::default();
    cfg.max_turns = 1; // structured output caps the forced loop at one model call

    let orch = ConversationOrchestrator::new(
        cfg,
        api,
        Arc::new(registry),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
    .with_analytics_bus(bus);

    let outcome = orch
        .run_turn("answer the question")
        .await
        .expect("StructuredOutput ends the turn directly");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    assert_eq!(
        slot.lock().unwrap().as_ref(),
        Some(&json!({ "answer": 4 })),
        "the forced StructuredOutput call must capture the model's result into the slot"
    );
    let events = sink.events().await;
    let ended = events
        .iter()
        .find(|event| event.name == telemetry::tengu::mcp::TOOL_RESULT_ENDED_TURN)
        .expect("tool-result end telemetry");
    assert!(matches!(
        ended.metadata.get("source"),
        Some(telemetry::AnalyticsValue::String(value)) if value == "tool"
    ));
}

#[tokio::test]
async fn prompt_only_fallback_can_retry_after_another_tool() {
    let slot: StructuredOutputSlot = Arc::new(Mutex::new(None));
    let inspect = mock_message_response(
        vec![LlmContentBlock::ToolCall {
            id: ToolUseId::new().to_string(),
            name: "Inspect".into(),
            input: json!({}),
        }],
        Some("tool_use"),
    );
    let structured = mock_message_response(
        vec![LlmContentBlock::ToolCall {
            id: ToolUseId::new().to_string(),
            name: "StructuredOutput".into(),
            input: json!({"answer": 4}),
        }],
        Some("tool_use"),
    );
    let api = Arc::new(MockApiClient::new(vec![inspect, structured]));
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(InspectTool {
        schema: json!({"type": "object"}),
    }));
    registry.register_builtin(Arc::new(StructuredOutputTool::new(
        json!({"type": "object", "required": ["answer"]}),
        slot.clone(),
    )));

    let mut cfg = OrchestratorConfig::default();
    cfg.max_turns = 1;
    let orch = ConversationOrchestrator::new(
        cfg,
        api.clone(),
        Arc::new(registry),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );

    let first = orch
        .run_turn(
            "answer the question\n\nYou must call the StructuredOutput tool exactly once with the final result. Do not call any other tool.",
        )
        .await;
    assert!(matches!(
        first,
        Err(orchestrator::OrchestratorError::MaxTurnsReached { max_turns: 1 })
    ));
    assert!(slot.lock().unwrap().is_none());

    let second = orch
        .run_turn(
            "You must call the StructuredOutput tool exactly once with the final result. Do not call any other tool.",
        )
        .await;
    assert!(matches!(second, Ok(ConversationOutcome::EndTurn { .. })));
    assert_eq!(slot.lock().unwrap().as_ref(), Some(&json!({"answer": 4})));
    assert_eq!(api.captured_msgs().await.len(), 2);
}
