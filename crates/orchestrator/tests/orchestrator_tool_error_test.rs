//! Tool failures reach conversation results and output events.

use async_trait::async_trait;
use llm_runtime::ContentBlock as LlmContentBlock;
use orchestrator::test_support::{
    mock_message_response, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, ConversationOutcome, OrchestratorConfig};
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use platform_api::OutputEvent;
use protocol::{ContentBlock, ConversationMessage, ToolUseId};
use serde_json::json;
use std::sync::Arc;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};

struct AlwaysFailingTool;

#[async_trait]
impl Tool for AlwaysFailingTool {
    fn name(&self) -> &str {
        "AlwaysFail"
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
        "fail".into()
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
        Err(ToolError::Internal("disk on fire".into()))
    }
}

#[tokio::test]
async fn tool_error_becomes_tool_result_with_is_error_true_and_loop_continues() {
    let tool_use_id = ToolUseId::new();
    let r1 = mock_message_response(
        vec![LlmContentBlock::ToolCall {
            id: tool_use_id.to_string(),
            name: "AlwaysFail".into(),
            input: json!({}),
        }],
        Some("tool_use"),
    );
    let r2 = mock_message_response(
        vec![LlmContentBlock::Text {
            text: "sorry, fix it later".into(),
            cache_control: None,
        }],
        Some("end_turn"),
    );
    let api = Arc::new(MockApiClient::new(vec![r1, r2]));
    let output = Arc::new(MockOutputStream::new());
    let hooks = orchestrator::test_support::noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(AlwaysFailingTool));
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
    let outcome = orch
        .run_turn("try the broken tool")
        .await
        .expect("loop succeeds despite tool failure");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    // Inspect the OutputStream — the tool_result event must carry the
    // error envelope payload.
    let events = output.snapshot().await;
    let tool_result = events
        .iter()
        .find_map(|e| match e {
            OutputEvent::ToolResult { tool, result, .. } if tool == "AlwaysFail" => {
                Some(result.clone())
            }
            _ => None,
        })
        .expect("ToolResult event present");
    assert!(
        tool_result.get("error").is_some(),
        "result envelope: {tool_result}"
    );

    // The session's third message must be the user message carrying the
    // ToolResult block with is_error = true.
    let session = orch.session();
    let session = session.lock().await;
    // history: [user(prompt), assistant(tool_use), user(tool_result), assistant(text/end_turn)]
    assert_eq!(session.history.len(), 4);
    let third = &session.history[2];
    match third {
        ConversationMessage::User { content, .. } => {
            assert_eq!(content.len(), 1);
            match &content[0] {
                ContentBlock::ToolResult {
                    tool_use_id: id,
                    content: text,
                    is_error,
                    ..
                } => {
                    assert_eq!(*id, tool_use_id);
                    assert!(text.starts_with("Error: "), "byte-locked prefix: {text}");
                    assert!(text.contains("disk on fire"), "preserves payload: {text}");
                    assert!(*is_error);
                }
                _ => panic!("expected ToolResult content block"),
            }
        }
        _ => panic!("expected User message at index 2"),
    }
}
