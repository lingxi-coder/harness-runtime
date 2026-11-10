use super::*;
use crate::test_support::{
    mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use lingxi_core::types::{ContentBlock, MessageId, SessionId, ToolUseId};
use tool_api::Tool;

struct Environment(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl Environment {
    fn v1() -> Self {
        let values = [
            ("LINGXI_ENABLE_TASKS", "off"),
            ("LINGXI_TODO_REMINDER_MODE", "baseline"),
        ];
        let old = values
            .iter()
            .map(|(key, _)| (*key, std::env::var_os(key)))
            .collect();
        for (key, value) in values {
            std::env::set_var(key, value);
        }
        Self(old)
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
    }
}

#[tokio::test]
async fn clear_first_v1_request_cannot_resurrect_the_previous_todo_marker() {
    let _lock = crate::test_support::TODO_REMINDER_ENV_LOCK.lock().await;
    let _env = Environment::v1();
    let tool = Arc::new(tool_task::TodoWriteTool::new(
        tool_api::test_support::shell_test_ctx(mobile_linux_api::ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        }),
    ));
    let mut registry = tool_api::ToolRegistry::new();
    registry.register_builtin(tool.clone());
    registry.set_main_loop_model(Some("claude-sonnet-4-5".into()));
    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![llm_runtime::ContentBlock::Text {
            text: "done".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    )]));
    let owner = ConversationOrchestrator::new(
        crate::OrchestratorConfig {
            model: "claude-sonnet-4-5".into(),
            ..Default::default()
        },
        api.clone(),
        Arc::new(registry),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    let mut ctx = tool_api::test_support::fresh_ctx();
    ctx.session = Some(owner.session());
    tool.call(
        Utf16JsonProjection::parse(r#"{"todos":[{"content":"OLD-SESSION-TODO-MARKER","status":"pending","activeForm":"Working"}]}"#).unwrap(),
        ctx, tool_api::test_support::fresh_tx(),
    ).await.unwrap();
    {
        let mut session = owner.session.lock().await;
        session
            .history
            .push(lingxi_core::types::ConversationMessage::user(
                MessageId::new(),
                "old work".into(),
            ));
        session.turns_since_last_todo_write = 10;
        session.turns_since_last_reminder = 10;
    }
    assert!(owner
        .todo_reminder_message()
        .await
        .unwrap()
        .text_content()
        .contains("OLD-SESSION-TODO-MARKER"));
    owner.session.lock().await.turns_since_last_reminder = 10;
    owner.clear_session().await.unwrap();
    {
        let session = owner.session.lock().await;
        assert!(session.todos.is_empty());
        assert_eq!(session.turns_since_last_todo_write, 0);
        assert_eq!(session.turns_since_last_reminder, 0);
    }
    owner
        .run_turn_with_cancel("new work", tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    let calls = api.captured_msgs().await;
    assert_eq!(calls.len(), 1);
    assert!(!calls[0]
        .iter()
        .any(|message| message.text_content().contains("OLD-SESSION-TODO-MARKER")));
}

#[tokio::test]
async fn hot_resume_replaces_todos_from_successful_target_history() {
    let owner = ConversationOrchestrator::new(
        crate::OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(tool_api::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    let input = Utf16JsonProjection::parse(
        r#"{"todos":[{"content":"target todo","status":"pending","activeForm":"Working"}]}"#,
    )
    .unwrap();
    let call = ToolUseId::new();
    let mut result = lingxi_core::types::ConversationMessage::user(MessageId::new(), String::new());
    if let lingxi_core::types::ConversationMessage::User { content, .. } = &mut result {
        *content = vec![ContentBlock::ToolResult {
            tool_use_id: call.clone(),
            content: "done".into(),
            is_error: Some(false),
            provider_tool_use_id: None,
            content_blocks: None,
        }];
    }
    let history = vec![
        lingxi_core::types::ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![ContentBlock::ToolUse {
                id: call,
                name: "TodoWrite".into(),
                input,
                provider_id: None,
            }],
            stop_reason: Some("tool_use".into()),
            resumable: None,
            per_turn_effort: None,
        },
        result,
    ];
    let target = SessionId::new();
    owner
        .resume_session(
            target,
            history,
            None,
            None,
            crate::test_support::code_resume_runtime_snapshot(target),
        )
        .await
        .unwrap();
    assert_eq!(
        owner.session.lock().await.todos[0].content.string_at(""),
        Some("target todo")
    );
    owner.session.lock().await.turns_since_last_reminder = 99;
    let empty_target = SessionId::new();
    owner
        .resume_session(
            empty_target,
            Vec::new(),
            None,
            None,
            crate::test_support::code_resume_runtime_snapshot(empty_target),
        )
        .await
        .unwrap();
    let session = owner.session.lock().await;
    assert!(session.todos.is_empty());
    assert_eq!(session.turns_since_last_reminder, 0);
}
