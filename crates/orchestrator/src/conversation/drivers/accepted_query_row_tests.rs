use crate::conversation::ConversationOrchestrator;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use async_trait::async_trait;
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId, ToolUseId};
use permission::PermissionDecisionReason;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct CapturedToolCall {
    input: Value,
    tool_use_id: Option<ToolUseId>,
    assistant_message_id: Option<MessageId>,
    messages: Vec<ConversationMessage>,
    assistant_message: Option<ConversationMessage>,
    same_turn_tool_uses: Vec<ContentBlock>,
}

struct ContextCaptureTool(Arc<Mutex<Vec<CapturedToolCall>>>);

#[async_trait]
impl Tool for ContextCaptureTool {
    fn name(&self) -> &str {
        "ContextCapture"
    }

    fn input_schema(&self) -> &Value {
        static SCHEMA: once_cell::sync::Lazy<Value> =
            once_cell::sync::Lazy::new(|| json!({"type":"object"}));
        &SCHEMA
    }

    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        1024
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }

    fn is_read_only(&self, _: &Value) -> bool {
        true
    }

    async fn validate_input(&self, _: &Value, _: &ToolUseContext) -> Result<(), ValidationError> {
        Ok(())
    }

    async fn check_permissions(
        &self,
        _: &Value,
        _: &ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "test".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }

    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        "capture W1 facts".into()
    }

    async fn prompt(&self, _: &PromptOptions) -> String {
        String::new()
    }

    async fn call(
        &self,
        input: Value,
        context: ToolUseContext,
        _: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        self.0.lock().unwrap().push(CapturedToolCall {
            input,
            tool_use_id: context.tool_use_id.clone(),
            assistant_message_id: context.assistant_message_id,
            messages: context.messages.clone(),
            assistant_message: context.assistant_message.clone(),
            same_turn_tool_uses: context.same_turn_tool_uses.clone(),
        });
        Ok(ToolCallResult::from_data(json!({"content":"captured"})))
    }
}

#[tokio::test]
async fn accepted_assistant_row_is_w1_current_row_but_not_prequery_history() {
    let root = tempfile::tempdir().expect("tempdir");
    let module = root.path().join("session-append.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
  on('session.append', ($, e, next) => {
    if (e.message.type !== 'assistant') return next(e);
    const tool = e.message.content.find(block => block.type === 'tool_use');
    return next({ ...e, message: { ...e.message, content: [
      { type: 'text', text: 'accepted text' },
      { ...tool, name: 'Forged', input: { forged: true } },
    ] } });
  });
}"#,
    )
    .expect("write session.append module");
    let host = hooks::mods::ModHost::start(None)
        .await
        .expect("start Mod host");
    host.load("accepted-row", root.path(), &module, json!({}))
        .await
        .expect("load session.append hook");
    let mut hook_registry = hooks::HookRegistry::new();
    hook_registry.set_mod_host(host);

    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut tool_registry = ToolRegistry::new();
    tool_registry.register_builtin(Arc::new(ContextCaptureTool(calls.clone())));
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(tool_registry),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        root.path().to_path_buf(),
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(hook_registry)));

    let tool_use_id = ToolUseId::new();
    let source_tool_use = ContentBlock::ToolUse {
        input_projection: None,
        id: tool_use_id.clone(),
        name: "ContextCapture".into(),
        input: json!({"original":true}),
        provider_id: Some("toolu_original".into()),
    };
    let source_row = ConversationMessage::Assistant { per_turn_effort: None,
        id: MessageId::new(),
        content: vec![
            ContentBlock::Text {
                text: "raw text".into(),
                citations: None,
            },
            source_tool_use.clone(),
        ],
        stop_reason: Some("tool_use".into()),
    };
    let prequery = vec![ConversationMessage::user(
        MessageId::new(),
        "physical request snapshot".into(),
    )];
    {
        let mut session = orch.session.lock().await;
        session.history.clone_from(&prequery);
        session.history.push(source_row.clone());
    }

    let accepted_row = orch
        .persist_assistant_merged(&source_row, None, None, None)
        .await;
    let accepted_content = match &accepted_row {
        ConversationMessage::Assistant { content, .. } => content.clone(),
        _ => panic!("accepted row remains assistant"),
    };
    assert_eq!(
        accepted_content,
        vec![
            ContentBlock::Text {
                text: "accepted text".into(),
                citations: Some(None),
            },
            source_tool_use.clone(),
        ],
        "q/D must keep the original ToolUse despite Text-only acceptance"
    );

    let dispatched = crate::turn_loop::dispatch_streaming_tool_use(
        &orch,
        &(
            tool_use_id.clone(),
            "ContextCapture".into(),
            json!({"original":true}),
            Some("toolu_original".into()),
        ),
        None,
        accepted_row.id(),
        crate::turn_loop::ToolUseDispatchFacts {
            query_history: prequery.clone(),
            assistant_message: accepted_row.clone(),
            same_turn_tool_uses: Vec::new(),
        },
    )
    .await
    .expect("dispatch accepted ToolUse");
    assert_eq!(dispatched.results.len(), 1);

    let captured = calls.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].input, json!({"original":true}));
    assert_eq!(captured[0].tool_use_id, Some(tool_use_id));
    assert_eq!(captured[0].assistant_message_id, Some(accepted_row.id()));
    assert_eq!(captured[0].messages, prequery);
    assert_eq!(captured[0].assistant_message, Some(accepted_row.clone()));
    assert!(captured[0].same_turn_tool_uses.is_empty());
}
