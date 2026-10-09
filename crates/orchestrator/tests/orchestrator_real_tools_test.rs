//! the `ToolRegistry`.
//!
//! Uses a minimal `SimpleReadTool` rather than M4-01's `FileReadTool` so the
//! test does not have to construct a full `BuiltinToolContext` (analytics
//! bus, sandbox, etc.). The intent — exercise the orchestrator driving a
//! tool that performs real I/O — is preserved.
use async_trait::async_trait;
use lingxi_core::host::OutputEvent;
use lingxi_core::types::ToolUseId;
use llm_runtime::ContentBlock as LlmContentBlock;
use orchestrator::test_support::{
    mock_message_response, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, ConversationOutcome, OrchestratorConfig};
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::json;
use std::io::Write;
use std::sync::Arc;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};

/// Real-fs tool: reads a UTF-8 file from the path argument, returns its
/// content as a JSON `{"content": "..."}` payload. Exercises the orchestrator
/// dispatch path against an actual file I/O implementation.
struct SimpleReadTool;

#[async_trait]
impl Tool for SimpleReadTool {
    fn name(&self) -> &str {
        "Read"
    }
    fn input_schema(&self) -> &lingxi_core::types::utf16_json::Utf16JsonProjection {
        static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
            once_cell::sync::Lazy::new(|| {
                json!({
                    "type": "object",
                    "properties": { "file_path": { "type": "string" } },
                    "required": ["file_path"],
                })
            });
        static SCHEMA_SOURCE: once_cell::sync::Lazy<
            lingxi_core::types::utf16_json::Utf16JsonProjection,
        > = once_cell::sync::Lazy::new(|| tool_api::tool_trait::generated_builtin_schema(&SCHEMA));
        &SCHEMA_SOURCE
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
        1024 * 1024
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
    async fn validate_input(
        &self,
        _input: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        _ctx: &tool_api::context::ToolUseContext,
    ) -> Result<(), ValidationError> {
        Ok(())
    }
    async fn check_permissions(
        &self,
        _input: &lingxi_core::types::utf16_json::Utf16JsonProjection,
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
    async fn description(
        &self,
        _input: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        _opts: &DescriptionOptions,
    ) -> String {
        "Read a UTF-8 file".into()
    }
    async fn prompt(
        &self,
        _opts: &PromptOptions,
    ) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
        String::new().into()
    }
    async fn call(
        &self,
        input: lingxi_core::types::utf16_json::Utf16JsonProjection,
        _ctx: tool_api::context::ToolUseContext,
        _tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let path = input
            .string_at("/file_path")
            .ok_or_else(|| ToolError::InvalidInput("file_path required".into()))?;
        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| ToolError::Io(format!("read {path}: {e}").into()))?;
        Ok(ToolCallResult {
            data: tool_api::tool_trait::generated_builtin_result(&json!({ "content": content })),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            subagent_hooks_fired: false,
            mcp_meta: None,
        })
    }
}

#[tokio::test]
async fn orchestrator_drives_real_file_read_tool_on_a_tempfile() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("greeting.txt");
    {
        let mut f = std::fs::File::create(&path).expect("create");
        writeln!(f, "hello from disk").expect("write");
    }

    let tool_use_id = ToolUseId::new();
    let r1 = mock_message_response(
        vec![LlmContentBlock::ToolCall {
            id: tool_use_id.to_string(),
            name: "Read".into(),
            input: llm_runtime::services::sdk::protocol::JsonPayload::from_generated_value(
                &json!({ "file_path": path.to_string_lossy() }),
            )
            .expect("generated tool input fixture"),
        }],
        Some("tool_use"),
    );
    let r2 = mock_message_response(
        vec![LlmContentBlock::Text {
            text: "I read it".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    );
    let api = Arc::new(MockApiClient::new(vec![r1, r2]));
    let output = Arc::new(MockOutputStream::new());
    let hooks = orchestrator::test_support::noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(SimpleReadTool));
    let tools = Arc::new(registry);

    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api,
        tools,
        hooks,
        perms,
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    );

    let outcome = orch.run_turn("read greeting").await.expect("loop");
    assert!(matches!(
        outcome,
        ConversationOutcome::EndTurn { turn_count: 2, .. }
    ));

    let events = output.snapshot().await;
    let tool_result_payload = events
        .iter()
        .find_map(|e| match e {
            OutputEvent::ToolResult { tool, result, .. } if tool == "Read" => Some(result.clone()),
            _ => None,
        })
        .expect("Read ToolResult present");

    let s = serde_json::to_string(&tool_result_payload).unwrap();
    assert!(
        s.contains("hello from disk"),
        "tool payload should contain file body: {s}"
    );
}
