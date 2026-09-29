use super::*;
use crate::test_support::{
    content_block_start_text, content_block_start_tool_use, content_block_stop, input_json_delta,
    message_delta_stop, message_start, message_stop, mock_message_response, noop_hook_executor,
    text_delta, MockApiClient, MockOutputStream, MockStreamingApiClient, NoOpPermissionGate,
    StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use lingxi_core::types::ToolUseId;
use llm_runtime::ContentBlock as LlmContentBlock;
use std::sync::Arc;
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    ContextModifier, DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError,
    ToolStaticContext, ValidationError,
};

/// The model the [`ModelSwitchTool`] switches the session to.
const SWITCHED_MODEL: &str = "claude-opus-4-9-zzz";

/// A tool that succeeds AND returns a `context_modifier` setting the turn's
/// `main_loop_model` to [`SWITCHED_MODEL`] — the orchestrator-side twin of a
/// Skill tool with a `model:` frontmatter. Sets the model directly (the
/// skill-specific `[1m]`-resolution logic is unit-tested in the skill crate).
struct ModelSwitchTool;
#[async_trait]
impl Tool for ModelSwitchTool {
    fn name(&self) -> &str {
        "ModelSwitch"
    }
    fn input_schema(&self) -> &serde_json::Value {
        static SCHEMA: once_cell::sync::Lazy<serde_json::Value> = once_cell::sync::Lazy::new(
            || serde_json::json!({ "type": "object", "properties": {} }),
        );
        &SCHEMA
    }
    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024 * 1024
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
        _ctx: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        Ok(())
    }
    async fn check_permissions(
        &self,
        _input: &serde_json::Value,
        _ctx: &ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "test".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }
    async fn description(&self, _input: &serde_json::Value, _opts: &DescriptionOptions) -> String {
        "model-switch".into()
    }
    async fn prompt(&self, _opts: &PromptOptions) -> String {
        String::new()
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: ToolUseContext,
        _tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let modifier: ContextModifier = Box::new(|mut ctx: ToolUseContext| {
            ctx.options.main_loop_model = SWITCHED_MODEL.to_string();
            ctx
        });
        Ok(ToolCallResult {
            data: serde_json::json!({
                "content": "TOOL-RESULT",
                "model_content": "Launching skill: switcher",
            }),
            model_content: None,
            new_messages: vec![],
            context_modifier: Some(modifier),
            is_error: false,
            mcp_meta: None,
        })
    }
}

/// A tool with NO `context_modifier` (the byte-identical baseline — like
/// every existing tool).
struct PlainTool;
#[async_trait]
impl Tool for PlainTool {
    fn name(&self) -> &str {
        "Plain"
    }
    fn input_schema(&self) -> &serde_json::Value {
        static SCHEMA: once_cell::sync::Lazy<serde_json::Value> = once_cell::sync::Lazy::new(
            || serde_json::json!({ "type": "object", "properties": {} }),
        );
        &SCHEMA
    }
    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024 * 1024
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
        _ctx: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        Ok(())
    }
    async fn check_permissions(
        &self,
        _input: &serde_json::Value,
        _ctx: &ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "test".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }
    async fn description(&self, _input: &serde_json::Value, _opts: &DescriptionOptions) -> String {
        "plain".into()
    }
    async fn prompt(&self, _opts: &PromptOptions) -> String {
        String::new()
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: ToolUseContext,
        _tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        Ok(ToolCallResult {
            data: serde_json::json!({ "content": "PLAIN-RESULT" }),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

fn registry_with(tool: Arc<dyn Tool>) -> Arc<ToolRegistry> {
    let mut reg = ToolRegistry::new();
    reg.register_builtin(tool);
    Arc::new(reg)
}

// ----- batched driver (`run_turn`) -----

#[tokio::test]
async fn batched_skill_model_override_switches_session_model() {
    let tu = ToolUseId::new();
    let resp1 = mock_message_response(
        vec![LlmContentBlock::ToolCall {
            id: tu.to_string(),
            name: "ModelSwitch".into(),
            input: serde_json::json!({}),
        }],
        Some("tool_use"),
    );
    let resp2 = mock_message_response(
        vec![LlmContentBlock::Text {
            text: "done".into(),
            cache_control: None,
        }],
        Some("end_turn"),
    );
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![resp1, resp2])),
        registry_with(Arc::new(ModelSwitchTool)),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    // Precondition: the session boots on the default model.
    assert_eq!(
        orch.session.lock().await.model,
        crate::config::DEFAULT_MODEL
    );

    orch.run_turn("switch please").await.expect("turn");

    // POST-BATCH the override took effect; the NEXT turn's API call reads it.
    assert_eq!(orch.session.lock().await.model, SWITCHED_MODEL);
}

#[tokio::test]
async fn batched_no_modifier_leaves_session_model_untouched() {
    // Byte-identical guard: a tool with NO context_modifier must not move
    // `session.model`.
    let tu = ToolUseId::new();
    let resp1 = mock_message_response(
        vec![LlmContentBlock::ToolCall {
            id: tu.to_string(),
            name: "Plain".into(),
            input: serde_json::json!({}),
        }],
        Some("tool_use"),
    );
    let resp2 = mock_message_response(
        vec![LlmContentBlock::Text {
            text: "done".into(),
            cache_control: None,
        }],
        Some("end_turn"),
    );
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![resp1, resp2])),
        registry_with(Arc::new(PlainTool)),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    orch.run_turn("no switch").await.expect("turn");
    assert_eq!(
        orch.session.lock().await.model,
        crate::config::DEFAULT_MODEL,
        "no context_modifier → session.model unchanged (byte-identical)"
    );
}

// ----- streaming driver (`run_turn_streaming`) -----

#[tokio::test]
async fn streaming_skill_model_override_switches_session_and_next_call() {
    let tu = ToolUseId::new();
    let turn1 = vec![
        message_start("m1", crate::config::DEFAULT_MODEL),
        content_block_start_tool_use(0, tu.clone(), "ModelSwitch"),
        input_json_delta(0, "{}"),
        content_block_stop(0),
        message_delta_stop("tool_use"),
        message_stop(),
    ];
    let turn2 = vec![
        message_start("m2", SWITCHED_MODEL),
        content_block_start_text(0),
        text_delta(0, "done"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ];
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![turn1, turn2]));
    let orch = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        streaming.clone(),
        registry_with(Arc::new(ModelSwitchTool)),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );

    orch.run_turn_streaming("switch please")
        .await
        .expect("streaming turn");

    // session.model switched POST-BATCH...
    assert_eq!(orch.session.lock().await.model, SWITCHED_MODEL);
    // ...and the NEXT (second) streaming call used the switched model, while
    // the first used the boot default.
    let calls = streaming.captured_calls().await;
    assert_eq!(calls.len(), 2, "two streaming calls (tool turn + end turn)");
    assert_eq!(calls[0].model, crate::config::DEFAULT_MODEL);
    assert_eq!(
        calls[1].model, SWITCHED_MODEL,
        "the NEXT API call must use the switched model"
    );
}

#[tokio::test]
async fn streaming_no_modifier_leaves_session_model_untouched() {
    // Byte-identical guard on the streaming path.
    let tu = ToolUseId::new();
    let turn1 = vec![
        message_start("m1", crate::config::DEFAULT_MODEL),
        content_block_start_tool_use(0, tu.clone(), "Plain"),
        input_json_delta(0, "{}"),
        content_block_stop(0),
        message_delta_stop("tool_use"),
        message_stop(),
    ];
    let turn2 = vec![
        message_start("m2", crate::config::DEFAULT_MODEL),
        content_block_start_text(0),
        text_delta(0, "done"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ];
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![turn1, turn2]));
    let orch = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        streaming.clone(),
        registry_with(Arc::new(PlainTool)),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    orch.run_turn_streaming("no switch")
        .await
        .expect("streaming turn");
    assert_eq!(
        orch.session.lock().await.model,
        crate::config::DEFAULT_MODEL,
        "no context_modifier → session.model unchanged on the streaming path"
    );
    let calls = streaming.captured_calls().await;
    assert!(
        calls
            .iter()
            .all(|c| c.model == crate::config::DEFAULT_MODEL),
        "every streaming call used the unchanged default model"
    );
}

/// Regression guard for the streaming-profile gap: when `session.model_profile`
/// is set (e.g. `"github-copilot"`) the INITIAL streaming `.stream()` call
/// must carry the profile, not `None`.  Mirrors the batched
/// `build_request_sets_profile_when_provided` test in `provider_adapter.rs`.
#[tokio::test]
async fn streaming_threads_model_profile_to_stream_call() {
    let turn = vec![
        message_start("m1", crate::config::DEFAULT_MODEL),
        content_block_start_text(0),
        text_delta(0, "hello"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ];
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![turn]));
    let orch = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        streaming.clone(),
        registry_with(Arc::new(PlainTool)),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    // Set model_profile on the session directly (mirrors what switch_model does).
    {
        let mut s = orch.session.lock().await;
        s.model_profile = Some("github-copilot".to_string());
    }

    orch.run_turn_streaming("hello")
        .await
        .expect("streaming turn");

    let calls = streaming.captured_calls().await;
    assert_eq!(calls.len(), 1, "one streaming call");
    assert_eq!(
        calls[0].profile.as_deref(),
        Some("github-copilot"),
        "streaming path must thread session.model_profile through to the stream() call"
    );
}
