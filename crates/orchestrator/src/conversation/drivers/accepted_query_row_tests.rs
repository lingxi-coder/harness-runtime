use crate::conversation::ConversationOrchestrator;
use crate::test_support::{
    mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use async_trait::async_trait;
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId, ToolUseId};
use permission::PermissionDecisionReason;
use serde_json::json;
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
    input: lingxi_core::types::utf16_json::Utf16JsonProjection,
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

    fn input_schema(&self) -> &lingxi_core::types::utf16_json::Utf16JsonProjection {
        static SCHEMA: once_cell::sync::Lazy<lingxi_core::types::utf16_json::Utf16JsonProjection> =
            once_cell::sync::Lazy::new(|| {
                tool_api::tool_trait::generated_builtin_schema(&json!({"type":"object"}))
            });
        &SCHEMA
    }

    fn input_repair_metadata(&self) -> tool_api::tool_trait::ToolInputRepairMetadata {
        use tool_api::tool_trait::{
            ToolInputRepairMetadata, ToolInputRepairSchema, ToolInputSchemaProvenance,
        };

        ToolInputRepairMetadata::Known {
            schema: ToolInputRepairSchema::Advertised,
            schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }

    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        1024
    }

    fn is_concurrency_safe(&self, _: &lingxi_core::types::utf16_json::Utf16JsonProjection) -> bool {
        true
    }

    fn is_read_only(&self, _: &lingxi_core::types::utf16_json::Utf16JsonProjection) -> bool {
        true
    }

    async fn validate_input(
        &self,
        _: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        Ok(())
    }

    async fn check_permissions(
        &self,
        _: &lingxi_core::types::utf16_json::Utf16JsonProjection,
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

    async fn description(
        &self,
        _: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        _: &DescriptionOptions,
    ) -> String {
        "capture W1 facts".into()
    }

    async fn prompt(
        &self,
        _: &PromptOptions,
    ) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
        String::new().into()
    }

    async fn call(
        &self,
        input: lingxi_core::types::utf16_json::Utf16JsonProjection,
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
        Ok(ToolCallResult::from_data(
            tool_api::tool_trait::generated_builtin_result(&json!({"content":"captured"})),
        ))
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
    host.load(
        "accepted-row",
        root.path(),
        &module,
        lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(&json!({}))
            .expect("generated hook options are representable"),
    )
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
        id: tool_use_id.clone(),
        name: "ContextCapture".into(),
        input: lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
            &json!({"original":true}),
        )
        .expect("generated tool input is representable"),
        provider_id: Some("toolu_original".into()),
    };
    let source_row = ConversationMessage::Assistant {
        per_turn_effort: None,
        id: MessageId::new(),
        content: vec![
            ContentBlock::Text {
                text: "raw text".into(),
                citations: None,
            },
            source_tool_use.clone(),
        ],
        stop_reason: Some("tool_use".into()),
        resumable: None,
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
            lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
                &json!({"original":true}),
            )
            .expect("generated tool input is representable"),
            Some("toolu_original".into()),
        ),
        None,
        accepted_row.id(),
        crate::turn_loop::ToolUseDispatchFacts {
            native_query_depth: 0,
            query_history: prequery.clone().into(),
            assistant_message: accepted_row.clone(),
            same_turn_tool_uses: Vec::new(),
            refusal_call_boundary: None,
        },
    )
    .await
    .expect("dispatch accepted ToolUse");
    assert_eq!(dispatched.results.len(), 1);

    let captured = calls.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(
        captured[0].input.to_json_string().unwrap(),
        r#"{"original":true}"#
    );
    assert_eq!(captured[0].tool_use_id, Some(tool_use_id));
    assert_eq!(captured[0].assistant_message_id, Some(accepted_row.id()));
    assert_eq!(captured[0].messages, prequery);
    assert_eq!(captured[0].assistant_message, Some(accepted_row.clone()));
    assert!(captured[0].same_turn_tool_uses.is_empty());
}

#[tokio::test]
async fn appended_batched_tool_injection_contributes_query_user_event() {
    let root = tempfile::tempdir().expect("tempdir");
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        root.path().to_path_buf(),
    );
    orch.begin_turn_metrics();

    crate::turn_loop::append_tool_injected_messages(
        &orch,
        vec![(
            ConversationMessage::user_meta(MessageId::new(), "post-tool injected user".into()),
            ToolUseId::new(),
        )],
        None,
    )
    .await;
    orch.complete_turn_metrics(None);

    assert_eq!(
        orch.completed_query_user_events(),
        Some((1, None)),
        "the accepted meta user row appended by the batched helper is a query yield"
    );
}

struct QueryRowTool;

#[async_trait]
impl Tool for QueryRowTool {
    fn name(&self) -> &str {
        "QueryRow"
    }

    fn input_schema(&self) -> &lingxi_core::types::utf16_json::Utf16JsonProjection {
        static SCHEMA: once_cell::sync::Lazy<lingxi_core::types::utf16_json::Utf16JsonProjection> =
            once_cell::sync::Lazy::new(|| {
                tool_api::tool_trait::generated_builtin_schema(&json!({"type":"object"}))
            });
        &SCHEMA
    }

    fn input_repair_metadata(&self) -> tool_api::tool_trait::ToolInputRepairMetadata {
        tool_api::tool_trait::ToolInputRepairMetadata::Known {
            schema: tool_api::tool_trait::ToolInputRepairSchema::Advertised,
            schema_provenance: tool_api::tool_trait::ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }

    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        1024
    }

    fn is_concurrency_safe(&self, _: &lingxi_core::types::utf16_json::Utf16JsonProjection) -> bool {
        true
    }

    fn is_read_only(&self, _: &lingxi_core::types::utf16_json::Utf16JsonProjection) -> bool {
        true
    }

    async fn validate_input(
        &self,
        _: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        Ok(())
    }

    async fn check_permissions(
        &self,
        _: &lingxi_core::types::utf16_json::Utf16JsonProjection,
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

    async fn description(
        &self,
        _: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        _: &DescriptionOptions,
    ) -> String {
        "produce a query-local user row".into()
    }

    async fn prompt(
        &self,
        _: &PromptOptions,
    ) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
        String::new().into()
    }

    async fn call(
        &self,
        _: lingxi_core::types::utf16_json::Utf16JsonProjection,
        _: ToolUseContext,
        _: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        Ok(ToolCallResult {
            data: tool_api::tool_trait::generated_builtin_result(&json!({
                "content": "query tool result"
            })),
            model_content: None,
            new_messages: vec![ConversationMessage::user(
                MessageId::new(),
                "injected query user row".into(),
            )],
            context_modifier: None,
            is_error: false,
            subagent_hooks_fired: false,
            mcp_meta: None,
        })
    }
}

async fn completed_multirow_query() -> (
    Arc<ConversationOrchestrator>,
    Arc<MockOutputStream>,
    tempfile::TempDir,
) {
    let root = tempfile::tempdir().expect("tempdir");
    let tool_use_id = ToolUseId::new();
    let tool_response = mock_message_response(
        vec![llm_runtime::ContentBlock::ToolCall {
            id: tool_use_id.to_string(),
            name: "QueryRow".into(),
            input: lingxi_llm_client::protocol::JsonPayload::parse("{}").unwrap(),
        }],
        Some("tool_use"),
    );
    let final_response = mock_message_response(
        vec![llm_runtime::ContentBlock::Text {
            text: "query completed".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    );
    let mut tool_registry = ToolRegistry::new();
    tool_registry.register_builtin(Arc::new(QueryRowTool));
    let output = Arc::new(MockOutputStream::new());
    let orch = Arc::new(ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![tool_response, final_response])),
        Arc::new(tool_registry),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        root.path().to_path_buf(),
    ));

    let outcome = orch
        .run_turn_with_cancel(
            "query with a tool result and an injected user row",
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("the multi-row query completes");
    assert_eq!(outcome, crate::conversation::TurnOutcome::EndTurn);
    assert_eq!(
        orch.completed_query_user_events(),
        Some((2, None)),
        "the completed query has two actual yielded user rows"
    );
    assert_eq!(
        orch.completed_turn_metrics()
            .and_then(|metrics| metrics.stop_reason),
        Some("end_turn".into()),
        "the completed query leaves terminal metadata to detect stale reuse"
    );
    (orch, output, root)
}

#[tokio::test]
async fn pre_admission_service_shutdown_uses_fresh_query_metrics_and_dedupes_marker() {
    use crate::prompt::mid_turn_input::{CancelReason, CancelReasonFlag};

    let (orch, _, _root) = completed_multirow_query().await;
    let previous_history_len = orch.session.lock().await.history.len();
    let shutdown_reason = CancelReasonFlag::new();
    orch.set_cancel_reason(shutdown_reason.clone());
    shutdown_reason.set(CancelReason::ServiceShutdown);

    // Hold the admission gate to prove the cancelled call cannot replace the
    // completed query's snapshot until it owns the same serialized boundary.
    let gate = orch.turn_gate.lock().await;
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let cancelled_turn = {
        let orch = orch.clone();
        tokio::spawn(async move {
            let cancel = tokio_util::sync::CancellationToken::new();
            cancel.cancel();
            started_tx.send(()).unwrap();
            orch.run_turn_with_cancel("cancel before admission", cancel)
                .await
        })
    };
    started_rx.await.unwrap();
    assert!(
        !cancelled_turn.is_finished(),
        "the shutdown marker waits for the gate"
    );
    assert_eq!(orch.completed_query_user_events(), Some((2, None)));
    assert_eq!(
        orch.completed_turn_metrics()
            .and_then(|metrics| metrics.stop_reason),
        Some("end_turn".into())
    );
    assert_eq!(
        orch.session.lock().await.history.len(),
        previous_history_len
    );
    drop(gate);

    assert_eq!(
        cancelled_turn.await.unwrap().unwrap(),
        crate::conversation::TurnOutcome::Cancelled
    );
    assert_eq!(
        orch.completed_query_user_events(),
        Some((
            1,
            Some(crate::conversation::QueryUserInterruptionKind::Streaming)
        )),
        "the new snapshot contains only its newly persisted shutdown marker"
    );
    assert_eq!(
        orch.completed_turn_metrics()
            .and_then(|metrics| metrics.stop_reason),
        None,
        "pre-admission cancellation must not inherit the preceding end_turn"
    );

    let after_first_shutdown = orch.session.lock().await.history.clone();
    let shutdown_markers = after_first_shutdown
        .iter()
        .filter(|message| {
            matches!(message,
                ConversationMessage::User {
                    content,
                    interrupted_by_shutdown: Some(true),
                    ..
                } if content.iter().any(|block| matches!(block,
                    ContentBlock::Text { text, .. }
                        if text == "[Request interrupted by user]"
                ))
            )
        })
        .count();
    assert_eq!(shutdown_markers, 1);

    let duplicate_cancel = tokio_util::sync::CancellationToken::new();
    duplicate_cancel.cancel();
    assert_eq!(
        orch.run_turn_with_cancel("duplicate pre-admission shutdown", duplicate_cancel)
            .await
            .unwrap(),
        crate::conversation::TurnOutcome::Cancelled
    );
    assert_eq!(
        orch.completed_query_user_events(),
        Some((0, None)),
        "a deduped existing marker starts an empty query-local snapshot"
    );
    assert_eq!(
        orch.completed_turn_metrics()
            .and_then(|metrics| metrics.stop_reason),
        None
    );
    let after_dedupe = orch.session.lock().await.history.clone();
    assert_eq!(
        after_dedupe
            .iter()
            .filter(|message| {
                matches!(message,
                    ConversationMessage::User {
                        content,
                        interrupted_by_shutdown: Some(true),
                        ..
                    } if content.iter().any(|block| matches!(block,
                        ContentBlock::Text { text, .. }
                            if text == "[Request interrupted by user]"
                    ))
                )
            })
            .count(),
        1,
        "dedupe keeps one durable shutdown marker"
    );
}

struct EmptyTaskRegistry;

#[tokio::test]
async fn cancelled_task_notification_admission_preserves_running_owner() {
    let (orch, output, _root) = completed_multirow_query().await;
    let _owner = orch.turn_gate.lock().await;
    let previous_query = orch.completed_query_user_events();
    let previous_events = output.snapshot().await.len();
    for precancelled in [false, true] {
        let cancel = tokio_util::sync::CancellationToken::new();
        if precancelled {
            cancel.cancel();
        }
        let wake = orch.run_task_notification_rewake(&EmptyTaskRegistry, cancel.clone());
        tokio::pin!(wake);
        if !precancelled {
            tokio::select! {
                biased;
                result = &mut wake => panic!("wake bypassed the running owner: {result:?}"),
                () = tokio::task::yield_now() => {}
            }
            cancel.cancel();
        }
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), wake)
                .await
                .expect("cancel must not wait for the running owner")
                .unwrap(),
            crate::conversation::TurnOutcome::Cancelled,
        );
        assert_eq!(orch.completed_query_user_events(), previous_query);
        assert_eq!(output.snapshot().await.len(), previous_events);
    }
}

#[async_trait]
impl lingxi_core::host::task_registry::TaskRegistryHandle for EmptyTaskRegistry {
    async fn create(
        &self,
        _: lingxi_core::host::task_registry::TaskCreateInput,
    ) -> Result<
        lingxi_core::host::task_registry::TaskRecord,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        unreachable!("the empty registry is never mutated")
    }

    async fn get(
        &self,
        _: &str,
    ) -> Result<
        Option<lingxi_core::host::task_registry::TaskRecord>,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        Ok(None)
    }

    async fn list(
        &self,
        _: lingxi_core::host::task_registry::TaskListFilter,
    ) -> Result<
        Vec<lingxi_core::host::task_registry::TaskRecord>,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        Ok(Vec::new())
    }

    async fn update(
        &self,
        _: &str,
        _: lingxi_core::host::task_registry::TaskUpdatePatch,
    ) -> Result<
        lingxi_core::host::task_registry::TaskRecord,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        unreachable!("the empty registry is never mutated")
    }

    async fn set_status(
        &self,
        _: &str,
        _: &str,
    ) -> Result<
        lingxi_core::host::task_registry::TaskRecord,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        unreachable!("the empty registry is never mutated")
    }

    async fn kill(
        &self,
        _: &str,
    ) -> Result<
        lingxi_core::host::task_registry::TaskRecord,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        unreachable!("the empty registry is never mutated")
    }

    async fn output(
        &self,
        _: &str,
        _: Option<u64>,
    ) -> Result<
        lingxi_core::host::task_registry::TaskOutputChunk,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        unreachable!("the empty registry is never mutated")
    }
}

#[tokio::test]
async fn no_pending_task_notification_rewake_clears_old_query_count_and_emits_end_turn() {
    let (orch, output, _root) = completed_multirow_query().await;
    assert_eq!(orch.completed_query_user_events(), Some((2, None)));
    let end_turn_count_before = output
        .snapshot()
        .await
        .iter()
        .filter(|event| matches!(event, lingxi_core::host::OutputEvent::EndTurn { .. }))
        .count();

    let outcome = orch
        .run_task_notification_rewake(
            &EmptyTaskRegistry,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("empty task-notification recheck completes");
    assert_eq!(outcome, crate::conversation::TurnOutcome::EndTurn);
    assert_eq!(
        orch.completed_query_user_events(),
        None,
        "a no-work recheck cannot publish the preceding query's user-row count"
    );
    let end_turn_count_after = output
        .snapshot()
        .await
        .iter()
        .filter(|event| matches!(event, lingxi_core::host::OutputEvent::EndTurn { .. }))
        .count();
    assert_eq!(end_turn_count_after, end_turn_count_before + 1);
}

struct DeliveredReportNotification(Mutex<bool>);

#[async_trait]
impl crate::prompt::task_notification::TaskNotificationProvider for DeliveredReportNotification {
    async fn take_pending_task_notifications(
        &self,
    ) -> Vec<lingxi_core::host::task_registry::TaskNotification> {
        if std::mem::replace(&mut *self.0.lock().unwrap(), false) {
            vec![lingxi_core::host::task_registry::TaskNotification {
                task_id: "reported-child".into(),
                task_type: "local_agent".into(),
                status: "completed".into(),
                description: "configuration review".into(),
                result: Some(
                    lingxi_core::host::handback::handback_pointer(false, "lingxi-code-guide")
                        .into(),
                ),
                ..Default::default()
            }]
        } else {
            Vec::new()
        }
    }
}

fn response_stream(text: Option<&str>) -> Vec<llm_runtime::HistoryEvent> {
    use crate::test_support::{
        content_block_start_text, content_block_stop, message_delta_stop, message_start,
        message_stop, text_delta,
    };
    let mut events = vec![message_start("notification-response", "test-model")];
    if let Some(text) = text {
        events.extend([
            content_block_start_text(0),
            text_delta(0, text),
            content_block_stop(0),
        ]);
    }
    events.extend([message_delta_stop("end_turn"), message_stop()]);
    events
}

#[tokio::test]
async fn notification_rewake_may_end_silently_without_forcing_a_duplicate_answer() {
    let root = tempfile::tempdir().unwrap();
    let streaming = Arc::new(crate::test_support::MockStreamingApiClient::with_turns(
        vec![response_stream(None)],
    ));
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            streaming.clone(),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            root.path().to_path_buf(),
        )
        .with_task_notifications(Arc::new(DeliveredReportNotification(Mutex::new(true)))),
    );
    orch.session.lock().await.history.extend([
        ConversationMessage::user(MessageId::new(), "Read the report and answer once".into()),
        ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![ContentBlock::Text {
                text: "Report received; here is the answer".into(),
                citations: None,
            }],
            per_turn_effort: None,
            stop_reason: Some("end_turn".into()),
            resumable: None,
        },
    ]);
    orch.run_async_hook_rewake(None).await.unwrap();
    assert_eq!(
        streaming.captured_calls().await.len(),
        1,
        "a silent notification does not retry"
    );
    assert!(!orch
        .session
        .lock()
        .await
        .history
        .iter()
        .any(|message| message
            .text_content()
            .contains(crate::turn_loop::THINKING_ONLY_NUDGE)));
}

#[tokio::test]
async fn human_question_with_empty_response_still_gets_one_visible_output_recovery() {
    let root = tempfile::tempdir().unwrap();
    let streaming = Arc::new(crate::test_support::MockStreamingApiClient::with_turns(
        vec![
            response_stream(None),
            response_stream(Some("Recovered answer")),
        ],
    ));
    let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        streaming.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        root.path().to_path_buf(),
    ));
    orch.run_turn_streaming("Answer the question")
        .await
        .unwrap();
    assert_eq!(streaming.captured_calls().await.len(), 2);
    assert!(orch
        .session
        .lock()
        .await
        .history
        .iter()
        .any(|message| message
            .text_content()
            .contains(crate::turn_loop::THINKING_ONLY_NUDGE)));
}

#[tokio::test]
async fn human_input_during_notification_rewake_restores_visible_answer_requirement() {
    let root = tempfile::tempdir().unwrap();
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        root.path().to_path_buf(),
    );
    let old_user = ConversationMessage::user(MessageId::new(), "original request".into());
    orch.session.lock().await.history.push(old_user.clone());
    let mut state = super::TurnLoopState::new(&orch, old_user.id(), None);
    state.notification_user_boundary = Some(orch.latest_response_request_user().await);
    orch.session
        .lock()
        .await
        .history
        .push(ConversationMessage::user_meta(
            MessageId::new(),
            "report was already delivered".into(),
        ));
    assert!(!orch.requires_visible_response(&mut state).await);
    orch.session
        .lock()
        .await
        .history
        .push(ConversationMessage::user(
            MessageId::new(),
            "I have another question".into(),
        ));
    assert!(orch.requires_visible_response(&mut state).await);
    assert!(state.notification_user_boundary.is_none());
}
