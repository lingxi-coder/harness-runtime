use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::ConversationOrchestrator;
use crate::OrchestratorConfig;
use protocol::MessageId;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};

static ENV_LOCK: StdMutex<()> = StdMutex::new(());

struct DeferTool {
    name: &'static str,
    defer: bool,
}
#[async_trait]
impl Tool for DeferTool {
    fn name(&self) -> &str {
        self.name
    }
    fn should_defer(&self) -> bool {
        self.defer
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
            reason: permission::PermissionDecisionReason::Other { reason: "t".into() },
            updated_input: None,
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
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
        _ctx: ToolUseContext,
        _tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        Ok(ToolCallResult {
            data: serde_json::json!({}),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

fn registry(deferral_enabled: bool) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.register_builtin(Arc::new(DeferTool {
        name: "ToolSearch",
        defer: false,
    }));
    for name in ["Zeta", "Alpha"] {
        reg.register_builtin(Arc::new(DeferTool { name, defer: true }));
    }
    if deferral_enabled {
        reg.set_deferral(Arc::new(tool_api::defer::DeferralState::new(
            tool_api::defer::ToolSearchMode::Enabled,
            false,
        )));
    }
    reg.refresh_tool_search_view();
    reg
}

async fn orch(reg: ToolRegistry, assistant_turns: usize) -> ConversationOrchestrator {
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(reg),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::with_files(vec![])),
        PathBuf::from("/work/repo"),
    );
    {
        let mut s = orch.session.lock().await;
        for _ in 0..assistant_turns {
            s.history.push(protocol::ConversationMessage::Assistant {
                id: MessageId::new(),
                content: vec![protocol::ContentBlock::Text { text: "ok".into() }],
                stop_reason: None,
            });
        }
    }
    orch
}

#[tokio::test]
async fn the_gate_is_off_by_default() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("LINGXI_TOOL_SEARCH_REMINDER");
    let orch = orch(registry(true), 40).await;
    assert!(
        orch.tool_search_usage_reminder_message(false)
            .await
            .is_none(),
        "`Lda()` is null in a stock install ⇒ `Uzm` returns [] immediately"
    );
}

#[tokio::test]
async fn with_the_gate_on_it_lists_the_undiscovered_tools_sorted() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("LINGXI_TOOL_SEARCH_REMINDER", "1");
    let mode = Arc::new(ReminderCoordinatorMode(std::sync::atomic::AtomicBool::new(
        true,
    )));
    let orch = orch(registry(true), 40)
        .await
        .with_coordinator_mode(mode.clone())
        .with_coordinator_simple_mode_for_test(false);
    assert!(orch.tools.find_by_name("ToolSearch").is_some());
    assert!(orch
        .tool_search_usage_reminder_message(false)
        .await
        .is_none());
    mode.0.store(false, std::sync::atomic::Ordering::SeqCst);
    let msg = orch.tool_search_usage_reminder_message(false).await;
    std::env::remove_var("LINGXI_TOOL_SEARCH_REMINDER");
    let text = msg.expect("40 turns > everyNTurns=15").text_content();
    assert!(text.starts_with("<system-reminder>\n"), "got: {text}");
    assert!(
        text.contains("not loaded in this conversation yet: Alpha, Zeta."),
        "sorted, no remainder; got: {text}"
    );
    // The mark was recorded ⇒ the next call is inside the interval.
    assert!(orch
        .tool_search_usage_reminder_message(false)
        .await
        .is_none());
}

#[tokio::test]
async fn it_never_fires_alongside_a_todo_reminder_or_with_deferral_off() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("LINGXI_TOOL_SEARCH_REMINDER", "1");

    let with_todo = orch(registry(true), 40).await;
    let a = with_todo.tool_search_usage_reminder_message(true).await;

    // `mBr()!=="tst"` — deferral off ⇒ mode is `Standard`.
    let no_defer = orch(registry(false), 40).await;
    let b = no_defer.tool_search_usage_reminder_message(false).await;

    // Not enough assistant turns yet.
    let too_soon = orch(registry(true), 3).await;
    let c = too_soon.tool_search_usage_reminder_message(false).await;

    std::env::remove_var("LINGXI_TOOL_SEARCH_REMINDER");
    assert!(a.is_none(), "task_reminder_same_turn");
    assert!(b.is_none(), "mode_not_tst");
    assert!(c.is_none(), "turnsSinceLastToolSearch < everyNTurns");
}

struct ReminderCoordinatorMode(std::sync::atomic::AtomicBool);

impl platform_api::coordinator_mode::CoordinatorModeHandle for ReminderCoordinatorMode {
    fn is_enabled(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}
