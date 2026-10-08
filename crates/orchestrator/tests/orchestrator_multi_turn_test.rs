//! Conversation state persists across successive turns.

use llm_runtime::ContentBlock as LlmContentBlock;

use async_trait::async_trait;
use lingxi_core::host::OutputEvent;
use lingxi_core::types::ToolUseId;
use orchestrator::test_support::{
    mock_message_response, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, ConversationOutcome, OrchestratorConfig};
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::json;
use std::sync::Arc;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};

/// Mock tool that always returns `{"ok": true}`.
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
        Ok(ToolCallResult { mcp_meta_projection: None, model_content_projection: None, data_projection: None,
            data: json!({"ok": true}),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

#[tokio::test]
async fn two_turns_with_one_tool_use_drives_loop_to_end_turn() {
    let tool_use_id = ToolUseId::new();
    let r1 = mock_message_response(
        vec![
            LlmContentBlock::Text {
                text: "let me check".into(),
                cache_control: None, citations: None,
            },
            LlmContentBlock::ToolCall { input_projection: None,
                id: tool_use_id.to_string(),
                name: "AlwaysOk".into(),
                input: json!({"x": 1}),
            },
        ],
        Some("tool_use"),
    );
    let r2 = mock_message_response(
        vec![LlmContentBlock::Text {
            text: "all good".into(),
            cache_control: None, citations: None,
        }],
        Some("end_turn"),
    );
    let api = Arc::new(MockApiClient::new(vec![r1, r2]));
    let output = Arc::new(MockOutputStream::new());
    let hooks = orchestrator::test_support::noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(AlwaysOkTool));
    let tools = Arc::new(registry);

    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        tools,
        hooks,
        perms,
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );

    let outcome = orch.run_turn("please check").await.expect("turn loop");
    match outcome {
        ConversationOutcome::EndTurn { turn_count, .. } => {
            assert_eq!(turn_count, 2, "expected 2 turns; got {turn_count}");
        }
        _ => panic!("unexpected outcome variant"),
    }

    let events = output.snapshot().await;
    // 0: Text "let me check", 1: ToolCall, 2: ToolResult, 3: Text "all good", 4: EndTurn
    assert_eq!(events.len(), 5, "events: {events:?}");
    match &events[0] {
        OutputEvent::Text { text } => assert_eq!(text, "let me check"),
        _ => panic!("event 0 expected Text"),
    }
    match &events[1] {
        OutputEvent::ToolCall { tool, .. } => assert_eq!(tool, "AlwaysOk"),
        _ => panic!("event 1 expected ToolCall"),
    }
    match &events[2] {
        OutputEvent::ToolResult { tool, result, .. } => {
            assert_eq!(tool, "AlwaysOk");
            assert_eq!(result, &json!({"ok": true}));
        }
        _ => panic!("event 2 expected ToolResult"),
    }
    match &events[3] {
        OutputEvent::Text { text } => assert_eq!(text, "all good"),
        _ => panic!("event 3 expected Text"),
    }
    match &events[4] {
        OutputEvent::EndTurn { stop_reason, .. } => assert_eq!(stop_reason, "end_turn"),
        _ => panic!("event 4 expected EndTurn"),
    }

    let captured = api.captured_msgs().await;
    assert_eq!(captured.len(), 2);
    // The 2nd call carries a leading `additionalContext` meta message (R-P1c/d,
    // prepended each turn) followed by: user(prompt), assistant(tool_use),
    // user(tool_result), then the transient total-tokens reminder.
    let second_call = &captured[1];
    assert_eq!(second_call.len(), 5);
    assert!(
        second_call[0].is_meta(),
        "first message is the leading additionalContext meta; got {:?}",
        second_call[0]
    );
    assert!(serde_json::to_string(&second_call[3])
        .unwrap()
        .contains("tool_result"));
}
