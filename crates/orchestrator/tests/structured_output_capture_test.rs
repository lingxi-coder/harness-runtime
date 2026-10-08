//! Structured output stays in one query: validation failures are tool results,
//! accepted output admits the terminal cycle, and exhaustion stops before the
//! next provider call. Native 2.1.293 captures pin these continuation semantics.

use lingxi_core::types::{ContentBlock, ConversationMessage, ToolUseId};
use llm_runtime::ContentBlock as LlmContentBlock;
use orchestrator::structured_output::{StructuredOutputSlot, StructuredOutputTool};
use orchestrator::test_support::{
    MockApiClient, MockOutputStream, MockStreamingApiClient, NoOpPermissionGate,
    StaticMemoryProvider, content_block_start_tool_use, content_block_stop, input_json_delta,
    message_delta_stop, message_start, message_stop, mock_message_response, noop_hook_executor,
};
use orchestrator::{
    ConversationOrchestrator, ConversationOutcome, OrchestratorConfig, OrchestratorError,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tool_api::registry::ToolRegistry;

fn response(input: Value) -> llm_runtime::HistoryResponse {
    mock_message_response(
        vec![LlmContentBlock::ToolCall {
            input_projection: None,
            id: ToolUseId::new().to_string(),
            name: "StructuredOutput".into(),
            input,
        }],
        Some("tool_use"),
    )
}

fn streaming_response(input: Value) -> Vec<llm_runtime::HistoryEvent> {
    vec![
        message_start(&ToolUseId::new().to_string(), "claude-opus-4-7"),
        content_block_start_tool_use(0, ToolUseId::new(), "StructuredOutput"),
        input_json_delta(0, &input.to_string()),
        content_block_stop(0),
        message_delta_stop("tool_use"),
        message_stop(),
    ]
}

fn orchestrator(api: Arc<MockApiClient>, slot: StructuredOutputSlot) -> ConversationOrchestrator {
    orchestrator_with_hooks(api, slot, noop_hook_executor(), 0, 2)
}

fn orchestrator_with_hooks(
    api: Arc<MockApiClient>,
    slot: StructuredOutputSlot,
    hooks: Arc<hooks::HookExecutorImpl>,
    max_turns: u32,
    max_structured_output_retries: i64,
) -> ConversationOrchestrator {
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(StructuredOutputTool::new(lingxi_core::types::utf16_json::Utf16JsonProjection::plain(json!({"type":"object","required":["answer"],"properties":{"answer":{"type":"string"}}})),
        slot,
    )));
    let config = OrchestratorConfig {
        max_turns,
        max_structured_output_retries,
        structured_output_enabled: true,
        ..Default::default()
    };
    ConversationOrchestrator::new(
        config,
        api,
        Arc::new(registry),
        hooks,
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
}

#[tokio::test]
async fn absent_schema_finishes_one_call_even_with_an_unrelated_registered_tool_name() {
    // Reproduce the prior activation bug with the real registry and capture
    // tool: catalog presence alone must never require a schema for this query.
    let slot: StructuredOutputSlot = Arc::new(Mutex::new(None));
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(StructuredOutputTool::new(
        lingxi_core::types::utf16_json::Utf16JsonProjection::plain(
            json!({"type":"object","additionalProperties":true}),
        ),
        slot.clone(),
    )));
    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![LlmContentBlock::Text {
            text: "HEADLESS_LOCAL_RESPONSE".into(),
            citations: None,
            cache_control: None,
        }],
        Some("end_turn"),
    )]));
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        Arc::new(registry),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    let outcome = orch.run_turn("original prompt").await.unwrap();
    assert!(matches!(
        outcome,
        ConversationOutcome::EndTurn { turn_count: 1, .. }
    ));
    assert_eq!(
        api.captured_msgs().await.len(),
        1,
        "absent schema must not inject an enforcement request"
    );
    assert!(slot.lock().unwrap().is_none());
    assert!(!orch.snapshot_history().await.iter().any(|message| {
        message
            .text_content()
            .contains("[structured-output-enforce]")
    }));
}

fn assert_native_retry_continuation(messages: &[ConversationMessage]) {
    let user_blocks: Vec<_> = messages
        .iter()
        .flat_map(|message| match message {
            ConversationMessage::User { content, .. } => content.as_slice(),
            _ => &[],
        })
        .collect();
    assert_eq!(
        user_blocks
            .iter()
            .filter(
                |block| matches!(block, ContentBlock::Text { text, .. } if text == "original prompt")
            )
            .count(),
        1,
        "retry retains the original query's user message"
    );
    assert!(
        user_blocks.iter().any(|block| matches!(block,
            ContentBlock::ToolResult { content, is_error: Some(true), .. }
            if content == "Output does not match required schema: /answer: must be string"
        )),
        "native validation error must be a tool result without an Error: prefix"
    );
    assert!(
        !messages.iter().any(|message| message
            .text_content()
            .contains("You must call the StructuredOutput tool exactly once")),
        "the retry must not inject a new corrective user prompt"
    );
}

#[tokio::test]
async fn accepted_output_finishes_one_provider_call_and_two_query_cycles() {
    for max_turns in [1, 2] {
        let slot: StructuredOutputSlot = Arc::new(Mutex::new(None));
        let api = Arc::new(MockApiClient::new(vec![response(json!({"answer":"ok"}))]));
        let orch = orchestrator_with_hooks(
            api.clone(),
            slot.clone(),
            noop_hook_executor(),
            max_turns,
            2,
        );
        let outcome = orch.run_turn("original prompt").await.unwrap();
        assert!(matches!(
            outcome,
            ConversationOutcome::EndTurn { turn_count: 2, .. }
        ));
        assert_eq!(api.captured_msgs().await.len(), 1);
        assert_eq!(
            slot.lock()
                .unwrap()
                .as_ref()
                .map(|projection| &projection.value),
            Some(&json!({"answer":"ok"}))
        );
        assert_eq!(orch.completed_turn_metrics().unwrap().num_turns, 2);
    }
}

#[tokio::test]
async fn invalid_then_valid_output_continues_the_same_query() {
    let slot: StructuredOutputSlot = Arc::new(Mutex::new(None));
    let api = Arc::new(MockApiClient::new(vec![
        response(json!({"answer":42})),
        response(json!({"answer":"ok"})),
    ]));
    let orch = orchestrator(api.clone(), slot.clone());
    let outcome = orch.run_turn("original prompt").await.unwrap();
    assert!(matches!(
        outcome,
        ConversationOutcome::EndTurn { turn_count: 3, .. }
    ));
    let requests = api.captured_msgs().await;
    assert_eq!(requests.len(), 2);
    assert_native_retry_continuation(&requests[1]);
    assert_eq!(
        slot.lock()
            .unwrap()
            .as_ref()
            .map(|projection| &projection.value),
        Some(&json!({"answer":"ok"}))
    );
}

#[tokio::test]
async fn exhausted_output_admits_terminal_cycle_without_a_third_provider_call() {
    let slot: StructuredOutputSlot = Arc::new(Mutex::new(None));
    let api = Arc::new(MockApiClient::new(vec![
        response(json!({"answer":42})),
        response(json!({"answer":43})),
    ]));
    let orch = orchestrator(api.clone(), slot.clone());
    let error = orch.run_turn("original prompt").await.unwrap_err();
    assert!(matches!(
        error,
        OrchestratorError::MaxStructuredOutputRetries { max_retries: 2, .. }
    ));
    assert_eq!(
        error.to_string(),
        "Failed to provide valid structured output after 2 attempts — last StructuredOutput error: Output does not match required schema: /answer: must be string"
    );
    let requests = api.captured_msgs().await;
    assert_eq!(requests.len(), 2);
    assert_native_retry_continuation(&requests[1]);
    assert!(slot.lock().unwrap().is_none());
    assert_eq!(orch.completed_turn_metrics().unwrap().num_turns, 3);
}

#[tokio::test]
async fn streaming_retry_preserves_the_query_for_success_and_exhaustion() {
    for valid_second in [true, false] {
        let slot: StructuredOutputSlot = Arc::new(Mutex::new(None));
        let second = if valid_second {
            json!({"answer":"ok"})
        } else {
            json!({"answer":43})
        };
        let api = Arc::new(MockStreamingApiClient::with_turns(vec![
            streaming_response(json!({"answer":42})),
            streaming_response(second),
        ]));
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(StructuredOutputTool::new(lingxi_core::types::utf16_json::Utf16JsonProjection::plain(json!({"type":"object","required":["answer"],"properties":{"answer":{"type":"string"}}})),
            slot.clone(),
        )));
        let orch =
            ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
                OrchestratorConfig {
                    max_turns: 0,
                    max_structured_output_retries: 2,
                    structured_output_enabled: true,
                    ..Default::default()
                },
                Arc::new(MockApiClient::new(Vec::new())),
                api.clone(),
                Arc::new(registry),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                std::env::temp_dir(),
            ));
        let outcome = orch.run_turn_streaming("original prompt").await;
        if valid_second {
            assert!(matches!(
                outcome.unwrap(),
                ConversationOutcome::EndTurn { turn_count: 3, .. }
            ));
            assert!(slot.lock().unwrap().is_some());
        } else {
            assert!(matches!(
                outcome.unwrap_err(),
                OrchestratorError::MaxStructuredOutputRetries { max_retries: 2, .. }
            ));
            assert!(slot.lock().unwrap().is_none());
        }
        let calls = api.captured_calls().await;
        assert_eq!(calls.len(), 2);
        assert_native_retry_continuation(&calls[1].messages);
        assert_eq!(orch.completed_turn_metrics().unwrap().num_turns, 3);
    }
}

struct CountPromptSubmit(Arc<std::sync::atomic::AtomicUsize>);

#[async_trait::async_trait]
impl hooks::executor::BuiltinHookHandler for CountPromptSubmit {
    fn id(&self) -> &str {
        "count-prompt-submit"
    }

    async fn handle(
        &self,
        event: &hooks::events::HookEvent,
        _ctx: &hooks::registry::HookContext,
    ) -> hooks::response::HookResult {
        if matches!(event, hooks::events::HookEvent::UserPromptSubmit { .. }) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        hooks::response::HookResult {
            outcome: hooks::response::HookOutcome::Success,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            response: None,
        }
    }
}

struct UnusedHookHost;

#[async_trait::async_trait]
impl lingxi_core::host::HttpTransport for UnusedHookHost {
    async fn request(
        &self,
        _req: lingxi_core::types::HttpRequest,
    ) -> Result<lingxi_core::types::HttpResponse, lingxi_core::host::HttpError> {
        Err(lingxi_core::host::HttpError::InvalidRequest(
            "builtin hook has no HTTP request".into(),
        ))
    }
    async fn stream_sse(
        &self,
        _req: lingxi_core::types::HttpRequest,
    ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
        Err(lingxi_core::host::HttpError::InvalidRequest(
            "builtin hook has no HTTP stream".into(),
        ))
    }
}

#[async_trait::async_trait]
impl lingxi_core::host::RuntimeSpawner for UnusedHookHost {
    async fn spawn(
        &self,
        _name: &str,
        _task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
    ) -> Result<lingxi_core::host::BackgroundTaskHandle, lingxi_core::host::RuntimeError> {
        Err(lingxi_core::host::RuntimeError::Internal(
            "builtin hook has no background task".into(),
        ))
    }
    async fn sleep(&self, _duration: std::time::Duration) {}
    async fn cancel(
        &self,
        _handle: &lingxi_core::host::BackgroundTaskHandle,
    ) -> Result<(), lingxi_core::host::RuntimeError> {
        Ok(())
    }
}

#[tokio::test]
async fn missing_tool_gets_one_meta_reminder_without_resubmitting_the_prompt() {
    let submit_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let registry = Arc::new(tokio::sync::RwLock::new(
        hooks::registry::HookRegistry::new(),
    ));
    registry
        .write()
        .await
        .register(hooks::definition::HookDefinition {
            id: lingxi_core::types::HookId::new(),
            name: "count-prompt-submit".into(),
            events: vec![hooks::events::HookEventType::UserPromptSubmit],
            if_condition: None,
            executor: hooks::definition::HookExecutor::Builtin {
                handler_id: "count-prompt-submit".into(),
            },
            source: hooks::definition::HookSource::Settings(
                lingxi_core::types::SettingsScope::User,
            ),
            blocking: true,
            timeout: None,
            priority: 0,
            once: false,
            status_message: None,
            async_rewake: false,
            async_timeout: None,
            rewake_message: None,
        });
    let mut hooks =
        hooks::HookExecutorImpl::new(registry, Arc::new(UnusedHookHost), Arc::new(UnusedHookHost));
    hooks.register_builtin(Arc::new(CountPromptSubmit(submit_count.clone())));
    let text_response = || {
        mock_message_response(
            vec![LlmContentBlock::Text {
                text: "HEADLESS_SCHEMA_WITHOUT_TOOL".into(),
                cache_control: None,
                citations: None,
            }],
            Some("end_turn"),
        )
    };
    let api = Arc::new(MockApiClient::new(vec![text_response(), text_response()]));
    let slot: StructuredOutputSlot = Arc::new(Mutex::new(None));
    let orch = orchestrator_with_hooks(api.clone(), slot.clone(), Arc::new(hooks), 0, 2);
    assert!(matches!(
        orch.run_turn("original prompt").await.unwrap(),
        ConversationOutcome::EndTurn { turn_count: 2, .. }
    ));
    let requests = api.captured_msgs().await;
    assert_eq!(requests.len(), 2);
    let reminder = "[structured-output-enforce] You MUST call the StructuredOutput tool to complete this request. Call this tool now.";
    assert_eq!(
        requests[1]
            .iter()
            .filter(|message| message.text_content() == reminder && message.is_meta())
            .count(),
        1
    );
    assert_eq!(
        requests[1]
            .iter()
            .filter(
                |message| matches!(message, ConversationMessage::User { is_meta: false, .. })
                    && message.text_content() == "original prompt"
            )
            .count(),
        1
    );
    assert_eq!(submit_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(slot.lock().unwrap().is_none());
}

#[tokio::test]
async fn zero_and_negative_limits_stop_after_the_first_invalid_tool_call() {
    for limit in [0, -1] {
        let slot: StructuredOutputSlot = Arc::new(Mutex::new(None));
        let api = Arc::new(MockApiClient::new(vec![response(json!({"answer":42}))]));
        let orch = orchestrator_with_hooks(api.clone(), slot, noop_hook_executor(), 0, limit);
        let error = orch.run_turn("original prompt").await.unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "Failed to provide valid structured output after {limit} attempts — last StructuredOutput error: Output does not match required schema: /answer: must be string"
            )
        );
        assert_eq!(api.captured_msgs().await.len(), 1);
        assert_eq!(orch.completed_turn_metrics().unwrap().num_turns, 2);
    }
}
