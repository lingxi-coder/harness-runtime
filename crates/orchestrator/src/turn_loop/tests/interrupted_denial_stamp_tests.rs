use super::{dispatch_tool_uses_tracked, ConversationOrchestrator};
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use async_trait::async_trait;
use protocol::ToolUseId;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};

/// A tool whose `call` returns the requested `ToolError` immediately.
struct FailingTool {
    name: &'static str,
    abort: bool,
}

#[async_trait]
impl Tool for FailingTool {
    fn name(&self) -> &str {
        self.name
    }
    fn input_schema(&self) -> &serde_json::Value {
        static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
            once_cell::sync::Lazy::new(|| json!({ "type": "object", "properties": {} }));
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
        "failing-tool".into()
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
        if self.abort {
            Err(ToolError::Aborted)
        } else {
            Err(ToolError::Internal("boom".into()))
        }
    }
}

fn orch_with(out: MockOutputStream) -> ConversationOrchestrator {
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(FailingTool {
        name: "AbortTool",
        abort: true,
    }) as Arc<dyn Tool>);
    registry.register_builtin(Arc::new(FailingTool {
        name: "BoomTool",
        abort: false,
    }) as Arc<dyn Tool>);
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(registry),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(out),
        Arc::new(StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    )
}

/// `ToolError::Aborted` from a tool's own `call` ⇒ `toolDenialKind:
/// "interrupted"`, both on the SDK frame and in the orchestrator's
/// persistence side-table.
#[tokio::test]
async fn aborted_tool_is_stamped_interrupted() {
    let out = MockOutputStream::new();
    let orch = orch_with(out.clone());
    let id = ToolUseId::new();
    let uses = vec![(id.clone(), "AbortTool".to_string(), json!({}), None)];
    let (blocks, _, _, _) = dispatch_tool_uses_tracked(&orch, &uses, None)
        .await
        .expect("dispatch");
    assert_eq!(blocks.len(), 1);

    let denials = out.denial_snapshot().await;
    assert_eq!(
        denials,
        vec![(id.clone(), "interrupted".to_string())],
        "an aborted tool must emit exactly one `interrupted` denial frame"
    );
    assert_eq!(
        orch.transcript
            .tool_denial_kinds
            .lock()
            .await
            .get(&id.to_string())
            .map(String::as_str),
        Some("interrupted"),
        "the kind must also be recorded for the persisted tool_result line"
    );
}

/// A NON-abort tool failure is an ordinary error result — no denial kind
/// (claude-code `YDd` returns `undefined` unless the error is an
/// AbortError / interrupted ShellError).
#[tokio::test]
async fn ordinary_tool_error_is_not_stamped() {
    let out = MockOutputStream::new();
    let orch = orch_with(out.clone());
    let id = ToolUseId::new();
    let uses = vec![(id.clone(), "BoomTool".to_string(), json!({}), None)];
    let _ = dispatch_tool_uses_tracked(&orch, &uses, None)
        .await
        .expect("dispatch");
    assert!(
        out.denial_snapshot().await.is_empty(),
        "a plain tool failure must not carry a toolDenialKind"
    );
    assert!(orch.transcript.tool_denial_kinds.lock().await.is_empty());
}
