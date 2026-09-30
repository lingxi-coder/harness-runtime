//! Streaming conversation state persists across successive turns.

use async_trait::async_trait;
use orchestrator::test_support::{
    content_block_start_text, content_block_start_tool_use, content_block_stop, input_json_delta,
    message_delta_stop, message_start, message_stop, text_delta, MockApiClient, MockOutputStream,
    MockStreamingApiClient, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{scripted, ConversationOrchestrator, ConversationOutcome, OrchestratorConfig};
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use platform_api::OutputEvent;
use protocol::ToolUseId;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use telemetry::InMemorySink;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};

struct AlwaysOkTool;

#[async_trait]
impl Tool for AlwaysOkTool {
    fn name(&self) -> &str {
        "AlwaysOk"
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
        "AlwaysOk".into()
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
            mcp_meta: None,
        })
    }
}

struct McpEndTurnTool;

#[async_trait]
impl Tool for McpEndTurnTool {
    fn name(&self) -> &str {
        "McpEndTurn"
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
        "McpEndTurn".into()
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
            data: json!([{ "type": "text", "text": "ended" }]),
            model_content: Some("ended".into()),
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: Some(json!({ "_meta": { "claude/endTurn": true } })),
        })
    }
}

fn registry_with_always_ok() -> Arc<ToolRegistry> {
    let mut r = ToolRegistry::new();
    r.register_builtin(Arc::new(AlwaysOkTool));
    Arc::new(r)
}

fn registry_with_mcp_end_turn() -> Arc<ToolRegistry> {
    let mut r = ToolRegistry::new();
    r.register_builtin(Arc::new(McpEndTurnTool));
    Arc::new(r)
}

#[tokio::test]
async fn two_streaming_turns_with_tool_in_between() {
    let tu = ToolUseId::new();
    let turn1 = scripted![
        message_start("m1", "claude-opus-4-7"),
        content_block_start_tool_use(0, tu.clone(), "AlwaysOk"),
        input_json_delta(0, "{}"),
        content_block_stop(0),
        message_delta_stop("tool_use"),
        message_stop(),
    ];
    let turn2 = scripted![
        message_start("m2", "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, "done"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ];

    let api = Arc::new(MockStreamingApiClient::with_turns(vec![turn1, turn2]));
    let batched = Arc::new(MockApiClient::new(Vec::new()));
    let output = Arc::new(MockOutputStream::new());
    let orch = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        batched,
        api.clone(),
        registry_with_always_ok(),
        orchestrator::test_support::noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    );

    let outcome = orch.run_turn_streaming("call then echo").await.expect("ok");
    match outcome {
        ConversationOutcome::EndTurn { turn_count, .. } => {
            assert_eq!(turn_count, 2);
        }
        _ => panic!("unexpected outcome variant"),
    }

    assert_eq!(api.captured_calls().await.len(), 2);

    let events = output.snapshot().await;
    // The §0.7 "light up thinking/usage" follow-up adds additive `Usage`
    // emits (from `message_start` / `message_delta`) that interleave but
    // are orthogonal to the tool/text ordering this test asserts, so
    // filter them (and `Thinking` and message identity metadata) out before the sequence assertion.
    let kinds: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            OutputEvent::Text { .. } => Some("Text"),
            OutputEvent::ToolCall { .. } => Some("ToolCall"),
            OutputEvent::ToolResult { .. } => Some("ToolResult"),
            OutputEvent::EndTurn { .. } => Some("EndTurn"),
            OutputEvent::Usage { .. }
            | OutputEvent::Thinking { .. }
            | OutputEvent::MessageIdentity { .. } => None,
            _ => Some("Other"),
        })
        .collect();
    assert_eq!(
        kinds,
        vec!["ToolCall", "ToolResult", "Text", "EndTurn"],
        "events: {events:?}"
    );
}

#[tokio::test]
async fn streaming_mcp_end_turn_stops_without_a_second_model_call() {
    let tu = ToolUseId::new();
    let turn = scripted![
        message_start("m1", "claude-opus-4-7"),
        content_block_start_tool_use(0, tu.clone(), "McpEndTurn"),
        input_json_delta(0, "{}"),
        content_block_stop(0),
        message_delta_stop("tool_use"),
        message_stop(),
    ];

    let api = Arc::new(MockStreamingApiClient::with_turns(vec![turn]));
    let batched = Arc::new(MockApiClient::new(Vec::new()));
    let output = Arc::new(MockOutputStream::new());
    let bus = Arc::new(telemetry::AnalyticsBus::new());
    let sink = Arc::new(InMemorySink::new());
    bus.attach_sink(sink.clone()).await;
    let orch = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        batched,
        api.clone(),
        registry_with_mcp_end_turn(),
        orchestrator::test_support::noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    )
    .with_analytics_bus(bus);

    let outcome = orch.run_turn_streaming("call and stop").await.expect("ok");
    match outcome {
        ConversationOutcome::EndTurn { turn_count, .. } => assert_eq!(turn_count, 1),
        _ => panic!("unexpected outcome variant"),
    }

    assert_eq!(
        api.captured_calls().await.len(),
        1,
        "no follow-up stream call"
    );
    let events = sink.events().await;
    let event = events
        .iter()
        .find(|event| event.name == telemetry::tengu::mcp::TOOL_RESULT_ENDED_TURN)
        .expect("MCP end-turn telemetry emitted");
    assert!(matches!(
        event.metadata.get("source"),
        Some(telemetry::AnalyticsValue::String(value)) if value == "mcp_meta"
    ));

    let kinds: Vec<&str> = output
        .snapshot()
        .await
        .iter()
        .filter_map(|e| match e {
            OutputEvent::ToolCall { .. } => Some("ToolCall"),
            OutputEvent::ToolResult { .. } => Some("ToolResult"),
            OutputEvent::EndTurn { .. } => Some("EndTurn"),
            OutputEvent::Usage { .. }
            | OutputEvent::Thinking { .. }
            | OutputEvent::MessageIdentity { .. } => None,
            _ => Some("Other"),
        })
        .collect();
    assert_eq!(kinds, vec!["ToolCall", "ToolResult", "EndTurn"]);
}
