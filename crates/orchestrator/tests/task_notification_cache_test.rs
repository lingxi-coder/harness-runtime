//! Per-step notification delivery and cancellation regressions for both turn drivers.

use async_trait::async_trait;
use lingxi_core::host::task_notification::TaskNotificationProvider;
use lingxi_core::host::task_registry::TaskNotification;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::ConversationMessage;
use llm_runtime::{ContentBlock, HistoryEvent, HistoryResponse};
use orchestrator::test_support::{
    content_block_start_text, content_block_start_tool_use, content_block_stop, input_json_delta,
    message_delta_stop, message_start, message_stop, mock_message_response, noop_hook_executor,
    text_delta, MockApiClient, MockOutputStream, MockStreamingApiClient, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    generated_builtin_result, generated_builtin_schema, DescriptionOptions, PromptOptions, Tool,
    ToolInputRepairMetadata, ToolInputRepairSchema, ToolInputSchemaProvenance, ToolStaticContext,
};
use tool_api::{ToolCallResult, ToolError};

const TASK_ID: &str = "b_cache_review_notification";

struct ContinueFixture {
    calls: Arc<AtomicUsize>,
    schema: Utf16JsonProjection,
}

#[async_trait]
impl Tool for ContinueFixture {
    fn name(&self) -> &str {
        "ContinueFixture"
    }
    fn input_schema(&self) -> &Utf16JsonProjection {
        &self.schema
    }
    fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
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
        1_024
    }
    fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
        false
    }
    fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
        false
    }
    async fn check_permissions(
        &self,
        _: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "isolated notification-cache fixture".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }
    async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
        "Complete one deterministic fixture step".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
        "Complete one deterministic fixture step".into()
    }
    async fn call(
        &self,
        _: Utf16JsonProjection,
        _: ToolUseContext,
        _: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolCallResult::from_data(generated_builtin_result(
            &serde_json::json!({"ok": true}),
        )))
    }
}

/// Faithful consume-once provider: an empty first drain does not consume the
/// later completion; a nonempty drain marks exactly this one completion sent.
struct FixtureNotifications {
    initially_ready: bool,
    gated_drain: Option<Arc<DiagnosticsGate>>,
    tool_calls: Arc<AtomicUsize>,
    drains: AtomicUsize,
    delivered: AtomicBool,
}

#[async_trait]
impl TaskNotificationProvider for FixtureNotifications {
    async fn take_pending_task_notifications(&self) -> Vec<TaskNotification> {
        self.drains.fetch_add(1, Ordering::SeqCst);
        let ready = self.initially_ready || self.tool_calls.load(Ordering::SeqCst) > 0;
        if !ready || self.delivered.swap(true, Ordering::SeqCst) {
            return Vec::new();
        }
        if let Some(gate) = &self.gated_drain {
            lingxi_core::host::NewDiagnosticsSource::take_new_diagnostics_block(gate.as_ref())
                .await;
        }
        vec![TaskNotification {
            task_id: TASK_ID.into(),
            task_type: "local_bash".into(),
            status: "completed".into(),
            description: "fixture background task".into(),
            exit_code: Some(0),
            ..Default::default()
        }]
    }
}

fn scripted_responses() -> Vec<HistoryResponse> {
    let mut tool = mock_message_response(
        vec![ContentBlock::ToolCall {
            id: "call_notification_continue".into(),
            name: "ContinueFixture".into(),
            input: tool_api::hosted_search::JsonPayload::parse("{}").unwrap(),
        }],
        Some("tool_use"),
    );
    let mut done = mock_message_response(
        vec![ContentBlock::Text {
            text: "fixture done".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    );
    tool.model = "gpt-6.1-sol".into();
    done.model = "gpt-6.1-sol".into();
    vec![tool, done]
}

fn scripted_streams(responses: &[HistoryResponse]) -> Vec<Vec<HistoryEvent>> {
    responses
        .iter()
        .map(|response| {
            let mut events = vec![message_start(&response.id, &response.model)];
            match &response.content[0] {
                ContentBlock::ToolCall { id, name, .. } => {
                    events.extend([
                        content_block_start_tool_use(0, id.clone().into(), name),
                        input_json_delta(0, "{"),
                        input_json_delta(0, "}"),
                    ]);
                }
                ContentBlock::Text { text, .. } => {
                    events.extend([content_block_start_text(0), text_delta(0, text)]);
                }
                _ => unreachable!("the mock returns only one tool or text block"),
            }
            events.extend([
                content_block_stop(0),
                message_delta_stop(response.stop_reason.as_deref().unwrap()),
                message_stop(),
            ]);
            events
        })
        .collect()
}

fn notification_rows(messages: &[ConversationMessage]) -> Vec<String> {
    messages
        .iter()
        .filter(|message| message.text_content().contains(TASK_ID))
        .map(|message| message.id().as_uuid().to_string())
        .collect()
}

struct SteeringInput {
    calls: Arc<AtomicUsize>,
    delivered: AtomicBool,
}
#[async_trait]
impl orchestrator::prompt::mid_turn_input::MidTurnInputSource for SteeringInput {
    async fn take_mid_turn_input(&self) -> Option<Utf16JsonProjection> {
        (self.calls.load(Ordering::SeqCst) > 0 && !self.delivered.swap(true, Ordering::SeqCst))
            .then(|| "additional human input during execution".into())
    }
}

async fn run_case(streaming: bool, initially_ready: bool, bare: bool, steering: bool) {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path().canonicalize().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let notifications = Arc::new(FixtureNotifications {
        initially_ready,
        gated_drain: None,
        tool_calls: calls.clone(),
        drains: AtomicUsize::new(0),
        delivered: AtomicBool::new(false),
    });
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(ContinueFixture {
        calls: calls.clone(),
        schema: generated_builtin_schema(&serde_json::json!({"type":"object"})),
    }));
    let responses = scripted_responses();
    let batched = Arc::new(MockApiClient::new(responses.clone()));
    let streamed = Arc::new(MockStreamingApiClient::with_turns(scripted_streams(
        &responses,
    )));
    let orchestrator = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig {
                model: "gpt-6.1-sol".into(),
                bare,
                max_turns: 4,
                ..Default::default()
            },
            batched.clone(),
            streamed.clone(),
            Arc::new(tools),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            cwd.clone(),
        )
        .with_config_home(cwd.join("config"))
        .with_task_notifications(notifications.clone())
        .with_mid_turn_input(Arc::new(SteeringInput {
            calls: calls.clone(),
            delivered: AtomicBool::new(!steering),
        })),
    );
    orchestrator.initialize_conversation_mode().await.unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(20), async {
        if streaming {
            orchestrator
                .run_turn_streaming("Run ContinueFixture once, then finish")
                .await
        } else {
            orchestrator
                .run_turn("Run ContinueFixture once, then finish")
                .await
        }
    })
    .await;
    let batched_requests = batched.captured_msgs().await;
    let stream_requests = streamed.captured_calls().await;
    let requests = if streaming {
        stream_requests
            .iter()
            .map(|call| call.messages.clone())
            .collect::<Vec<_>>()
    } else {
        batched_requests.clone()
    };
    let row_ids = requests
        .iter()
        .map(|messages| notification_rows(messages))
        .collect::<Vec<_>>();
    let counts = row_ids.iter().map(Vec::len).collect::<Vec<_>>();
    let expected = if initially_ready {
        vec![1, 1]
    } else {
        vec![0, 1]
    };
    let final_history = orchestrator.snapshot_history().await;
    assert!(matches!(outcome, Ok(Ok(_))), "{outcome:?}");
    assert_eq!(
        counts, expected,
        "streaming={streaming}, initially_ready={initially_ready}"
    );
    assert_eq!(notifications.drains.load(Ordering::SeqCst), 2);
    assert_eq!(notification_rows(&final_history).len(), 1);
}

struct DiagnosticsGate {
    first: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait]
impl lingxi_core::host::NewDiagnosticsSource for DiagnosticsGate {
    async fn take_new_diagnostics_block(&self) -> Option<Utf16JsonProjection> {
        if self.first.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        None
    }
}

/// Cancel either inside the consuming provider or after its drain, then resume
/// the same session before the next ordinary request.
async fn run_cancel_after_drain(streaming: bool, within_drain: bool) {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path().canonicalize().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(DiagnosticsGate {
        first: AtomicBool::new(true),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let notifications = Arc::new(FixtureNotifications {
        initially_ready: true,
        gated_drain: within_drain.then(|| gate.clone()),
        tool_calls: calls,
        drains: AtomicUsize::new(0),
        delivered: AtomicBool::new(false),
    });
    let mut response = mock_message_response(
        vec![ContentBlock::Text {
            text: "next query done".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    );
    response.model = "gpt-6.1-sol".into();
    // The cancelled attempt never opens a model request. This single response
    // is for the subsequent ordinary query.
    let streamed = Arc::new(MockStreamingApiClient::with_turns(scripted_streams(&[
        response.clone(),
    ])));
    let api = Arc::new(MockApiClient::new(vec![response]));
    let orchestrator = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig {
                model: "gpt-6.1-sol".into(),
                max_turns: 4,
                ..Default::default()
            },
            api.clone(),
            streamed.clone(),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            cwd.clone(),
        )
        .with_config_home(cwd.join("config"))
        .with_task_notifications(notifications.clone())
        .with_new_diagnostics_source(gate.clone()),
    );
    orchestrator.initialize_conversation_mode().await.unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut first = Box::pin(async {
        if streaming {
            orchestrator
                .run_turn_streaming_with_cancel("First prompt", cancel.clone())
                .await
        } else {
            orchestrator
                .run_turn_with_cancel("First prompt", cancel.clone())
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(20), async {
        tokio::select! {
            () = gate.entered.notified() => {},
            result = &mut first => panic!("first attempt ended before the diagnostics gate: {result:?}"),
        }
    }).await.expect("reach diagnostics after the notification drain");
    let drained_before_cancel = notifications.drains.load(Ordering::SeqCst);
    let consumed_before_cancel = notifications.delivered.load(Ordering::SeqCst);
    cancel.cancel();
    if streaming || within_drain {
        gate.release.notify_one();
    }
    let initial_cancel_outcome = tokio::time::timeout(Duration::from_secs(5), &mut first).await;
    let cancelled = matches!(
        &initial_cancel_outcome,
        Ok(Ok(orchestrator::TurnOutcome::Cancelled))
    );
    let initially_timed_out = initial_cancel_outcome.is_err();
    let mut first_outcome = format!("{initial_cancel_outcome:?}");
    // Also release the fixture when diagnosing a future implementation that
    // intentionally waits for the accepted notification boundary.
    gate.release.notify_one();
    if initially_timed_out {
        let after_release = tokio::time::timeout(Duration::from_secs(5), &mut first).await;
        first_outcome = format!("{first_outcome}; after release: {after_release:?}");
    }
    drop(first);
    use lingxi_core::host::OrchestratorHandle;
    let session_id = orchestrator.current_session_id().await;
    let history = orchestrator.snapshot_history().await;
    orchestrator
        .resume_session(
            session_id,
            history,
            None,
            None,
            orchestrator::test_support::code_resume_runtime_snapshot(session_id),
        )
        .await
        .unwrap();
    let before_next = notification_rows(&orchestrator.snapshot_history().await).len();
    let next_outcome = tokio::time::timeout(Duration::from_secs(20), async {
        if streaming {
            orchestrator
                .run_turn_streaming("Continue the next query")
                .await
        } else {
            orchestrator.run_turn("Continue the next query").await
        }
    })
    .await;
    let requests = if streaming {
        streamed
            .captured_calls()
            .await
            .iter()
            .map(|call| call.messages.clone())
            .collect::<Vec<_>>()
    } else {
        api.captured_msgs().await
    };
    let counts = requests
        .iter()
        .map(|messages| notification_rows(messages).len())
        .collect::<Vec<_>>();
    assert_eq!(drained_before_cancel, 1);
    assert!(consumed_before_cancel);
    assert!(cancelled && !initially_timed_out, "{first_outcome}");
    assert!(matches!(next_outcome, Ok(Ok(_))), "{next_outcome:?}");
    assert!(before_next <= 1, "resume duplicated the notification");
    assert_eq!(
        counts,
        vec![1],
        "streaming={streaming}, outcome={first_outcome}"
    );
    assert_eq!(
        notification_rows(&orchestrator.snapshot_history().await).len(),
        1
    );
}

#[tokio::test]
async fn batched_notifications_are_collected_per_step() {
    for ready in [false, true] {
        for bare in [false, true] {
            for steering in [false, true] {
                run_case(false, ready, bare, steering).await;
            }
        }
    }
}
#[tokio::test]
async fn streaming_notifications_are_collected_per_step() {
    for ready in [false, true] {
        for bare in [false, true] {
            for steering in [false, true] {
                run_case(true, ready, bare, steering).await;
            }
        }
    }
}
#[tokio::test]
async fn batched_cancel_after_drain_retains_notification() {
    run_cancel_after_drain(false, false).await;
}
#[tokio::test]
async fn streaming_cancel_after_drain_retains_notification() {
    run_cancel_after_drain(true, false).await;
}

#[tokio::test]
async fn batched_cancel_inside_consume_once_provider_retains_notification() {
    run_cancel_after_drain(false, true).await;
}
#[tokio::test]
async fn streaming_cancel_inside_consume_once_provider_retains_notification() {
    run_cancel_after_drain(true, true).await;
}
