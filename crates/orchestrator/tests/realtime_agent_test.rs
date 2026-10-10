//! Native realtime Agent tool policy, history, cancellation and budget regressions.
use async_trait::async_trait;
use futures::StreamExt;
use lingxi_core::host::{PermissionDecision, PermissionGate};
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId, ToolUseId};
use lingxi_llm_client::realtime::*;
use orchestrator::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, StaticMemoryProvider,
};
use orchestrator::{
    realtime_history, ConversationOrchestrator, RealtimeAgentEnd, RealtimeAgentInput,
    RealtimeAgentLimits,
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tool_api::tool_trait::*;
use tool_api::{context::ToolUseContext, progress::ToolProgressSender, registry::ToolRegistry};
#[test]
fn history_preserves_tool_identity_and_rejects_unimportable_media() {
    let id = ToolUseId::new();
    let history = vec![
        ConversationMessage::user(MessageId::new(), "prior".into()),
        ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![ContentBlock::ToolUse {
                id: id.clone(),
                name: "fixture".into(),
                input: json!({"x":1}),
                input_projection: None,
                provider_id: Some("provider-call".into()),
            }],
            stop_reason: Some("tool_use".into()),
            per_turn_effort: None,
        },
        ConversationMessage::User {
            id: MessageId::new(),
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id,
                content: "result".into(),
                is_error: Some(false),
                provider_tool_use_id: Some("provider-call".into()),
                content_blocks: None,
                content_projection: None,
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            api_message_override: None,
        },
    ];
    let imported = realtime_history(&history).unwrap();
    assert!(
        matches!(&imported[1],RealtimeHistoryItem::ToolCall{call_id,..} if call_id=="provider-call")
    );
    assert!(
        matches!(&imported[2],RealtimeHistoryItem::ToolResult{call_id,..} if call_id=="provider-call")
    );
    let mut unmatched = history.clone();
    unmatched.remove(1);
    assert!(realtime_history(&unmatched).is_err());
}
struct FixtureTool {
    name: &'static str,
    calls: Arc<AtomicUsize>,
    completion: Option<Arc<tokio::sync::Notify>>,
}
#[async_trait]
impl Tool for FixtureTool {
    fn name(&self) -> &str {
        self.name
    }
    fn input_schema(&self) -> &Value {
        static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| json!({"type":"object","properties":{}}))
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    async fn validate_input(&self, _: &Value, _: &ToolUseContext) -> Result<(), ValidationError> {
        Ok(())
    }
    async fn check_permissions(
        &self,
        _: &Value,
        _: &ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Ask {
            reason: permission::PermissionDecisionReason::Other {
                reason: "fixture permission".into(),
            },
            prompt: permission::result::PermissionPrompt {
                title: "fixture".into(),
                message: "approve fixture".into(),
                options: vec![],
            },
            pending_classifier_check: None,
            metadata: Default::default(),
        }
    }
    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        "fixture".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> String {
        "fixture".into()
    }
    async fn call(
        &self,
        _: Value,
        _: ToolUseContext,
        _: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(completion) = &self.completion {
            completion.notified().await;
        }
        Ok(ToolCallResult {
            data: json!({"content":"completed"}),
            data_projection: None,
            model_content_projection: None,
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            mcp_meta: None,
            mcp_meta_projection: None,
            is_error: false,
        })
    }
}
struct Gate {
    calls: AtomicUsize,
    deny: bool,
    release: Option<Arc<tokio::sync::Notify>>,
    approved: AtomicBool,
}
#[async_trait]
impl PermissionGate for Gate {
    async fn check(&self, _: &str, _: &Value) -> PermissionDecision {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(release) = &self.release {
            if !self.approved.load(Ordering::Acquire) {
                release.notified().await;
                self.approved.store(true, Ordering::Release);
            }
        }
        if self.deny {
            PermissionDecision::Deny {
                reason: "denied fixture".into(),
            }
        } else {
            PermissionDecision::Allow
        }
    }
}
async fn fixture(gate: Arc<Gate>) -> (Arc<ConversationOrchestrator>, Arc<AtomicUsize>) {
    fixture_with_completion(gate, None).await
}
async fn fixture_with_completion(
    gate: Arc<Gate>,
    completion: Option<Arc<tokio::sync::Notify>>,
) -> (Arc<ConversationOrchestrator>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    for name in ["fixture", "voice", "speech"] {
        registry.register_builtin(Arc::new(FixtureTool {
            name,
            calls: calls.clone(),
            completion: completion.clone(),
        }));
    }
    let orch = Arc::new(ConversationOrchestrator::new(
        orchestrator::OrchestratorConfig {
            interactive_permissions: true,
            ..Default::default()
        },
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(registry),
        noop_hook_executor(),
        gate,
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::path::PathBuf::from("/work/realtime"),
    ));
    orch.session().lock().await.model_profile = Some("exact-profile".into());
    (orch, calls)
}
struct Codec;
impl RealtimeCodec for Codec {
    fn capabilities(&self) -> RealtimeCapabilities {
        RealtimeCapabilities {
            history_import: true,
            tools: true,
            input_transcription: true,
            output_transcription: true,
            interruption: true,
            ..Default::default()
        }
    }
    fn encode(&self, input: &RealtimeInput) -> Result<Vec<RealtimeFrame>, RealtimeError> {
        Ok(vec![RealtimeFrame::text(format!("{input:?}"))])
    }
    fn decode(&self, frame: RealtimeFrame) -> Result<Vec<RealtimeEvent>, RealtimeError> {
        let RealtimeFrame::Text(bytes) = frame else {
            panic!("fixture")
        };
        let kind = String::from_utf8(bytes.to_vec()).unwrap();
        Ok(vec![match kind.as_str() {
            "tool" => RealtimeEvent::ToolCall {
                call_id: "call".into(),
                name: "fixture".into(),
                arguments: json!({}),
            },
            "tool-second" => RealtimeEvent::ToolCall {
                call_id: "call-second".into(),
                name: "fixture".into(),
                arguments: json!({}),
            },
            "audio" => RealtimeEvent::AudioDelta {
                data: bytes.clone(),
                format: RealtimeAudioFormat::Pcm16 {
                    sample_rate_hz: 24_000,
                },
                item_id: Some("item-heard".into()),
            },
            "output" => RealtimeEvent::Transcript {
                direction: RealtimeTranscriptDirection::Output,
                update: RealtimeTranscriptUpdate::Replace,
                text: "heard assistant".into(),
                item_id: Some("item-heard".into()),
                turn_id: Some("turn".into()),
                final_chunk: true,
            },
            "audio-unheard" => RealtimeEvent::AudioDelta {
                data: bytes.clone(),
                format: RealtimeAudioFormat::Pcm16 {
                    sample_rate_hz: 24_000,
                },
                item_id: Some("item-unheard".into()),
            },
            "output-unheard" => RealtimeEvent::Transcript {
                direction: RealtimeTranscriptDirection::Output,
                update: RealtimeTranscriptUpdate::Replace,
                text: "unheard assistant".into(),
                item_id: Some("item-unheard".into()),
                turn_id: Some("turn2".into()),
                final_chunk: true,
            },
            "input" => RealtimeEvent::Transcript {
                direction: RealtimeTranscriptDirection::Input,
                update: RealtimeTranscriptUpdate::Replace,
                text: "spoken input".into(),
                item_id: Some("item-input".into()),
                turn_id: Some("turn".into()),
                final_chunk: true,
            },
            "interrupt" => RealtimeEvent::Interrupted,
            "cancel-tool" => RealtimeEvent::ToolCancelled {
                call_ids: vec!["call".into()],
            },
            _ => RealtimeEvent::SessionReady,
        }])
    }
}
struct Sink(Arc<std::sync::Mutex<Vec<String>>>);
#[async_trait]
impl RealtimeSink for Sink {
    async fn send(&mut self, frame: RealtimeFrame) -> Result<(), RealtimeError> {
        if let RealtimeFrame::Text(bytes) = frame {
            self.0
                .lock()
                .unwrap()
                .push(String::from_utf8(bytes.to_vec()).unwrap());
        }
        Ok(())
    }
    async fn ping(&mut self, _: bytes::Bytes) -> Result<(), RealtimeError> {
        Err(RealtimeError::InvalidInput {
            message: "fixture transport does not support Ping".into(),
        })
    }
    fn abort(&mut self) {
        // No network state to release; the driver drops both fixture halves.
    }
    async fn close(&mut self, _: RealtimeClose) -> Result<(), RealtimeError> {
        Ok(())
    }
}
struct Transport {
    incoming: std::sync::Mutex<
        Option<futures::channel::mpsc::UnboundedReceiver<Result<RealtimeFrame, RealtimeError>>>,
    >,
    sent: Arc<std::sync::Mutex<Vec<String>>>,
}
#[async_trait]
impl RealtimeTransport for Transport {
    async fn connect(
        &self,
        _: RealtimeConnectRequest,
    ) -> Result<RealtimeConnection, RealtimeError> {
        Ok(RealtimeConnection {
            outbound: Box::new(Sink(self.sent.clone())),
            inbound: self.incoming.lock().unwrap().take().unwrap().boxed(),
        })
    }
}
async fn connection() -> (
    RealtimeControl,
    RealtimeEvents,
    tokio::task::JoinHandle<Result<(), RealtimeError>>,
    futures::channel::mpsc::UnboundedSender<Result<RealtimeFrame, RealtimeError>>,
    Arc<std::sync::Mutex<Vec<String>>>,
) {
    let (tx, rx) = futures::channel::mpsc::unbounded();
    let sent = Arc::new(std::sync::Mutex::new(vec![]));
    let transport = Transport {
        incoming: std::sync::Mutex::new(Some(rx)),
        sent: sent.clone(),
    };
    let (session, driver) = RealtimeSession::connect(
        &transport,
        RealtimeConnectRequest {
            endpoint: "wss://fixture".into(),
            headers: vec![],
            max_frame_bytes: 1024,
        },
        Arc::new(Codec),
        RealtimeLimits::default(),
    )
    .await
    .unwrap();
    let (control, events) = session.into_parts();
    (control, events, tokio::spawn(driver.run()), tx, sent)
}
#[tokio::test]
async fn realtime_tool_loop_keeps_idle_paused_during_permission_and_uses_common_result() {
    let release = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: false,
        release: Some(release.clone()),
        approved: AtomicBool::new(false),
    });
    let (orch, calls) = fixture(gate.clone()).await;
    let prepared = orch.prepare_realtime_agent().await.unwrap();
    let (control, events, driver, events_tx, sent) = connection().await;
    let (_inputs_tx, inputs) = mpsc::channel(4);
    let (output, _output_rx) = mpsc::channel(16);
    let cancel = CancellationToken::new();
    let task = tokio::spawn({
        let orch = orch.clone();
        let cancel = cancel.clone();
        async move {
            orch.run_realtime_agent(
                prepared,
                control,
                events,
                inputs,
                output,
                RealtimeAgentLimits {
                    idle_timeout: Duration::from_millis(20),
                    max_duration: Duration::from_secs(2),
                    ..Default::default()
                },
                cancel,
            )
            .await
        }
    });
    events_tx
        .unbounded_send(Ok(RealtimeFrame::text("tool")))
        .unwrap();
    wait_until(
        || gate.calls.load(Ordering::SeqCst) > 0,
        "permission gate called",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(!task.is_finished());
    release.notify_one();
    wait_until(
        || calls.load(Ordering::SeqCst) == 1,
        "approved tool executes",
    )
    .await;
    wait_until(
        || sent.lock().unwrap().len() >= 2,
        "tool result followed by continuation",
    )
    .await;
    assert!(sent.lock().unwrap()[0].starts_with("ToolResult"));
    assert_eq!(sent.lock().unwrap()[1], "ContinueResponse");
    cancel.cancel();
    assert_eq!(task.await.unwrap().unwrap(), RealtimeAgentEnd::Cancelled);
    let _ = driver.await;
    assert_eq!(orch.snapshot_history().await.len(), 2);
}
#[tokio::test]
async fn realtime_deadline_is_absolute_and_provider_tool_cancel_preserves_permission_boundary() {
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: true,
        release: Some(Arc::new(tokio::sync::Notify::new())),
        approved: AtomicBool::new(false),
    });
    let (orch, calls) = fixture(gate.clone()).await;
    let prepared = orch.prepare_realtime_agent().await.unwrap();
    let (control, events, driver, events_tx, _) = connection().await;
    let (_inputs_tx, inputs) = mpsc::channel(4);
    let (output, mut output_rx) = mpsc::channel(16);
    let task = tokio::spawn({
        let orch = orch.clone();
        async move {
            orch.run_realtime_agent(
                prepared,
                control,
                events,
                inputs,
                output,
                RealtimeAgentLimits {
                    idle_timeout: Duration::from_secs(1),
                    max_duration: Duration::from_millis(40),
                    ..Default::default()
                },
                CancellationToken::new(),
            )
            .await
        }
    });
    events_tx
        .unbounded_send(Ok(RealtimeFrame::text("tool")))
        .unwrap();
    wait_until(
        || gate.calls.load(Ordering::SeqCst) > 0,
        "permission gate called",
    )
    .await;
    events_tx
        .unbounded_send(Ok(RealtimeFrame::text("cancel-tool")))
        .unwrap();
    // The host's owner cancellation drains a pending native permission ask.
    while !matches!(
        output_rx.recv().await.unwrap(),
        RealtimeEvent::ToolCancelled { .. }
    ) {}
    gate.release.as_ref().unwrap().notify_one();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        RealtimeAgentEnd::Deadline
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let _ = driver.await;
}

#[tokio::test]
async fn current_catalog_removes_recursive_audio_and_permission_denial_never_executes_tool() {
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: true,
        release: None,
        approved: AtomicBool::new(false),
    });
    let (orch, calls) = fixture(gate.clone()).await;
    let prepared = orch.prepare_realtime_agent().await.unwrap();
    assert_eq!(prepared.profile_name.as_deref(), Some("exact-profile"));
    assert_eq!(prepared.tools.len(), 1);
    assert_eq!(prepared.tools[0]["name"], "fixture");
    let (control, events, driver, events_tx, sent) = connection().await;
    let (_inputs_tx, inputs) = mpsc::channel(4);
    let (output, _rx) = mpsc::channel(16);
    let cancel = CancellationToken::new();
    let task = tokio::spawn({
        let orch = orch.clone();
        let cancel = cancel.clone();
        async move {
            orch.run_realtime_agent(
                prepared,
                control,
                events,
                inputs,
                output,
                RealtimeAgentLimits::default(),
                cancel,
            )
            .await
        }
    });
    events_tx
        .unbounded_send(Ok(RealtimeFrame::text("tool")))
        .unwrap();
    wait_until(
        || sent.lock().unwrap().len() >= 2,
        "tool result followed by continuation",
    )
    .await;
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(sent.lock().unwrap()[0].contains("is_error"));
    cancel.cancel();
    assert_eq!(task.await.unwrap().unwrap(), RealtimeAgentEnd::Cancelled);
    let _ = driver.await;
    assert_eq!(orch.snapshot_history().await.len(), 2);
}

#[tokio::test]
async fn realtime_transcripts_commit_after_playback_ack_and_skip_interrupted_audio() {
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: false,
        release: None,
        approved: AtomicBool::new(false),
    });
    let (orch, _) = fixture(gate).await;
    let prepared = orch.prepare_realtime_agent().await.unwrap();
    let (control, events, driver, events_tx, _) = connection().await;
    let (inputs_tx, inputs) = mpsc::channel(4);
    let (output, mut output_rx) = mpsc::channel(16);
    let cancel = CancellationToken::new();
    let task = tokio::spawn({
        let orch = orch.clone();
        let cancel = cancel.clone();
        async move {
            orch.run_realtime_agent(
                prepared,
                control,
                events,
                inputs,
                output,
                RealtimeAgentLimits::default(),
                cancel,
            )
            .await
        }
    });
    for event in ["input", "audio", "output"] {
        events_tx
            .unbounded_send(Ok(RealtimeFrame::text(event)))
            .unwrap();
        output_rx.recv().await.unwrap();
    }
    assert_eq!(orch.snapshot_history().await.len(), 1);
    inputs_tx
        .send(RealtimeAgentInput::PlaybackCompleted {
            item_id: Some("item-heard".into()),
        })
        .await
        .unwrap();
    while orch.snapshot_history().await.len() < 2 {
        tokio::task::yield_now().await;
    }
    for event in ["output", "audio-unheard", "interrupt", "output-unheard"] {
        events_tx
            .unbounded_send(Ok(RealtimeFrame::text(event)))
            .unwrap();
        output_rx.recv().await.unwrap();
    }
    inputs_tx
        .send(RealtimeAgentInput::PlaybackCompleted {
            item_id: Some("item-unheard".into()),
        })
        .await
        .unwrap();
    cancel.cancel();
    assert_eq!(task.await.unwrap().unwrap(), RealtimeAgentEnd::Cancelled);
    let _ = driver.await;
    let history = orch.snapshot_history().await;
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].text_content(), "spoken input");
    assert_eq!(history[1].text_content(), "heard assistant");
}

async fn wait_until(condition: impl Fn() -> bool, label: &str) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out: {label}"));
}

#[tokio::test]
async fn absolute_deadline_closes_audio_but_waits_for_block_tool_and_persists_result() {
    let completion = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: false,
        release: None,
        approved: AtomicBool::new(false),
    });
    let (orch, calls) = fixture_with_completion(gate, Some(completion.clone())).await;
    let prepared = orch.prepare_realtime_agent().await.unwrap();
    let (control, events, driver, provider, _) = connection().await;
    let (_inputs, input_rx) = mpsc::channel(8);
    let (output, _output_rx) = mpsc::channel(8);
    let task = tokio::spawn({
        let orch = orch.clone();
        async move {
            orch.run_realtime_agent(
                prepared,
                control,
                events,
                input_rx,
                output,
                RealtimeAgentLimits {
                    max_duration: Duration::from_millis(40),
                    ..Default::default()
                },
                CancellationToken::new(),
            )
            .await
        }
    });
    provider
        .unbounded_send(Ok(RealtimeFrame::text("tool")))
        .unwrap();
    wait_until(|| calls.load(Ordering::SeqCst) == 1, "block tool starts").await;
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(
        !task.is_finished(),
        "Block tools must finish before Agent ownership releases"
    );
    completion.notify_one();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        RealtimeAgentEnd::Deadline
    );
    assert!(realtime_history(&orch.session().lock().await.history)
        .unwrap()
        .iter()
        .any(|item| matches!(item, RealtimeHistoryItem::ToolResult { .. })));
    driver.await.unwrap().unwrap();
}
struct RoutingApi(llm_runtime::ModelRuntime);
#[async_trait]
impl orchestrator::OrchestratorApiClient for RoutingApi {
    async fn messages_create(
        &self,
        _: orchestrator::OrchestratorApiRequest,
    ) -> Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> {
        Err(llm_runtime::LlmError::ModelUnavailable)
    }
    fn resolve_media_route(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<llm_runtime::MediaRoute, llm_runtime::LlmError> {
        self.0.resolve_media_route(model, profile)
    }
}
#[tokio::test]
async fn default_session_audio_uses_authoritative_registry_profile_and_unresolved_routes_fail() {
    let client = llm_runtime::ModelRuntime::from_config(llm_runtime::ClientConfig {
        providers: llm_runtime::builtin_presets().providers,
    })
    .unwrap();
    let route = client
        .available_models()
        .into_iter()
        .find(|model| model.profile_name == "openai")
        .unwrap();
    let expected = client
        .resolve_media_route(&route.request_model, None)
        .unwrap()
        .main
        .profile_name;
    let orch = ConversationOrchestrator::new(
        Default::default(),
        Arc::new(RoutingApi(client)),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(Gate {
            calls: AtomicUsize::new(0),
            deny: false,
            release: None,
            approved: AtomicBool::new(false),
        }),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        "/work/realtime".into(),
    );
    {
        let state = orch.session();
        let mut session = state.lock().await;
        session.model = route.request_model;
        session.model_profile = None;
    }
    assert_eq!(orch.current_audio_profile().await.unwrap(), expected);
    assert_eq!(
        orch.prepare_realtime_agent().await.unwrap().profile_name,
        Some(expected)
    );
    let (unresolved, _) = fixture(Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: false,
        release: None,
        approved: AtomicBool::new(false),
    }))
    .await;
    unresolved.session().lock().await.model_profile = None;
    assert!(unresolved.current_audio_profile().await.is_err());
}

#[tokio::test]
async fn separate_realtime_tool_calls_do_not_overlap_mutation_batches() {
    let completion = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: false,
        release: None,
        approved: AtomicBool::new(false),
    });
    let (orch, calls) = fixture_with_completion(gate, Some(completion.clone())).await;
    let prepared = orch.prepare_realtime_agent().await.unwrap();
    let (control, events, driver, provider, sent) = connection().await;
    let (_inputs, inputs) = mpsc::channel(8);
    let (output, mut output_rx) = mpsc::channel(8);
    let cancel = CancellationToken::new();
    let task = tokio::spawn({
        let orch = orch.clone();
        let cancel = cancel.clone();
        async move {
            orch.run_realtime_agent(
                prepared,
                control,
                events,
                inputs,
                output,
                Default::default(),
                cancel,
            )
            .await
        }
    });
    provider
        .unbounded_send(Ok(RealtimeFrame::text("tool")))
        .unwrap();
    wait_until(
        || calls.load(Ordering::SeqCst) == 1,
        "first mutation starts",
    )
    .await;
    provider
        .unbounded_send(Ok(RealtimeFrame::text("tool-second")))
        .unwrap();
    while !matches!(output_rx.recv().await.unwrap(),RealtimeEvent::ToolCall {call_id,..} if call_id=="call-second")
    {
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    completion.notify_one();
    wait_until(
        || calls.load(Ordering::SeqCst) == 2,
        "second mutation starts after first settles",
    )
    .await;
    completion.notify_one();
    wait_until(
        || sent.lock().unwrap().len() == 3,
        "two results then continuation",
    )
    .await;
    cancel.cancel();
    assert!(task.await.unwrap().is_ok());
    driver.await.unwrap().unwrap();
    let history = orch.snapshot_history().await;
    assert_eq!(
        realtime_history(&history)
            .unwrap()
            .iter()
            .filter(|item| matches!(item, RealtimeHistoryItem::ToolResult { .. }))
            .count(),
        2
    );
}
