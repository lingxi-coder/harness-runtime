//! E2E: a forced `StructuredOutput` tool call captures the model's result into
//! the shared slot and returns `endsTurn`, so the orchestrator consumes exactly
//! one model response without relying on the defensive 1-turn cap. This verifies
//! the runtime half of the structured-output mechanism that the print path drives.
#![allow(clippy::field_reassign_with_default)]

use async_trait::async_trait;
use lingxi_core::types::ToolUseId;
use llm_runtime::ContentBlock as LlmContentBlock;
use orchestrator::structured_output::{StructuredOutputSlot, StructuredOutputTool};
use orchestrator::test_support::{
    mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, ConversationOutcome, OrchestratorConfig};
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::json;
use std::sync::{Arc, Mutex};
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
};
use tool_api::ToolUseContext;

struct InspectTool {
    schema: lingxi_core::types::utf16_json::Utf16JsonProjection,
}

#[async_trait]
impl Tool for InspectTool {
    fn name(&self) -> &str {
        "Inspect"
    }

    fn input_schema(&self) -> &lingxi_core::types::utf16_json::Utf16JsonProjection {
        &self.schema
    }

    fn input_repair_metadata(&self) -> tool_api::tool_trait::ToolInputRepairMetadata {
        tool_api::tool_trait::ToolInputRepairMetadata::Known {
            schema: tool_api::tool_trait::ToolInputRepairSchema::Advertised,
            schema_provenance: tool_api::tool_trait::ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }

    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        1024
    }

    fn is_concurrency_safe(
        &self,
        _input: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) -> bool {
        true
    }

    fn is_read_only(&self, _input: &lingxi_core::types::utf16_json::Utf16JsonProjection) -> bool {
        true
    }

    async fn check_permissions(
        &self,
        _input: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        _ctx: &ToolUseContext,
    ) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "structured-output fallback test".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(
        &self,
        _input: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        _opts: &DescriptionOptions,
    ) -> String {
        "Inspect before returning the final result".into()
    }

    async fn prompt(
        &self,
        _opts: &PromptOptions,
    ) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
        String::new().into()
    }

    async fn call(
        &self,
        _input: lingxi_core::types::utf16_json::Utf16JsonProjection,
        _ctx: ToolUseContext,
        _progress_tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        Ok(ToolCallResult {
            data: tool_api::tool_trait::generated_builtin_result(&json!({"inspected": true})),
            model_content: None,
            new_messages: Vec::new(),
            context_modifier: None,
            is_error: false,
            subagent_hooks_fired: false,
            mcp_meta: None,
        })
    }
}

struct AcceptingSchema;

#[async_trait]
impl lingxi_core::host::json_schema_validator::CompiledJsonSchema for AcceptingSchema {
    async fn validate(
        &self,
        _input: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) -> Result<
        lingxi_core::host::json_schema_validator::JsonSchemaValidation,
        lingxi_core::host::json_schema_validator::SchemaWorkerError,
    > {
        Ok(lingxi_core::host::json_schema_validator::JsonSchemaValidation::Valid)
    }
}

fn structured_output_tool(schema: &str, slot: StructuredOutputSlot) -> StructuredOutputTool {
    StructuredOutputTool::new(
        lingxi_core::types::utf16_json::Utf16JsonProjection::parse(schema)
            .expect("generated JSON schema fixture"),
        Arc::new(AcceptingSchema),
        slot,
    )
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
            input: llm_runtime::services::sdk::protocol::JsonPayload::from_generated_value(
                &json!({ "answer": 4 }),
            )
            .expect("generated tool input fixture"),
        }],
        Some("tool_use"),
    );
    let api = Arc::new(MockApiClient::new(vec![r1]));
    let bus = Arc::new(telemetry::AnalyticsBus::new());
    let sink = Arc::new(telemetry::InMemorySink::new());
    bus.attach_sink(sink.clone()).await;
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(structured_output_tool(
        r#"{"type":"object","required":["answer"]}"#,
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
        Some(
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
                &json!({ "answer": 4 }),
            )
            .expect("generated expected output is representable")
        ),
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
            input: llm_runtime::services::sdk::protocol::JsonPayload::from_generated_value(&json!(
                {}
            ))
            .expect("generated tool input fixture"),
        }],
        Some("tool_use"),
    );
    let structured = mock_message_response(
        vec![LlmContentBlock::ToolCall {
            id: ToolUseId::new().to_string(),
            name: "StructuredOutput".into(),
            input: llm_runtime::services::sdk::protocol::JsonPayload::from_generated_value(
                &json!({"answer": 4}),
            )
            .expect("generated tool input fixture"),
        }],
        Some("tool_use"),
    );
    let api = Arc::new(MockApiClient::new(vec![inspect, structured]));
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(InspectTool {
        schema: tool_api::tool_trait::generated_builtin_schema(&json!({"type": "object"})),
    }));
    registry.register_builtin(Arc::new(structured_output_tool(
        r#"{"type":"object","required":["answer"]}"#,
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
    let expected = lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
        &json!({"answer": 4}),
    )
    .expect("generated expected output is representable");
    assert_eq!(slot.lock().unwrap().as_ref(), Some(&expected));
    assert_eq!(api.captured_msgs().await.len(), 2);
}
