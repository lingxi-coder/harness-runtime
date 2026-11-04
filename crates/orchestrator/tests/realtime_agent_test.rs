//! Native realtime Agent tool policy, history, cancellation and budget regressions.
use async_trait::async_trait;
use base64::Engine as _;
use futures::StreamExt;
use lingxi_core::host::session_context::ConversationMode;
use lingxi_core::host::{OrchestratorHandle as _, PermissionDecision, PermissionGate};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId, ToolUseId};
use lingxi_llm_client::realtime::*;
use orchestrator::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, StaticMemoryProvider,
};
use orchestrator::{
    realtime_history, ConversationOrchestrator, RealtimeAgentEnd, RealtimeAgentInput,
    RealtimeAgentLimits, RealtimeAgentOutput, RealtimeCompletionBoundary,
};
use serde_json::json;
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
                input: Utf16JsonProjection::from_serde_value(&json!({"x":1})).unwrap(),
                provider_id: Some("provider-call".into()),
            }],
            stop_reason: Some("tool_use".into()),
            per_turn_effort: None,
            resumable: None,
        },
        ConversationMessage::User {
            id: MessageId::new(),
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id,
                content: "result".into(),
                is_error: Some(false),
                provider_tool_use_id: Some("provider-call".into()),
                content_blocks: None,
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            api_message_override: None,
            interrupted_by_shutdown: None,
            refused_message_id: None,
            origin: None,
            turn_origin: None,
            turn_companion: None,
            usage_limit_note: None,
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

    let historical_image = vec![ConversationMessage::User {
        id: MessageId::new(),
        content: vec![ContentBlock::Image {
            source: lingxi_core::types::ImageSource::Base64 {
                media_type: "image/png".into(),
                data: VISUAL_TEST_PNG_BASE64.into(),
            },
        }],
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
        api_message_override: None,
        interrupted_by_shutdown: None,
        refused_message_id: None,
        origin: None,
        turn_origin: None,
        turn_companion: None,
        usage_limit_note: None,
    }];
    let error = realtime_history(&historical_image).unwrap_err().to_string();
    assert!(error.contains("historical image content"));
}
struct FixtureTool {
    name: &'static str,
    calls: Arc<AtomicUsize>,
    completion: Option<Arc<tokio::sync::Notify>>,
    metadata: ToolExecutionMetadata,
}
const VISUAL_TEST_PNG_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+j4YkAAAAASUVORK5CYII=";
#[async_trait]
impl Tool for FixtureTool {
    fn name(&self) -> &str {
        self.name
    }
    fn input_schema(&self) -> &Utf16JsonProjection {
        static SCHEMA: std::sync::OnceLock<Utf16JsonProjection> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            Utf16JsonProjection::from_serde_value(&json!({"type":"object","properties":{}}))
                .unwrap()
        })
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
        1024
    }
    fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
        false
    }
    fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
        false
    }
    fn execution_metadata(&self, _: &Utf16JsonProjection) -> ToolExecutionMetadata {
        self.metadata.clone()
    }
    fn catalog_execution_metadata(&self) -> ToolExecutionMetadata {
        self.metadata.clone()
    }
    async fn validate_input(
        &self,
        _: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        Ok(())
    }
    async fn check_permissions(
        &self,
        _: &Utf16JsonProjection,
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
    async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
        "fixture".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
        "fixture".into()
    }
    async fn call(
        &self,
        _: Utf16JsonProjection,
        _: ToolUseContext,
        _: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(completion) = &self.completion {
            completion.notified().await;
        }
        let (data, model_content) = if self.name == "media_capture" {
            (
                json!({
                    "type": "image",
                    "file": {"base64": VISUAL_TEST_PNG_BASE64, "type": "image/png"},
                }),
                Some("[Image attached for visual analysis.]".into()),
            )
        } else {
            (json!({"content":"completed"}), None)
        };
        Ok(ToolCallResult {
            data: Utf16JsonProjection::from_serde_value(&data).unwrap(),
            model_content,
            new_messages: vec![],
            context_modifier: None,
            mcp_meta: None,
            is_error: false,
            subagent_hooks_fired: false,
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
    async fn check(&self, _: &str, _: &Utf16JsonProjection) -> PermissionDecision {
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
async fn prepare_bound(orch: &ConversationOrchestrator) -> orchestrator::RealtimeAgentContext {
    let mut prepared = orch.prepare_realtime_agent().await.unwrap();
    assert_eq!(
        prepared.account_scope.as_deref(),
        Some(lingxi_core::host::provider_accounts::UNBOUND_ACCOUNT_SCOPE)
    );
    // Simulate the host binding the prepared resource to an explicit service.
    prepared.account_scope = Some("explicit-service.realtime-test".into());
    prepared
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
    for (name, resources) in [
        ("fixture", vec![]),
        ("media_record", vec![MediaResource::Microphone]),
        ("media_play", vec![MediaResource::Speaker]),
        ("media_capture", vec![MediaResource::Camera]),
        ("media_observe", vec![]),
    ] {
        registry.register_builtin(Arc::new(FixtureTool {
            name,
            calls: calls.clone(),
            completion: completion.clone(),
            metadata: ToolExecutionMetadata {
                exclusive_media_resources: resources,
            },
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
    ConversationOrchestrator::bind_streaming_tool_dispatch_owner(&orch);
    orch.session().lock().await.model_profile = Some("exact-profile".into());
    (orch, calls)
}
async fn visual_fixture(gate: Arc<Gate>) -> (Arc<ConversationOrchestrator>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(FixtureTool {
        name: "media_capture",
        calls: calls.clone(),
        completion: None,
        metadata: ToolExecutionMetadata {
            exclusive_media_resources: vec![MediaResource::Camera],
        },
    }));
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
        std::path::PathBuf::from("/work/realtime-visual"),
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
        if matches!(
            kind.as_str(),
            "done-cancelled" | "done-cancelled-continuation"
        ) {
            let id = if kind == "done-cancelled" {
                "response-first"
            } else {
                "response-next"
            };
            return Ok(vec![
                RealtimeEvent::TurnCompleted {
                    turn_id: Some(id.into()),
                    status: Some("cancelled".into()),
                },
                RealtimeEvent::ToolCancelled {
                    call_ids: vec!["call".into()],
                },
                RealtimeEvent::Interrupted,
                RealtimeEvent::ProviderEvent {
                    name: "response.done".into(),
                    native: json!({"type":"response.done","response":{"id":id,"status":"cancelled"}}),
                },
            ]);
        }
        if matches!(
            kind.as_str(),
            "gemini-progress" | "gemini-idle" | "gemini-missing"
        ) {
            let status = match kind.as_str() {
                "gemini-progress" => Some("IN_PROGRESS"),
                "gemini-idle" => Some("IDLE"),
                _ => None,
            };
            let mut content = json!({"turnComplete":true});
            if let Some(status) = status {
                content["interactionStatus"] = json!(status);
            }
            return Ok(vec![
                RealtimeEvent::TurnCompleted {
                    turn_id: None,
                    status: Some("completed".into()),
                },
                RealtimeEvent::ProviderEvent {
                    name: "generationComplete".into(),
                    native: json!(true),
                },
                RealtimeEvent::ProviderEvent {
                    name: "serverContent".into(),
                    native: json!({"serverContent":content}),
                },
            ]);
        }
        Ok(vec![match kind.as_str() {
            "start" => RealtimeEvent::TurnStarted {
                turn_id: Some("response-first".into()),
            },
            "start-continuation" => RealtimeEvent::TurnStarted {
                turn_id: Some("response-next".into()),
            },
            "ready" => RealtimeEvent::SessionReady,
            "done" => RealtimeEvent::TurnCompleted {
                turn_id: Some("response-first".into()),
                status: Some("completed".into()),
            },
            "done-continuation" => RealtimeEvent::TurnCompleted {
                turn_id: Some("response-next".into()),
                status: Some("completed".into()),
            },
            "text-delta" => RealtimeEvent::TextDelta {
                text: "unsolicited".into(),
                item_id: None,
                final_chunk: false,
            },
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
            "audio-committed" => RealtimeEvent::ProviderEvent {
                name: "input_audio_buffer.committed".into(),
                native: json!({"type":"input_audio_buffer.committed","item_id":"item-input"}),
            },
            "input-partial" => RealtimeEvent::Transcript {
                direction: RealtimeTranscriptDirection::Input,
                update: RealtimeTranscriptUpdate::Delta,
                text: "spoken".into(),
                item_id: Some("item-input".into()),
                turn_id: Some("turn".into()),
                final_chunk: false,
            },
            "input" => RealtimeEvent::Transcript {
                direction: RealtimeTranscriptDirection::Input,
                update: RealtimeTranscriptUpdate::Replace,
                text: "spoken input".into(),
                item_id: Some("item-input".into()),
                turn_id: Some("turn".into()),
                final_chunk: true,
            },
            "input-second" => RealtimeEvent::Transcript {
                direction: RealtimeTranscriptDirection::Input,
                update: RealtimeTranscriptUpdate::Replace,
                text: "more spoken input".into(),
                item_id: Some("item-input-second".into()),
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

struct RejectInputCodec;
impl RealtimeCodec for RejectInputCodec {
    fn capabilities(&self) -> RealtimeCapabilities {
        Codec.capabilities()
    }
    fn encode(&self, input: &RealtimeInput) -> Result<Vec<RealtimeFrame>, RealtimeError> {
        if matches!(input, RealtimeInput::CommitAudio | RealtimeInput::Text(_)) {
            return Err(RealtimeError::InvalidInput {
                message: "fixture rejected input".into(),
            });
        }
        Codec.encode(input)
    }
    fn decode(&self, frame: RealtimeFrame) -> Result<Vec<RealtimeEvent>, RealtimeError> {
        Codec.decode(frame)
    }
}

#[derive(Default)]
struct CapturedMediaInputs {
    order: std::sync::Mutex<Vec<String>>,
    images: std::sync::Mutex<Vec<(String, Vec<u8>)>>,
    tool_outputs: std::sync::Mutex<Vec<String>>,
}

struct ImageRecordingCodec(Arc<CapturedMediaInputs>);

impl RealtimeCodec for ImageRecordingCodec {
    fn capabilities(&self) -> RealtimeCapabilities {
        Codec.capabilities()
    }

    fn encode(&self, input: &RealtimeInput) -> Result<Vec<RealtimeFrame>, RealtimeError> {
        match input {
            RealtimeInput::Image { data, mime_type } => {
                self.0.order.lock().unwrap().push("image".into());
                self.0
                    .images
                    .lock()
                    .unwrap()
                    .push((mime_type.clone(), data.as_ref().to_vec()));
            }
            RealtimeInput::ToolResult { output, .. } => {
                self.0.order.lock().unwrap().push("tool_result".into());
                self.0
                    .tool_outputs
                    .lock()
                    .unwrap()
                    .push(format!("{output:?}"));
            }
            RealtimeInput::ContinueResponse => {
                self.0.order.lock().unwrap().push("continue".into());
            }
            _ => self.0.order.lock().unwrap().push("other".into()),
        }
        Ok(vec![RealtimeFrame::text(format!("{input:?}"))])
    }

    fn decode(&self, frame: RealtimeFrame) -> Result<Vec<RealtimeEvent>, RealtimeError> {
        let RealtimeFrame::Text(bytes) = frame else {
            panic!("fixture")
        };
        let kind = String::from_utf8(bytes.to_vec()).unwrap();
        Ok(vec![match kind.as_str() {
            "image-tool" => RealtimeEvent::ToolCall {
                call_id: "image-call".into(),
                name: "media_capture".into(),
                arguments: json!({}),
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

struct ObservedTransport {
    incoming: std::sync::Mutex<
        Option<futures::channel::mpsc::UnboundedReceiver<Result<RealtimeFrame, RealtimeError>>>,
    >,
    sent: Arc<std::sync::Mutex<Vec<String>>>,
    received: Arc<AtomicUsize>,
}

#[async_trait]
impl RealtimeTransport for ObservedTransport {
    async fn connect(
        &self,
        _: RealtimeConnectRequest,
    ) -> Result<RealtimeConnection, RealtimeError> {
        let received = self.received.clone();
        let incoming = self.incoming.lock().unwrap().take().unwrap();
        Ok(RealtimeConnection {
            outbound: Box::new(Sink(self.sent.clone())),
            inbound: incoming
                .inspect(move |_| {
                    received.fetch_add(1, Ordering::SeqCst);
                })
                .boxed(),
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
    connection_with_codec(Arc::new(Codec)).await
}

fn admitted_inputs() -> (
    mpsc::Sender<RealtimeAgentInput>,
    mpsc::Receiver<RealtimeAgentInput>,
) {
    let (sender, receiver) = mpsc::channel(8);
    sender
        .try_send(RealtimeAgentInput::Provider(RealtimeInput::CommitAudio))
        .unwrap();
    (sender, receiver)
}

async fn gemini_connection() -> (
    RealtimeControl,
    RealtimeEvents,
    tokio::task::JoinHandle<Result<(), RealtimeError>>,
    futures::channel::mpsc::UnboundedSender<Result<RealtimeFrame, RealtimeError>>,
    Arc<std::sync::Mutex<Vec<String>>>,
) {
    use lingxi_llm_client::providers::google::live::{GeminiLiveConfig, GeminiLiveSession};
    let (tx, rx) = futures::channel::mpsc::unbounded();
    tx.unbounded_send(Ok(RealtimeFrame::text(r#"{"setupComplete":{}}"#)))
        .unwrap();
    let sent = Arc::new(std::sync::Mutex::new(vec![]));
    let transport = Arc::new(Transport {
        incoming: std::sync::Mutex::new(Some(rx)),
        sent: sent.clone(),
    });
    let (session, driver) = GeminiLiveSession::connect(
        transport,
        RealtimeConnectRequest {
            endpoint: "wss://fixture.example/ws".into(),
            headers: vec![],
            max_frame_bytes: 4096,
        },
        GeminiLiveConfig::new("gemini-3.8-live", "fixture-account").with_agent_history(),
        RealtimeLimits::default(),
    )
    .await
    .unwrap();
    let (control, events) = session.into_realtime_parts();
    control.import_history(vec![]).unwrap();
    (control, events, tokio::spawn(driver.run()), tx, sent)
}

async fn connection_with_codec(
    codec: Arc<dyn RealtimeCodec>,
) -> (
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
        codec,
        RealtimeLimits::default(),
    )
    .await
    .unwrap();
    let (control, events) = session.into_parts();
    (control, events, tokio::spawn(driver.run()), tx, sent)
}

async fn observed_openai_connection() -> (
    RealtimeControl,
    RealtimeEvents,
    tokio::task::JoinHandle<Result<(), RealtimeError>>,
    futures::channel::mpsc::UnboundedSender<Result<RealtimeFrame, RealtimeError>>,
    Arc<std::sync::Mutex<Vec<String>>>,
    Arc<AtomicUsize>,
) {
    use lingxi_llm_client::providers::openai::realtime::{
        OpenAiRealtimeCodec, OpenAiRealtimeConfig,
    };
    let (tx, rx) = futures::channel::mpsc::unbounded();
    let sent = Arc::new(std::sync::Mutex::new(vec![]));
    let received = Arc::new(AtomicUsize::new(0));
    let transport = ObservedTransport {
        incoming: std::sync::Mutex::new(Some(rx)),
        sent: sent.clone(),
        received: received.clone(),
    };
    let codec = OpenAiRealtimeCodec::new(OpenAiRealtimeConfig {
        manual_turns: true,
        ..Default::default()
    });
    let (session, driver) = RealtimeSession::connect(
        &transport,
        RealtimeConnectRequest {
            endpoint: "wss://fixture".into(),
            headers: vec![],
            max_frame_bytes: 4096,
        },
        Arc::new(codec),
        RealtimeLimits::default(),
    )
    .await
    .unwrap();
    let (control, events) = session.into_parts();
    (
        control,
        events,
        tokio::spawn(driver.run()),
        tx,
        sent,
        received,
    )
}

#[tokio::test]
async fn unsolicited_provider_activity_never_admits_a_turn_or_executes_a_tool() {
    for kind in ["start", "tool", "audio", "text-delta", "output"] {
        let gate = Arc::new(Gate {
            calls: AtomicUsize::new(0),
            deny: false,
            release: None,
            approved: AtomicBool::new(false),
        });
        let (orch, calls) = fixture(gate).await;
        let prepared = prepare_bound(&orch).await;
        let (control, events, driver, provider, _) = connection().await;
        let (_input_tx, inputs) = mpsc::channel(4);
        let (output, mut output_rx) = mpsc::channel(16);
        provider
            .unbounded_send(Ok(RealtimeFrame::text(kind)))
            .unwrap();
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            orch.run_realtime_agent(
                prepared,
                control,
                events,
                inputs,
                output,
                Default::default(),
                false,
                CancellationToken::new(),
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("without an admitted local input"),
            "{kind}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "{kind}");
        while let Ok(event) = output_rx.try_recv() {
            assert!(
                !matches!(event, RealtimeAgentOutput::AgentResponseStarted { .. }),
                "{kind}"
            );
        }
        let _ = driver.await;
    }
}

#[tokio::test]
async fn queued_provider_tool_cannot_borrow_simultaneously_ready_local_input() {
    for input in [
        RealtimeInput::CommitAudio,
        RealtimeInput::Text("hello".into()),
    ] {
        let gate = Arc::new(Gate {
            calls: AtomicUsize::new(0),
            deny: false,
            release: None,
            approved: AtomicBool::new(false),
        });
        let (orch, calls) = fixture(gate).await;
        let prepared = prepare_bound(&orch).await;
        let (control, events, driver, provider, _sent, received) =
            observed_openai_connection().await;
        for frame in [
            json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":"call","name":"fixture","arguments":"{}"}}),
            json!({"type":"session.updated"}),
        ] {
            provider
                .unbounded_send(Ok(RealtimeFrame::text(frame.to_string())))
                .unwrap();
        }
        wait_until(
            || received.load(Ordering::SeqCst) >= 2,
            "provider tool already queued",
        )
        .await;
        let (input_tx, inputs) = mpsc::channel(4);
        input_tx
            .try_send(RealtimeAgentInput::Provider(input))
            .unwrap();
        let (output, mut output_rx) = mpsc::channel(16);
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            orch.run_realtime_agent(
                prepared,
                control,
                events,
                inputs,
                output,
                Default::default(),
                false,
                CancellationToken::new(),
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("without an admitted local input"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(orch.snapshot_history().await.is_empty());
        while let Ok(event) = output_rx.try_recv() {
            assert!(!matches!(
                event,
                RealtimeAgentOutput::AgentResponseStarted { .. }
                    | RealtimeAgentOutput::Provider(RealtimeEvent::ToolCall { .. })
            ));
        }
        let _ = driver.await;
    }
}

#[tokio::test]
async fn queued_openai_commit_ack_cannot_claim_a_new_audio_input() {
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: false,
        release: None,
        approved: AtomicBool::new(false),
    });
    let (orch, _) = fixture(gate).await;
    let prepared = prepare_bound(&orch).await;
    let (control, events, driver, provider, _, received) = observed_openai_connection().await;
    for frame in [
        json!({"type":"input_audio_buffer.committed","item_id":"ghost"}),
        json!({"type":"session.updated","session":{"id":"before"}}),
    ] {
        provider
            .unbounded_send(Ok(RealtimeFrame::text(frame.to_string())))
            .unwrap();
    }
    wait_until(
        || received.load(Ordering::SeqCst) >= 2,
        "old commit acknowledgement already queued",
    )
    .await;
    let (input_tx, inputs) = mpsc::channel(8);
    input_tx
        .try_send(RealtimeAgentInput::Provider(RealtimeInput::CommitAudio))
        .unwrap();
    let (output, mut output_rx) = mpsc::channel(32);
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
                false,
                cancel,
            )
            .await
        }
    });
    while !matches!(
        tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        RealtimeAgentOutput::AgentResponseStarted { .. }
    ) {}
    for frame in [
        json!({"type":"input_audio_buffer.committed","item_id":"actual"}),
        json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"actual","transcript":"actual"}),
        json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"ghost","transcript":"forged"}),
        json!({"type":"session.updated","session":{"id":"after"}}),
    ] {
        provider
            .unbounded_send(Ok(RealtimeFrame::text(frame.to_string())))
            .unwrap();
    }
    let mut saw_actual = false;
    loop {
        match tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
            .await
            .unwrap()
            .unwrap()
        {
            RealtimeAgentOutput::Provider(RealtimeEvent::Transcript {
                item_id: Some(id), ..
            }) if id == "ghost" => panic!("old acknowledgement claimed the new input"),
            RealtimeAgentOutput::Provider(RealtimeEvent::Transcript {
                item_id: Some(id), ..
            }) if id == "actual" => saw_actual = true,
            RealtimeAgentOutput::Provider(RealtimeEvent::ProviderEvent { native, .. })
                if native
                    .pointer("/session/id")
                    .and_then(serde_json::Value::as_str)
                    == Some("after") =>
            {
                break
            }
            _ => {}
        }
    }
    assert!(saw_actual);
    assert_eq!(
        orch.snapshot_history()
            .await
            .iter()
            .map(|message| message.text_content())
            .collect::<Vec<_>>(),
        ["actual"]
    );
    cancel.cancel();
    assert_eq!(task.await.unwrap().unwrap(), RealtimeAgentEnd::Cancelled);
    let _ = driver.await;
}

#[tokio::test]
async fn input_transcript_before_commit_is_forwarded_without_admitting_or_persisting() {
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: false,
        release: None,
        approved: AtomicBool::new(false),
    });
    let (orch, _) = fixture(gate).await;
    let prepared = prepare_bound(&orch).await;
    let (control, events, driver, provider, sent) = connection().await;
    let (input_tx, inputs) = mpsc::channel(4);
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
                Default::default(),
                false,
                cancel,
            )
            .await
        }
    });
    input_tx
        .send(RealtimeAgentInput::Provider(RealtimeInput::Audio {
            data: bytes::Bytes::from_static(&[0, 0]),
            format: RealtimeAudioFormat::Pcm16 {
                sample_rate_hz: 24_000,
            },
        }))
        .await
        .unwrap();
    wait_until(
        || {
            sent.lock()
                .unwrap()
                .iter()
                .any(|value| value.starts_with("Audio"))
        },
        "local audio sent",
    )
    .await;
    provider
        .unbounded_send(Ok(RealtimeFrame::text("input-partial")))
        .unwrap();
    assert!(matches!(
        output_rx.recv().await.unwrap(),
        RealtimeAgentOutput::Provider(RealtimeEvent::Transcript {
            direction: RealtimeTranscriptDirection::Input,
            ..
        })
    ));
    assert!(orch.snapshot_history().await.is_empty());
    assert!(output_rx.try_recv().is_err());
    input_tx
        .send(RealtimeAgentInput::Provider(RealtimeInput::CommitAudio))
        .await
        .unwrap();
    assert!(matches!(
        output_rx.recv().await.unwrap(),
        RealtimeAgentOutput::AgentResponseStarted { .. }
    ));
    cancel.cancel();
    assert_eq!(task.await.unwrap().unwrap(), RealtimeAgentEnd::Cancelled);
    let _ = driver.await;
}

#[tokio::test]
async fn realtime_output_burst_waits_for_the_consumer_and_remains_cancellable() {
    for consume in [true, false] {
        let gate = Arc::new(Gate {
            calls: AtomicUsize::new(0),
            deny: false,
            release: None,
            approved: AtomicBool::new(false),
        });
        let (orch, _) = fixture(gate).await;
        let prepared = prepare_bound(&orch).await;
        let (control, events, driver, provider, sent) = connection().await;
        let (input_tx, inputs) = mpsc::channel(8);
        let (output, mut output_rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let task = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                orch.run_realtime_agent(
                    prepared,
                    control,
                    events,
                    inputs,
                    output,
                    Default::default(),
                    false,
                    cancel,
                )
                .await
            }
        });
        input_tx
            .send(RealtimeAgentInput::Provider(RealtimeInput::Audio {
                data: bytes::Bytes::from_static(&[0, 0]),
                format: RealtimeAudioFormat::Pcm16 {
                    sample_rate_hz: 24_000,
                },
            }))
            .await
            .unwrap();
        wait_until(
            || {
                sent.lock()
                    .unwrap()
                    .iter()
                    .any(|value| value.starts_with("Audio"))
            },
            "burst audio sent",
        )
        .await;
        for _ in 0..48 {
            provider
                .unbounded_send(Ok(RealtimeFrame::text("input-partial")))
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !task.is_finished(),
            "a temporarily full output queue ended the session"
        );
        if consume {
            for _ in 0..48 {
                assert!(matches!(
                    tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
                        .await
                        .unwrap()
                        .unwrap(),
                    RealtimeAgentOutput::Provider(RealtimeEvent::Transcript {
                        direction: RealtimeTranscriptDirection::Input,
                        final_chunk: false,
                        ..
                    })
                ));
            }
            input_tx
                .send(RealtimeAgentInput::Provider(RealtimeInput::CommitAudio))
                .await
                .unwrap();
            next_agent_started(&mut output_rx).await;
        }
        cancel.cancel();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            RealtimeAgentEnd::Cancelled
        );
        let _ = tokio::time::timeout(Duration::from_secs(2), driver)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn rejected_local_input_never_emits_start_or_runs_provider_tool() {
    for input in [
        RealtimeInput::CommitAudio,
        RealtimeInput::Text("hello".into()),
    ] {
        let gate = Arc::new(Gate {
            calls: AtomicUsize::new(0),
            deny: false,
            release: None,
            approved: AtomicBool::new(false),
        });
        let (orch, calls) = fixture(gate).await;
        let prepared = prepare_bound(&orch).await;
        let (control, events, driver, _provider, _) =
            connection_with_codec(Arc::new(RejectInputCodec)).await;
        let (input_tx, inputs) = mpsc::channel(4);
        let (output, mut output_rx) = mpsc::channel(16);
        input_tx
            .try_send(RealtimeAgentInput::Provider(input))
            .unwrap();
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            orch.run_realtime_agent(
                prepared,
                control,
                events,
                inputs,
                output,
                Default::default(),
                false,
                CancellationToken::new(),
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(error.to_string().contains("fixture rejected input"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(orch.snapshot_history().await.is_empty());
        while let Ok(event) = output_rx.try_recv() {
            assert!(!matches!(
                event,
                RealtimeAgentOutput::AgentResponseStarted { .. }
            ));
        }
        let _ = driver.await;
    }
}

#[tokio::test]
async fn admitted_audio_and_text_precede_queued_provider_work_and_keep_one_id() {
    for input in [
        RealtimeInput::CommitAudio,
        RealtimeInput::Text("hello".into()),
    ] {
        let gate = Arc::new(Gate {
            calls: AtomicUsize::new(0),
            deny: false,
            release: None,
            approved: AtomicBool::new(false),
        });
        let (orch, calls) = fixture(gate).await;
        let prepared = prepare_bound(&orch).await;
        let (control, events, driver, provider, sent) = connection().await;
        let (input_tx, inputs) = mpsc::channel(4);
        let (output, mut output_rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();
        input_tx
            .try_send(RealtimeAgentInput::Provider(input))
            .unwrap();
        let task = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                orch.run_realtime_agent(
                    prepared,
                    control,
                    events,
                    inputs,
                    output,
                    Default::default(),
                    false,
                    cancel,
                )
                .await
            }
        });
        let RealtimeAgentOutput::AgentResponseStarted { turn_id: id } =
            output_rx.recv().await.unwrap()
        else {
            panic!("local admission must precede provider work")
        };
        for kind in ["start", "tool"] {
            provider
                .unbounded_send(Ok(RealtimeFrame::text(kind)))
                .unwrap();
        }
        wait_until(
            || {
                calls.load(Ordering::SeqCst) == 1
                    && sent
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|value| value == "ContinueResponse")
            },
            "admitted tool continuation",
        )
        .await;
        for kind in ["done", "start-continuation", "done-continuation"] {
            provider
                .unbounded_send(Ok(RealtimeFrame::text(kind)))
                .unwrap();
        }
        loop {
            match tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                RealtimeAgentOutput::AgentResponseStarted { .. } => {
                    panic!("continuation allocated a second local turn")
                }
                RealtimeAgentOutput::AgentTurnCompleted { turn_id } => {
                    assert_eq!(turn_id, id);
                    break;
                }
                _ => {}
            }
        }
        cancel.cancel();
        assert_eq!(task.await.unwrap().unwrap(), RealtimeAgentEnd::Cancelled);
        let _ = driver.await;
    }
}

#[tokio::test]
async fn late_and_fragmented_audio_transcripts_survive_terminal_without_a_new_turn() {
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: false,
        release: None,
        approved: AtomicBool::new(false),
    });
    let (orch, _) = fixture(gate).await;
    let prepared = prepare_bound(&orch).await;
    let (control, events, driver, provider, _) = connection().await;
    let (_input_tx, inputs) = admitted_inputs();
    let (output, mut output_rx) = mpsc::channel(32);
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
                false,
                cancel,
            )
            .await
        }
    });
    while !matches!(
        tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        RealtimeAgentOutput::AgentResponseStarted { .. }
    ) {}
    for kind in ["audio-committed", "input-partial", "start", "done"] {
        provider
            .unbounded_send(Ok(RealtimeFrame::text(kind)))
            .unwrap();
    }
    loop {
        if matches!(
            output_rx.recv().await.unwrap(),
            RealtimeAgentOutput::AgentTurnCompleted { .. }
        ) {
            break;
        }
    }
    for kind in ["input", "input"] {
        provider
            .unbounded_send(Ok(RealtimeFrame::text(kind)))
            .unwrap();
        loop {
            match output_rx.recv().await.unwrap() {
                RealtimeAgentOutput::AgentResponseStarted { .. } => {
                    panic!("late transcript reopened a turn")
                }
                RealtimeAgentOutput::Provider(RealtimeEvent::Transcript {
                    direction: RealtimeTranscriptDirection::Input,
                    ..
                }) => break,
                _ => {}
            }
        }
    }
    provider
        .unbounded_send(Ok(RealtimeFrame::text("input-second")))
        .unwrap();
    provider
        .unbounded_send(Ok(RealtimeFrame::text("ready")))
        .unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
            .await
            .unwrap()
            .unwrap()
        {
            RealtimeAgentOutput::Provider(RealtimeEvent::Transcript {
                item_id: Some(id), ..
            }) if id == "item-input-second" => panic!("uncommitted input reached the host"),
            RealtimeAgentOutput::Provider(RealtimeEvent::SessionReady) => break,
            _ => {}
        }
    }
    let history = orch.snapshot_history().await;
    assert_eq!(
        history
            .iter()
            .map(|message| message.text_content())
            .collect::<Vec<_>>(),
        ["spoken input"]
    );
    cancel.cancel();
    assert_eq!(task.await.unwrap().unwrap(), RealtimeAgentEnd::Cancelled);
    let _ = driver.await;
}

#[tokio::test]
async fn openai_transcripts_require_a_locally_committed_audio_item() {
    use lingxi_llm_client::providers::openai::realtime::{
        OpenAiRealtimeCodec, OpenAiRealtimeConfig,
    };
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: false,
        release: None,
        approved: AtomicBool::new(false),
    });
    let (orch, _) = fixture(gate).await;
    let prepared = prepare_bound(&orch).await;
    let codec = OpenAiRealtimeCodec::new(OpenAiRealtimeConfig {
        manual_turns: true,
        ..Default::default()
    });
    let (control, events, driver, provider, _) = connection_with_codec(Arc::new(codec)).await;
    let (input_tx, inputs) = mpsc::channel(8);
    let (output, mut output_rx) = mpsc::channel(64);
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
                false,
                cancel,
            )
            .await
        }
    });

    input_tx
        .send(RealtimeAgentInput::Provider(RealtimeInput::CommitAudio))
        .await
        .unwrap();
    while !matches!(
        tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        RealtimeAgentOutput::AgentResponseStarted { .. }
    ) {}
    // A final can race its commit acknowledgement. It stays out of history
    // until that exact item receives the acknowledgement for a local commit.
    provider.unbounded_send(Ok(RealtimeFrame::text(json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"audio-a","transcript":"first"}).to_string()))).unwrap();
    provider
        .unbounded_send(Ok(RealtimeFrame::text(
            json!({"type":"session.updated"}).to_string(),
        )))
        .unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
            .await
            .unwrap()
            .unwrap()
        {
            RealtimeAgentOutput::Provider(RealtimeEvent::Transcript {
                final_chunk: true, ..
            }) => panic!("unconfirmed final reached the host"),
            RealtimeAgentOutput::Provider(RealtimeEvent::SessionReady) => break,
            _ => {}
        }
    }
    assert!(orch.snapshot_history().await.is_empty());
    provider
        .unbounded_send(Ok(RealtimeFrame::text(
            json!({"type":"input_audio_buffer.committed","item_id":"audio-a"}).to_string(),
        )))
        .unwrap();
    let mut saw_confirmed_final = false;
    loop {
        match tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
            .await
            .unwrap()
            .unwrap()
        {
            RealtimeAgentOutput::Provider(RealtimeEvent::Transcript {
                item_id: Some(id),
                final_chunk: true,
                ..
            }) if id == "audio-a" => saw_confirmed_final = true,
            RealtimeAgentOutput::Provider(RealtimeEvent::ProviderEvent { ref name, .. })
                if name == "input_audio_buffer.committed" =>
            {
                break
            }
            _ => {}
        }
    }
    assert!(
        saw_confirmed_final,
        "a matching acknowledgement must release the buffered final"
    );
    assert_eq!(orch.snapshot_history().await[0].text_content(), "first");
    for frame in [
        json!({"type":"response.created","response":{"id":"response-a"}}),
        json!({"type":"response.done","response":{"id":"response-a","status":"completed"}}),
    ] {
        provider
            .unbounded_send(Ok(RealtimeFrame::text(frame.to_string())))
            .unwrap();
    }
    while !matches!(
        tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        RealtimeAgentOutput::AgentTurnCompleted { .. }
    ) {}

    input_tx
        .send(RealtimeAgentInput::Provider(RealtimeInput::CommitAudio))
        .await
        .unwrap();
    while !matches!(
        tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        RealtimeAgentOutput::AgentResponseStarted { .. }
    ) {}
    for frame in [
        json!({"type":"input_audio_buffer.committed","item_id":"audio-a"}),
        json!({"type":"input_audio_buffer.committed","item_id":"audio-b"}),
        json!({"type":"response.created","response":{"id":"response-b"}}),
        json!({"type":"response.done","response":{"id":"response-b","status":"completed"}}),
    ] {
        provider
            .unbounded_send(Ok(RealtimeFrame::text(frame.to_string())))
            .unwrap();
    }
    while !matches!(
        tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        RealtimeAgentOutput::AgentTurnCompleted { .. }
    ) {}
    for (id, text) in [("audio-b", "second"), ("audio-b", "second")] {
        provider.unbounded_send(Ok(RealtimeFrame::text(json!({"type":"conversation.item.input_audio_transcription.completed","item_id":id,"transcript":text}).to_string()))).unwrap();
        while !matches!(
            tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
                .await
                .unwrap()
                .unwrap(),
            RealtimeAgentOutput::Provider(RealtimeEvent::Transcript {
                final_chunk: true,
                ..
            })
        ) {}
    }
    provider.unbounded_send(Ok(RealtimeFrame::text(json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"uncommitted","transcript":"forged"}).to_string()))).unwrap();
    provider
        .unbounded_send(Ok(RealtimeFrame::text(
            json!({"type":"session.updated"}).to_string(),
        )))
        .unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
            .await
            .unwrap()
            .unwrap()
        {
            RealtimeAgentOutput::Provider(RealtimeEvent::Transcript {
                item_id: Some(id), ..
            }) if id == "uncommitted" => panic!("uncommitted final reached the host"),
            RealtimeAgentOutput::Provider(RealtimeEvent::SessionReady) => break,
            _ => {}
        }
    }
    assert_eq!(
        orch.snapshot_history()
            .await
            .iter()
            .map(|message| message.text_content())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    input_tx
        .send(RealtimeAgentInput::Provider(RealtimeInput::Text(
            "typed".into(),
        )))
        .await
        .unwrap();
    while !matches!(
        tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        RealtimeAgentOutput::AgentResponseStarted { .. }
    ) {}
    provider.unbounded_send(Ok(RealtimeFrame::text(json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"text-turn-forgery","transcript":"forged"}).to_string()))).unwrap();
    provider
        .unbounded_send(Ok(RealtimeFrame::text(
            json!({"type":"session.updated"}).to_string(),
        )))
        .unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
            .await
            .unwrap()
            .unwrap()
        {
            RealtimeAgentOutput::Provider(RealtimeEvent::Transcript {
                item_id: Some(id), ..
            }) if id == "text-turn-forgery" => {
                panic!("text-only turn admitted audio transcription")
            }
            RealtimeAgentOutput::Provider(RealtimeEvent::SessionReady) => break,
            _ => {}
        }
    }
    assert_eq!(
        orch.snapshot_history()
            .await
            .iter()
            .map(|message| message.text_content())
            .collect::<Vec<_>>(),
        ["first", "second", "typed"]
    );
    cancel.cancel();
    assert_eq!(task.await.unwrap().unwrap(), RealtimeAgentEnd::Cancelled);
    let _ = driver.await;
}

#[tokio::test]
async fn openai_unowned_fragment_cannot_borrow_a_later_audio_commit() {
    use lingxi_llm_client::providers::openai::realtime::{
        OpenAiRealtimeCodec, OpenAiRealtimeConfig,
    };
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: false,
        release: None,
        approved: AtomicBool::new(false),
    });
    let (orch, _) = fixture(gate).await;
    let prepared = prepare_bound(&orch).await;
    let codec = OpenAiRealtimeCodec::new(OpenAiRealtimeConfig {
        manual_turns: true,
        ..Default::default()
    });
    let (control, events, driver, provider, _) = connection_with_codec(Arc::new(codec)).await;
    let (input_tx, inputs) = mpsc::channel(8);
    let (output, mut output_rx) = mpsc::channel(64);
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
                false,
                cancel,
            )
            .await
        }
    });
    provider.unbounded_send(Ok(RealtimeFrame::text(json!({"type":"conversation.item.input_audio_transcription.delta","item_id":"poisoned","delta":"forged "}).to_string()))).unwrap();
    provider
        .unbounded_send(Ok(RealtimeFrame::text(
            json!({"type":"session.updated"}).to_string(),
        )))
        .unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
            .await
            .unwrap()
            .unwrap()
        {
            RealtimeAgentOutput::Provider(RealtimeEvent::Transcript { .. }) => {
                panic!("unowned input reached the host")
            }
            RealtimeAgentOutput::Provider(RealtimeEvent::SessionReady) => break,
            _ => {}
        }
    }
    input_tx
        .send(RealtimeAgentInput::Provider(RealtimeInput::CommitAudio))
        .await
        .unwrap();
    while !matches!(
        tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        RealtimeAgentOutput::AgentResponseStarted { .. }
    ) {}
    for frame in [
        json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"poisoned","transcript":"forged real"}),
        json!({"type":"input_audio_buffer.committed","item_id":"poisoned"}),
        json!({"type":"session.updated"}),
    ] {
        provider
            .unbounded_send(Ok(RealtimeFrame::text(frame.to_string())))
            .unwrap();
    }
    loop {
        match tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
            .await
            .unwrap()
            .unwrap()
        {
            RealtimeAgentOutput::Provider(RealtimeEvent::Transcript { .. }) => {
                panic!("tainted input borrowed a later commit")
            }
            RealtimeAgentOutput::Provider(RealtimeEvent::SessionReady) => break,
            _ => {}
        }
    }
    assert!(orch.snapshot_history().await.is_empty());
    cancel.cancel();
    assert_eq!(task.await.unwrap().unwrap(), RealtimeAgentEnd::Cancelled);
    let _ = driver.await;
}

#[tokio::test]
async fn gemini_transcripts_require_the_admitted_audio_turn() {
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: false,
        release: None,
        approved: AtomicBool::new(false),
    });
    let (orch, _) = fixture(gate).await;
    let prepared = prepare_bound(&orch).await;
    let (control, events, driver, provider, _) = gemini_connection().await;
    let (input_tx, inputs) = mpsc::channel(8);
    let (output, mut output_rx) = mpsc::channel(64);
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
                    completion_boundary: RealtimeCompletionBoundary::Gemini38,
                    ..Default::default()
                },
                false,
                cancel,
            )
            .await
        }
    });
    input_tx
        .send(RealtimeAgentInput::Provider(RealtimeInput::CommitAudio))
        .await
        .unwrap();
    while !matches!(
        tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        RealtimeAgentOutput::AgentResponseStarted { .. }
    ) {}
    for frame in [
        json!({"serverContent":{"inputTranscription":{"text":"first "}}}),
        json!({"serverContent":{"inputTranscription":{"text":"audio"},"turnComplete":true,"interactionStatus":"IDLE"}}),
    ] {
        provider
            .unbounded_send(Ok(RealtimeFrame::text(frame.to_string())))
            .unwrap();
    }
    while !matches!(
        tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        RealtimeAgentOutput::AgentTurnCompleted { .. }
    ) {}
    assert_eq!(
        orch.snapshot_history().await[0].text_content(),
        "first audio"
    );
    provider.unbounded_send(Ok(RealtimeFrame::text(json!({"serverContent":{"inputTranscription":{"text":"unadmitted"},"turnComplete":true,"interactionStatus":"IDLE"}}).to_string()))).unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
            .await
            .unwrap()
            .unwrap()
        {
            RealtimeAgentOutput::Provider(RealtimeEvent::Transcript { ref text, .. })
                if text == "unadmitted" =>
            {
                panic!("unadmitted transcript reached the host")
            }
            RealtimeAgentOutput::Provider(RealtimeEvent::ProviderEvent { native, .. })
                if native
                    .pointer("/serverContent/inputTranscription/text")
                    .and_then(serde_json::Value::as_str)
                    == Some("unadmitted") =>
            {
                break
            }
            _ => {}
        }
    }
    assert_eq!(
        orch.snapshot_history()
            .await
            .iter()
            .map(|message| message.text_content())
            .collect::<Vec<_>>(),
        ["first audio"]
    );
    cancel.cancel();
    assert_eq!(task.await.unwrap().unwrap(), RealtimeAgentEnd::Cancelled);
    let _ = driver.await;
}

#[tokio::test]
async fn gemini_precommit_fragments_need_locally_streamed_audio() {
    for poisoned in [false, true] {
        let gate = Arc::new(Gate {
            calls: AtomicUsize::new(0),
            deny: false,
            release: None,
            approved: AtomicBool::new(false),
        });
        let (orch, _) = fixture(gate).await;
        let prepared = prepare_bound(&orch).await;
        let (control, events, driver, provider, sent) = gemini_connection().await;
        let (input_tx, inputs) = mpsc::channel(8);
        let (output, mut output_rx) = mpsc::channel(64);
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
                        completion_boundary: RealtimeCompletionBoundary::Gemini38,
                        ..Default::default()
                    },
                    false,
                    cancel,
                )
                .await
            }
        });
        if poisoned {
            provider
                .unbounded_send(Ok(RealtimeFrame::text(
                    json!({"serverContent":{"inputTranscription":{"text":"forged "}}}).to_string(),
                )))
                .unwrap();
            loop {
                match tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
                    .await
                    .unwrap()
                    .unwrap()
                {
                    RealtimeAgentOutput::Provider(RealtimeEvent::Transcript { .. }) => {
                        panic!("unowned fragment reached the host")
                    }
                    RealtimeAgentOutput::Provider(RealtimeEvent::ProviderEvent {
                        native, ..
                    }) if native
                        .pointer("/serverContent/inputTranscription/text")
                        .and_then(serde_json::Value::as_str)
                        == Some("forged ") =>
                    {
                        break
                    }
                    _ => {}
                }
            }
        }
        input_tx
            .send(RealtimeAgentInput::Provider(RealtimeInput::Audio {
                data: bytes::Bytes::from_static(&[0, 0]),
                format: RealtimeAudioFormat::Pcm16 {
                    sample_rate_hz: 16_000,
                },
            }))
            .await
            .unwrap();
        wait_until(
            || {
                sent.lock()
                    .unwrap()
                    .iter()
                    .any(|value| value.contains("\"audio\""))
            },
            "Gemini local audio sent",
        )
        .await;
        provider
            .unbounded_send(Ok(RealtimeFrame::text(
                json!({"serverContent":{"inputTranscription":{"text":"spoken "}}}).to_string(),
            )))
            .unwrap();
        loop {
            match tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                RealtimeAgentOutput::Provider(RealtimeEvent::Transcript { ref text, .. })
                    if text == "spoken " =>
                {
                    assert!(!poisoned, "tainted preview reached the host");
                    break;
                }
                RealtimeAgentOutput::Provider(RealtimeEvent::ProviderEvent { native, .. })
                    if poisoned
                        && native
                            .pointer("/serverContent/inputTranscription/text")
                            .and_then(serde_json::Value::as_str)
                            == Some("spoken ") =>
                {
                    break
                }
                _ => {}
            }
        }
        assert!(orch.snapshot_history().await.is_empty());
        input_tx
            .send(RealtimeAgentInput::Provider(RealtimeInput::CommitAudio))
            .await
            .unwrap();
        while !matches!(
            tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
                .await
                .unwrap()
                .unwrap(),
            RealtimeAgentOutput::AgentResponseStarted { .. }
        ) {}
        provider.unbounded_send(Ok(RealtimeFrame::text(json!({"serverContent":{"inputTranscription":{"text":"audio"},"turnComplete":true,"interactionStatus":"IDLE"}}).to_string()))).unwrap();
        while !matches!(
            tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
                .await
                .unwrap()
                .unwrap(),
            RealtimeAgentOutput::AgentTurnCompleted { .. }
        ) {}
        let history = orch.snapshot_history().await;
        if poisoned {
            assert!(
                history.is_empty(),
                "unowned interim text contaminated an admitted transcript"
            );
        } else {
            assert_eq!(
                history
                    .iter()
                    .map(|message| message.text_content())
                    .collect::<Vec<_>>(),
                ["spoken audio"]
            );
        }
        cancel.cancel();
        assert_eq!(task.await.unwrap().unwrap(), RealtimeAgentEnd::Cancelled);
        let _ = driver.await;
    }
}

#[tokio::test]
async fn queued_gemini_fragment_cannot_borrow_simultaneously_ready_audio() {
    use lingxi_llm_client::providers::google::live::{GeminiLiveConfig, GeminiLiveSession};
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: false,
        release: None,
        approved: AtomicBool::new(false),
    });
    let (orch, _) = fixture(gate).await;
    let prepared = prepare_bound(&orch).await;
    let (provider, rx) = futures::channel::mpsc::unbounded();
    provider
        .unbounded_send(Ok(RealtimeFrame::text(r#"{"setupComplete":{}}"#)))
        .unwrap();
    let received = Arc::new(AtomicUsize::new(0));
    let transport = Arc::new(ObservedTransport {
        incoming: std::sync::Mutex::new(Some(rx)),
        sent: Arc::new(std::sync::Mutex::new(vec![])),
        received: received.clone(),
    });
    let (session, driver) = GeminiLiveSession::connect(
        transport,
        RealtimeConnectRequest {
            endpoint: "wss://fixture.example/ws".into(),
            headers: vec![],
            max_frame_bytes: 4096,
        },
        GeminiLiveConfig::new("gemini-3.8-live", "fixture-account").with_agent_history(),
        RealtimeLimits::default(),
    )
    .await
    .unwrap();
    let (control, events) = session.into_realtime_parts();
    control.import_history(vec![]).unwrap();
    let driver = tokio::spawn(driver.run());
    provider
        .unbounded_send(Ok(RealtimeFrame::text(
            json!({"serverContent":{"inputTranscription":{"text":"forged "}}}).to_string(),
        )))
        .unwrap();
    // Seeing the next frame pulled proves the prior frame was decoded and its
    // normalized events were enqueued before either local input is admitted.
    provider
        .unbounded_send(Ok(RealtimeFrame::text(json!({"goAway":{}}).to_string())))
        .unwrap();
    wait_until(
        || received.load(Ordering::SeqCst) >= 3,
        "provider transcript already queued",
    )
    .await;

    let (input_tx, inputs) = mpsc::channel(8);
    input_tx
        .try_send(RealtimeAgentInput::Provider(RealtimeInput::Audio {
            data: bytes::Bytes::from_static(&[0, 0]),
            format: RealtimeAudioFormat::Pcm16 {
                sample_rate_hz: 16_000,
            },
        }))
        .unwrap();
    input_tx
        .try_send(RealtimeAgentInput::Provider(RealtimeInput::CommitAudio))
        .unwrap();
    let (output, mut output_rx) = mpsc::channel(64);
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
                    completion_boundary: RealtimeCompletionBoundary::Gemini38,
                    ..Default::default()
                },
                false,
                cancel,
            )
            .await
        }
    });
    while !matches!(
        tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        RealtimeAgentOutput::AgentResponseStarted { .. }
    ) {}
    provider.unbounded_send(Ok(RealtimeFrame::text(json!({"serverContent":{"inputTranscription":{"text":"real"},"turnComplete":true,"interactionStatus":"IDLE"}}).to_string()))).unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
            .await
            .unwrap()
            .unwrap()
        {
            RealtimeAgentOutput::Provider(RealtimeEvent::Transcript {
                direction: RealtimeTranscriptDirection::Input,
                ..
            }) => panic!("queued unowned transcript reached the host"),
            RealtimeAgentOutput::AgentTurnCompleted { .. } => break,
            _ => {}
        }
    }
    assert!(orch.snapshot_history().await.is_empty());
    cancel.cancel();
    assert_eq!(task.await.unwrap().unwrap(), RealtimeAgentEnd::Cancelled);
    let _ = driver.await;
}

#[tokio::test]
async fn undriven_gemini_frame_precedes_local_audio_admission() {
    use lingxi_llm_client::providers::google::live::{GeminiLiveConfig, GeminiLiveSession};
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: false,
        release: None,
        approved: AtomicBool::new(false),
    });
    let (orch, _) = fixture(gate).await;
    let prepared = prepare_bound(&orch).await;
    let (provider, rx) = futures::channel::mpsc::unbounded();
    provider
        .unbounded_send(Ok(RealtimeFrame::text(r#"{"setupComplete":{}}"#)))
        .unwrap();
    provider
        .unbounded_send(Ok(RealtimeFrame::text(
            json!({"serverContent":{"inputTranscription":{"text":"forged "}}}).to_string(),
        )))
        .unwrap();
    let transport = Arc::new(Transport {
        incoming: std::sync::Mutex::new(Some(rx)),
        sent: Arc::new(std::sync::Mutex::new(vec![])),
    });
    let (session, driver) = GeminiLiveSession::connect(
        transport,
        RealtimeConnectRequest {
            endpoint: "wss://fixture.example/ws".into(),
            headers: vec![],
            max_frame_bytes: 4096,
        },
        GeminiLiveConfig::new("gemini-3.8-live", "fixture-account").with_agent_history(),
        RealtimeLimits::default(),
    )
    .await
    .unwrap();
    let (control, events) = session.into_realtime_parts();
    control.import_history(vec![]).unwrap();
    let (input_tx, inputs) = mpsc::channel(8);
    input_tx
        .try_send(RealtimeAgentInput::Provider(RealtimeInput::Audio {
            data: bytes::Bytes::from_static(&[0, 0]),
            format: RealtimeAudioFormat::Pcm16 {
                sample_rate_hz: 16_000,
            },
        }))
        .unwrap();
    input_tx
        .try_send(RealtimeAgentInput::Provider(RealtimeInput::CommitAudio))
        .unwrap();
    let (output, mut output_rx) = mpsc::channel(64);
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
                    completion_boundary: RealtimeCompletionBoundary::Gemini38,
                    ..Default::default()
                },
                false,
                cancel,
            )
            .await
        }
    });
    // The audio command is accepted locally, but the driver has not yet
    // started and must order the already-received raw frame before its fence.
    wait_until(
        || input_tx.capacity() == 7,
        "audio queued before driver start",
    )
    .await;
    let driver = tokio::spawn(driver.run());
    loop {
        match tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
            .await
            .unwrap()
            .unwrap()
        {
            RealtimeAgentOutput::Provider(RealtimeEvent::Transcript {
                direction: RealtimeTranscriptDirection::Input,
                ..
            }) => panic!("pre-admission raw input preview reached the host"),
            RealtimeAgentOutput::AgentResponseStarted { .. } => break,
            _ => {}
        }
    }
    provider.unbounded_send(Ok(RealtimeFrame::text(json!({"serverContent":{"inputTranscription":{"text":"real"},"turnComplete":true,"interactionStatus":"IDLE"}}).to_string()))).unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
            .await
            .unwrap()
            .unwrap()
        {
            RealtimeAgentOutput::Provider(RealtimeEvent::Transcript {
                direction: RealtimeTranscriptDirection::Input,
                ..
            }) => panic!("pre-admission raw input reached the host"),
            RealtimeAgentOutput::AgentTurnCompleted { .. } => break,
            _ => {}
        }
    }
    assert!(orch.snapshot_history().await.is_empty());
    cancel.cancel();
    assert_eq!(task.await.unwrap().unwrap(), RealtimeAgentEnd::Cancelled);
    let _ = driver.await;
}

#[tokio::test]
async fn interrupted_gemini_audio_waits_for_the_previous_turn_to_finish() {
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: false,
        release: None,
        approved: AtomicBool::new(false),
    });
    let (orch, _) = fixture(gate).await;
    let prepared = prepare_bound(&orch).await;
    let (control, events, driver, provider, sent) = gemini_connection().await;
    let (input_tx, inputs) = mpsc::channel(8);
    let (output, mut output_rx) = mpsc::channel(64);
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
                    completion_boundary: RealtimeCompletionBoundary::Gemini38,
                    ..Default::default()
                },
                false,
                cancel,
            )
            .await
        }
    });
    input_tx
        .send(RealtimeAgentInput::Provider(RealtimeInput::CommitAudio))
        .await
        .unwrap();
    let first = loop {
        if let RealtimeAgentOutput::AgentResponseStarted { turn_id } =
            tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
                .await
                .unwrap()
                .unwrap()
        {
            break turn_id;
        }
    };
    provider
        .unbounded_send(Ok(RealtimeFrame::text(
            json!({"serverContent":{"inputTranscription":{"text":"first"}}}).to_string(),
        )))
        .unwrap();
    while !matches!(
        tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        RealtimeAgentOutput::Provider(RealtimeEvent::Transcript {
            direction: RealtimeTranscriptDirection::Input,
            ..
        })
    ) {}
    for input in [
        RealtimeInput::Interrupt,
        RealtimeInput::Audio {
            data: bytes::Bytes::from_static(&[1, 2]),
            format: RealtimeAudioFormat::Pcm16 {
                sample_rate_hz: 16_000,
            },
        },
        RealtimeInput::CommitAudio,
    ] {
        input_tx
            .try_send(RealtimeAgentInput::Provider(input))
            .unwrap();
    }
    wait_until(
        || {
            input_tx.capacity() == 8
                && sent
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|value| value.contains("activityStart"))
        },
        "interrupted audio deferred",
    )
    .await;
    assert!(
        !sent
            .lock()
            .unwrap()
            .iter()
            .any(|value| value.contains("AQI=")),
        "new audio was sent before the old turn ended"
    );
    provider.unbounded_send(Ok(RealtimeFrame::text(json!({"serverContent":{"interrupted":true,"turnComplete":true,"interactionStatus":"IDLE"}}).to_string()))).unwrap();
    let mut first_finished = false;
    loop {
        match tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
            .await
            .unwrap()
            .unwrap()
        {
            RealtimeAgentOutput::AgentTurnCompleted { turn_id } => {
                assert_eq!(turn_id, first);
                first_finished = true;
            }
            RealtimeAgentOutput::AgentResponseStarted { .. } => {
                assert!(first_finished);
                break;
            }
            _ => {}
        }
    }
    wait_until(
        || {
            sent.lock()
                .unwrap()
                .iter()
                .any(|value| value.contains("AQI="))
        },
        "deferred Gemini audio sent",
    )
    .await;
    provider.unbounded_send(Ok(RealtimeFrame::text(json!({"serverContent":{"inputTranscription":{"text":"second"},"turnComplete":true,"interactionStatus":"IDLE"}}).to_string()))).unwrap();
    while !matches!(
        tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        RealtimeAgentOutput::AgentTurnCompleted { .. }
    ) {}
    assert_eq!(
        orch.snapshot_history()
            .await
            .iter()
            .map(|message| message.text_content())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    cancel.cancel();
    assert_eq!(task.await.unwrap().unwrap(), RealtimeAgentEnd::Cancelled);
    let _ = driver.await;
}

#[tokio::test]
async fn openai_cancelled_frame_drains_before_deferred_next_input() {
    use lingxi_llm_client::providers::openai::realtime::{
        OpenAiRealtimeCodec, OpenAiRealtimeConfig,
    };
    for with_usage in [false, true] {
        let gate = Arc::new(Gate {
            calls: AtomicUsize::new(0),
            deny: false,
            release: None,
            approved: AtomicBool::new(false),
        });
        let (orch, _) = fixture(gate).await;
        let prepared = prepare_bound(&orch).await;
        let codec = OpenAiRealtimeCodec::new(OpenAiRealtimeConfig {
            manual_turns: true,
            ..Default::default()
        });
        let (control, events, driver, provider, sent) =
            connection_with_codec(Arc::new(codec)).await;
        let (input_tx, inputs) = admitted_inputs();
        let (output, mut output_rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();
        let task = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                orch.run_realtime_agent(
                    prepared,
                    control,
                    events,
                    inputs,
                    output,
                    Default::default(),
                    false,
                    cancel,
                )
                .await
            }
        });
        let first = loop {
            if let RealtimeAgentOutput::AgentResponseStarted { turn_id } =
                output_rx.recv().await.unwrap()
            {
                break turn_id;
            }
        };
        provider
            .unbounded_send(Ok(RealtimeFrame::text(
                r#"{"type":"response.created","response":{"id":"response-a"}}"#,
            )))
            .unwrap();
        loop {
            if matches!(
                output_rx.recv().await.unwrap(),
                RealtimeAgentOutput::Provider(RealtimeEvent::TurnStarted { .. })
            ) {
                break;
            }
        }
        for input in [
            RealtimeInput::Interrupt,
            RealtimeInput::CommitAudio,
            RealtimeInput::ContinueResponse,
        ] {
            input_tx
                .try_send(RealtimeAgentInput::Provider(input))
                .unwrap();
        }
        wait_until(
            || {
                input_tx.capacity() == 8
                    && sent
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|item| item.contains("response.cancel"))
            },
            "next input deferred behind response A",
        )
        .await;
        let mut done =
            json!({"type":"response.done","response":{"id":"response-a","status":"cancelled"}});
        if with_usage {
            done["response"]["usage"] =
                json!({"input_tokens":1,"output_tokens":2,"total_tokens":3});
        }
        provider
            .unbounded_send(Ok(RealtimeFrame::text(done.to_string())))
            .unwrap();
        let mut old_done = false;
        let mut saw_tool_cancelled = false;
        let mut saw_interrupted = false;
        let mut saw_raw = false;
        let second = loop {
            match tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                RealtimeAgentOutput::Provider(RealtimeEvent::ToolCancelled { .. }) => {
                    saw_tool_cancelled = true
                }
                RealtimeAgentOutput::Provider(RealtimeEvent::Interrupted) => {
                    assert!(saw_tool_cancelled);
                    saw_interrupted = true;
                }
                RealtimeAgentOutput::Provider(RealtimeEvent::ProviderEvent { name, .. })
                    if name == "response.done" =>
                {
                    assert!(saw_interrupted);
                    saw_raw = true;
                }
                RealtimeAgentOutput::AgentTurnCompleted { turn_id } => {
                    assert_eq!(turn_id, first);
                    assert!(saw_raw);
                    old_done = true;
                }
                RealtimeAgentOutput::AgentResponseStarted { turn_id } => {
                    assert!(old_done);
                    assert_ne!(turn_id, first);
                    break turn_id;
                }
                _ => {}
            }
        };
        // Duplicate A completion/cancellation must not affect the newly admitted B.
        let mut old_completed = done.clone();
        old_completed["response"]["status"] = json!("completed");
        provider
            .unbounded_send(Ok(RealtimeFrame::text(old_completed.to_string())))
            .unwrap();
        provider
            .unbounded_send(Ok(RealtimeFrame::text(done.to_string())))
            .unwrap();
        provider
            .unbounded_send(Ok(RealtimeFrame::text(
                r#"{"type":"response.created","response":{"id":"response-b"}}"#,
            )))
            .unwrap();
        provider
            .unbounded_send(Ok(RealtimeFrame::text(
                r#"{"type":"response.done","response":{"id":"response-b","status":"completed"}}"#,
            )))
            .unwrap();
        loop {
            match tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                RealtimeAgentOutput::Provider(
                    RealtimeEvent::Interrupted | RealtimeEvent::ToolCancelled { .. },
                ) => panic!("stale A cancellation reached B"),
                RealtimeAgentOutput::Provider(RealtimeEvent::TurnCompleted {
                    turn_id: Some(id),
                    ..
                }) if id == "response-a" => panic!("stale A completion reached B"),
                RealtimeAgentOutput::AgentTurnCompleted { turn_id } => {
                    assert_eq!(turn_id, second);
                    break;
                }
                _ => {}
            }
        }
        cancel.cancel();
        assert_eq!(task.await.unwrap().unwrap(), RealtimeAgentEnd::Cancelled);
        let _ = driver.await;
    }
}

#[tokio::test]
async fn provider_done_before_interrupt_waits_for_tool_drain_without_extra_cancel() {
    for gemini in [false, true] {
        let completion = Arc::new(tokio::sync::Notify::new());
        let gate = Arc::new(Gate {
            calls: AtomicUsize::new(0),
            deny: false,
            release: None,
            approved: AtomicBool::new(false),
        });
        let (orch, calls) = fixture_with_completion(gate, Some(completion.clone())).await;
        let prepared = prepare_bound(&orch).await;
        let (control, events, driver, provider, sent) = if gemini {
            gemini_connection().await
        } else {
            connection().await
        };
        let (input_tx, inputs) = admitted_inputs();
        let (output, mut output_rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();
        let limits = RealtimeAgentLimits {
            completion_boundary: if gemini {
                RealtimeCompletionBoundary::Gemini38
            } else {
                RealtimeCompletionBoundary::ExplicitResponse
            },
            ..Default::default()
        };
        let task = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                orch.run_realtime_agent(
                    prepared, control, events, inputs, output, limits, false, cancel,
                )
                .await
            }
        });
        let first = loop {
            if let RealtimeAgentOutput::AgentResponseStarted { turn_id } =
                output_rx.recv().await.unwrap()
            {
                break turn_id;
            }
        };
        if gemini {
            provider
                .unbounded_send(Ok(RealtimeFrame::text(
                    r#"{"toolCall":{"functionCalls":[{"id":"call","name":"fixture","args":{}}]}}"#,
                )))
                .unwrap();
        } else {
            for kind in ["start", "tool"] {
                provider
                    .unbounded_send(Ok(RealtimeFrame::text(kind)))
                    .unwrap();
            }
        }
        wait_until(
            || calls.load(Ordering::SeqCst) == 1,
            "tool started before provider completion",
        )
        .await;
        provider
            .unbounded_send(Ok(RealtimeFrame::text(if gemini {
                r#"{"serverContent":{"turnComplete":true,"interactionStatus":"IDLE"}}"#
            } else {
                "done"
            })))
            .unwrap();
        loop {
            match tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                RealtimeAgentOutput::Provider(RealtimeEvent::ProviderEvent { name, .. })
                    if gemini && name == "serverContent" =>
                {
                    break
                }
                RealtimeAgentOutput::Provider(RealtimeEvent::TurnCompleted { .. }) if !gemini => {
                    break
                }
                RealtimeAgentOutput::AgentTurnCompleted { .. } => {
                    panic!("tool still owns the turn")
                }
                _ => {}
            }
        }
        for input in [
            RealtimeInput::Interrupt,
            RealtimeInput::CommitAudio,
            RealtimeInput::ContinueResponse,
        ] {
            input_tx
                .try_send(RealtimeAgentInput::Provider(input))
                .unwrap();
        }
        wait_until(
            || input_tx.capacity() == 8,
            "deferred interrupted input queued",
        )
        .await;
        assert!(!sent.lock().unwrap().iter().any(|item| if gemini {
            item.contains("activityStart")
        } else {
            item == "Interrupt"
        }));
        assert!(output_rx.try_recv().is_err());
        completion.notify_one();
        let mut old_done = false;
        loop {
            match tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                RealtimeAgentOutput::AgentTurnCompleted { turn_id } => {
                    assert_eq!(turn_id, first);
                    old_done = true;
                }
                RealtimeAgentOutput::AgentResponseStarted { turn_id } => {
                    assert!(old_done);
                    assert_ne!(turn_id, first);
                    break;
                }
                _ => {}
            }
        }
        wait_until(
            || {
                let sent = sent.lock().unwrap();
                if gemini {
                    sent.iter()
                        .filter(|item| item.contains("activityEnd"))
                        .count()
                        == 2
                } else {
                    sent.iter().filter(|item| *item == "CommitAudio").count() == 2
                        && sent.iter().any(|item| item == "ContinueResponse")
                }
            },
            "new commit and continuation sent after drain",
        )
        .await;
        assert!(!sent.lock().unwrap().iter().any(|item| if gemini {
            item.contains("activityStart")
        } else {
            item == "Interrupt"
        }));
        cancel.cancel();
        assert_eq!(task.await.unwrap().unwrap(), RealtimeAgentEnd::Cancelled);
        let _ = driver.await;
    }
}

#[tokio::test]
async fn interrupt_after_continuation_request_waits_for_new_provider_completion() {
    let completion = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: false,
        release: None,
        approved: AtomicBool::new(false),
    });
    let (orch, calls) = fixture_with_completion(gate, Some(completion.clone())).await;
    let prepared = prepare_bound(&orch).await;
    let (control, events, driver, provider, sent) = connection().await;
    let (input_tx, inputs) = admitted_inputs();
    let (output, mut output_rx) = mpsc::channel(32);
    let cancel = CancellationToken::new();
    let task = tokio::spawn({
        let cancel = cancel.clone();
        async move {
            orch.run_realtime_agent(
                prepared,
                control,
                events,
                inputs,
                output,
                Default::default(),
                false,
                cancel,
            )
            .await
        }
    });
    let first = loop {
        if let RealtimeAgentOutput::AgentResponseStarted { turn_id } =
            output_rx.recv().await.unwrap()
        {
            break turn_id;
        }
    };
    for kind in ["start", "tool"] {
        provider
            .unbounded_send(Ok(RealtimeFrame::text(kind)))
            .unwrap();
    }
    wait_until(|| calls.load(Ordering::SeqCst) == 1, "tool started").await;
    provider
        .unbounded_send(Ok(RealtimeFrame::text("done")))
        .unwrap();
    loop {
        if matches!(
            output_rx.recv().await.unwrap(),
            RealtimeAgentOutput::Provider(RealtimeEvent::TurnCompleted { .. })
        ) {
            break;
        }
    }
    completion.notify_one();
    wait_until(
        || {
            sent.lock()
                .unwrap()
                .iter()
                .any(|item| item == "ContinueResponse")
        },
        "continuation requested",
    )
    .await;
    input_tx
        .try_send(RealtimeAgentInput::Provider(RealtimeInput::Interrupt))
        .unwrap();
    wait_until(
        || sent.lock().unwrap().iter().any(|item| item == "Interrupt"),
        "continuation interrupted",
    )
    .await;
    assert!(
        output_rx.try_recv().is_err(),
        "old provider completion cannot settle the requested continuation"
    );
    for kind in ["start-continuation", "done-cancelled-continuation"] {
        provider
            .unbounded_send(Ok(RealtimeFrame::text(kind)))
            .unwrap();
    }
    loop {
        if let RealtimeAgentOutput::AgentTurnCompleted { turn_id } =
            tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
                .await
                .unwrap()
                .unwrap()
        {
            assert_eq!(turn_id, first);
            break;
        }
    }
    cancel.cancel();
    assert_eq!(task.await.unwrap().unwrap(), RealtimeAgentEnd::Cancelled);
    let _ = driver.await;
}

#[tokio::test]
async fn interrupt_after_local_terminal_does_not_cancel_a_new_response() {
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: false,
        release: None,
        approved: AtomicBool::new(false),
    });
    let (orch, _) = fixture(gate).await;
    let prepared = prepare_bound(&orch).await;
    let (control, events, driver, provider, sent) = connection().await;
    let (input_tx, inputs) = admitted_inputs();
    let (output, mut output_rx) = mpsc::channel(16);
    let cancel = CancellationToken::new();
    let task = tokio::spawn({
        let cancel = cancel.clone();
        async move {
            orch.run_realtime_agent(
                prepared,
                control,
                events,
                inputs,
                output,
                Default::default(),
                false,
                cancel,
            )
            .await
        }
    });
    let admitted = next_agent_started(&mut output_rx).await;
    for kind in ["start", "done"] {
        provider
            .unbounded_send(Ok(RealtimeFrame::text(kind)))
            .unwrap();
    }
    let first = loop {
        if let RealtimeAgentOutput::AgentTurnCompleted { turn_id } = output_rx.recv().await.unwrap()
        {
            break turn_id;
        }
    };
    assert_eq!(first, admitted);
    input_tx
        .try_send(RealtimeAgentInput::Provider(RealtimeInput::Interrupt))
        .unwrap();
    input_tx
        .try_send(RealtimeAgentInput::Provider(RealtimeInput::CommitAudio))
        .unwrap();
    loop {
        if let RealtimeAgentOutput::AgentResponseStarted { turn_id } =
            output_rx.recv().await.unwrap()
        {
            assert_ne!(turn_id, first);
            break;
        }
    }
    wait_until(
        || {
            sent.lock()
                .unwrap()
                .iter()
                .filter(|item| *item == "CommitAudio")
                .count()
                == 2
        },
        "new input sent",
    )
    .await;
    assert!(!sent.lock().unwrap().iter().any(|item| item == "Interrupt"));
    cancel.cancel();
    assert_eq!(task.await.unwrap().unwrap(), RealtimeAgentEnd::Cancelled);
    let _ = driver.await;
}

#[tokio::test]
async fn interrupted_tool_drains_before_deferred_commit_and_continue_are_replayed() {
    let completion = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: false,
        release: None,
        approved: AtomicBool::new(false),
    });
    let (orch, calls) = fixture_with_completion(gate, Some(completion.clone())).await;
    let prepared = prepare_bound(&orch).await;
    let (control, events, driver, provider, sent) = connection().await;
    let (input_tx, inputs) = admitted_inputs();
    let (output, mut output_rx) = mpsc::channel(32);
    let cancel = CancellationToken::new();
    let task = tokio::spawn({
        let cancel = cancel.clone();
        async move {
            orch.run_realtime_agent(
                prepared,
                control,
                events,
                inputs,
                output,
                Default::default(),
                false,
                cancel,
            )
            .await
        }
    });
    let first = loop {
        if let RealtimeAgentOutput::AgentResponseStarted { turn_id } =
            output_rx.recv().await.unwrap()
        {
            break turn_id;
        }
    };
    for kind in ["start", "tool"] {
        provider
            .unbounded_send(Ok(RealtimeFrame::text(kind)))
            .unwrap();
    }
    wait_until(
        || calls.load(Ordering::SeqCst) == 1,
        "interruptible tool started",
    )
    .await;
    input_tx
        .try_send(RealtimeAgentInput::Provider(RealtimeInput::Interrupt))
        .unwrap();
    input_tx
        .try_send(RealtimeAgentInput::Provider(RealtimeInput::CommitAudio))
        .unwrap();
    input_tx
        .try_send(RealtimeAgentInput::Provider(
            RealtimeInput::ContinueResponse,
        ))
        .unwrap();
    provider
        .unbounded_send(Ok(RealtimeFrame::text("done-cancelled")))
        .unwrap();
    loop {
        match output_rx.recv().await.unwrap() {
            RealtimeAgentOutput::Provider(RealtimeEvent::TurnCompleted { .. }) => break,
            RealtimeAgentOutput::AgentTurnCompleted { .. }
            | RealtimeAgentOutput::AgentResponseStarted { .. } => {
                panic!("old owner is not drained")
            }
            _ => {}
        }
    }
    assert_eq!(
        sent.lock()
            .unwrap()
            .iter()
            .filter(|item| *item == "CommitAudio")
            .count(),
        1
    );
    completion.notify_one();
    let mut old_done = false;
    loop {
        match tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
            .await
            .unwrap()
            .unwrap()
        {
            RealtimeAgentOutput::AgentTurnCompleted { turn_id } => {
                assert_eq!(turn_id, first);
                old_done = true;
            }
            RealtimeAgentOutput::AgentResponseStarted { turn_id } => {
                assert!(old_done);
                assert_ne!(turn_id, first);
                break;
            }
            _ => {}
        }
    }
    wait_until(
        || {
            let sent = sent.lock().unwrap();
            sent.iter().filter(|item| *item == "CommitAudio").count() == 2
                && sent.iter().any(|item| item == "ContinueResponse")
        },
        "deferred commit and continuation replayed",
    )
    .await;
    cancel.cancel();
    assert_eq!(task.await.unwrap().unwrap(), RealtimeAgentEnd::Cancelled);
    let _ = driver.await;
}

#[tokio::test]
async fn audio_chunk_arriving_after_commit_does_not_end_the_active_turn() {
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: false,
        release: None,
        approved: AtomicBool::new(false),
    });
    let (orch, _) = fixture(gate).await;
    let prepared = prepare_bound(&orch).await;
    let (control, events, driver, provider, sent) = connection().await;
    let (input_tx, inputs) = admitted_inputs();
    let (output, mut output_rx) = mpsc::channel(32);
    let cancel = CancellationToken::new();
    let task = tokio::spawn({
        let cancel = cancel.clone();
        async move {
            orch.run_realtime_agent(
                prepared,
                control,
                events,
                inputs,
                output,
                Default::default(),
                false,
                cancel,
            )
            .await
        }
    });
    let first = loop {
        if let RealtimeAgentOutput::AgentResponseStarted { turn_id } =
            tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
                .await
                .unwrap()
                .unwrap()
        {
            break turn_id;
        }
    };
    input_tx
        .send(RealtimeAgentInput::Provider(RealtimeInput::Audio {
            data: bytes::Bytes::from_static(&[1, 2]),
            format: RealtimeAudioFormat::Pcm16 {
                sample_rate_hz: 24_000,
            },
        }))
        .await
        .unwrap();
    wait_until(|| input_tx.capacity() == 8, "late audio consumed").await;
    for kind in ["start", "done"] {
        provider
            .unbounded_send(Ok(RealtimeFrame::text(kind)))
            .unwrap();
    }
    loop {
        if let RealtimeAgentOutput::AgentTurnCompleted { turn_id } =
            tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
                .await
                .unwrap()
                .unwrap()
        {
            assert_eq!(turn_id, first);
            break;
        }
    }
    assert!(!sent
        .lock()
        .unwrap()
        .iter()
        .any(|value| value.starts_with("Audio")));
    cancel.cancel();
    assert_eq!(task.await.unwrap().unwrap(), RealtimeAgentEnd::Cancelled);
    let _ = driver.await;
}

#[tokio::test]
async fn interrupted_audio_applies_backpressure_until_the_old_tool_drains() {
    let completion = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: false,
        release: None,
        approved: AtomicBool::new(false),
    });
    let (orch, calls) = fixture_with_completion(gate, Some(completion.clone())).await;
    let prepared = prepare_bound(&orch).await;
    let (control, events, driver, provider, sent) = connection().await;
    let (input_tx, inputs) = mpsc::channel(8);
    input_tx
        .try_send(RealtimeAgentInput::Provider(RealtimeInput::CommitAudio))
        .unwrap();
    let (output, mut output_rx) = mpsc::channel(64);
    let cancel = CancellationToken::new();
    let task = tokio::spawn({
        let cancel = cancel.clone();
        async move {
            orch.run_realtime_agent(
                prepared,
                control,
                events,
                inputs,
                output,
                Default::default(),
                false,
                cancel,
            )
            .await
        }
    });
    let first = loop {
        if let RealtimeAgentOutput::AgentResponseStarted { turn_id } =
            tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
                .await
                .unwrap()
                .unwrap()
        {
            break turn_id;
        }
    };
    for kind in ["start", "tool"] {
        provider
            .unbounded_send(Ok(RealtimeFrame::text(kind)))
            .unwrap();
    }
    wait_until(|| calls.load(Ordering::SeqCst) == 1, "old tool started").await;
    input_tx
        .send(RealtimeAgentInput::Provider(RealtimeInput::Interrupt))
        .await
        .unwrap();
    let sent_chunks = Arc::new(AtomicUsize::new(0));
    let producer = tokio::spawn({
        let tx = input_tx.clone();
        let sent_chunks = sent_chunks.clone();
        async move {
            for _ in 0..40 {
                tx.send(RealtimeAgentInput::Provider(RealtimeInput::Audio {
                    data: bytes::Bytes::from(vec![0; 64 * 1024]),
                    format: RealtimeAudioFormat::Pcm16 {
                        sample_rate_hz: 24_000,
                    },
                }))
                .await
                .unwrap();
                sent_chunks.fetch_add(1, Ordering::SeqCst);
            }
            tx.send(RealtimeAgentInput::Provider(RealtimeInput::CommitAudio))
                .await
                .unwrap();
        }
    });
    provider
        .unbounded_send(Ok(RealtimeFrame::text("done-cancelled")))
        .unwrap();
    wait_until(
        || sent_chunks.load(Ordering::SeqCst) >= 20,
        "deferred audio reached its high-water mark",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        sent_chunks.load(Ordering::SeqCst) < 40,
        "producer bypassed bounded backpressure"
    );
    assert!(
        !sent
            .lock()
            .unwrap()
            .iter()
            .any(|value| value.starts_with("Audio")),
        "new audio reached the provider while the old tool was active"
    );
    completion.notify_one();
    let mut first_finished = false;
    loop {
        match tokio::time::timeout(Duration::from_secs(5), output_rx.recv())
            .await
            .unwrap()
            .unwrap()
        {
            RealtimeAgentOutput::AgentTurnCompleted { turn_id } => {
                assert_eq!(turn_id, first);
                first_finished = true;
            }
            RealtimeAgentOutput::AgentResponseStarted { .. } => {
                assert!(first_finished);
                break;
            }
            _ => {}
        }
    }
    producer.await.unwrap();
    assert_eq!(
        sent.lock()
            .unwrap()
            .iter()
            .filter(|value| value.starts_with("Audio"))
            .count(),
        40
    );
    cancel.cancel();
    assert_eq!(task.await.unwrap().unwrap(), RealtimeAgentEnd::Cancelled);
    let _ = driver.await;
}

#[tokio::test]
async fn deferred_images_share_the_payload_backpressure_limit() {
    let completion = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: false,
        release: None,
        approved: AtomicBool::new(false),
    });
    let (orch, calls) = fixture_with_completion(gate, Some(completion.clone())).await;
    let prepared = prepare_bound(&orch).await;
    let (control, events, driver, provider, sent) = connection().await;
    let (input_tx, inputs) = mpsc::channel(8);
    input_tx
        .try_send(RealtimeAgentInput::Provider(RealtimeInput::CommitAudio))
        .unwrap();
    let (output, mut output_rx) = mpsc::channel(64);
    let cancel = CancellationToken::new();
    let task = tokio::spawn({
        let cancel = cancel.clone();
        async move {
            orch.run_realtime_agent(
                prepared,
                control,
                events,
                inputs,
                output,
                Default::default(),
                true,
                cancel,
            )
            .await
        }
    });
    next_agent_started(&mut output_rx).await;
    for kind in ["start", "tool"] {
        provider
            .unbounded_send(Ok(RealtimeFrame::text(kind)))
            .unwrap();
    }
    wait_until(|| calls.load(Ordering::SeqCst) == 1, "old tool started").await;
    input_tx
        .send(RealtimeAgentInput::Provider(RealtimeInput::Interrupt))
        .await
        .unwrap();
    input_tx
        .send(RealtimeAgentInput::Provider(RealtimeInput::Audio {
            data: bytes::Bytes::from_static(&[1, 2]),
            format: RealtimeAudioFormat::Pcm16 {
                sample_rate_hz: 24_000,
            },
        }))
        .await
        .unwrap();
    let mut png = base64::engine::general_purpose::STANDARD
        .decode(VISUAL_TEST_PNG_BASE64)
        .unwrap();
    png.resize(600 * 1024, 0);
    let png = bytes::Bytes::from(png);
    let sent_images = Arc::new(AtomicUsize::new(0));
    let producer = tokio::spawn({
        let tx = input_tx.clone();
        let sent_images = sent_images.clone();
        async move {
            for _ in 0..20 {
                if tx
                    .send(RealtimeAgentInput::Provider(RealtimeInput::Image {
                        data: png.clone(),
                        mime_type: "image/png".into(),
                    }))
                    .await
                    .is_err()
                {
                    break;
                }
                sent_images.fetch_add(1, Ordering::SeqCst);
            }
        }
    });
    wait_until(
        || sent_images.load(Ordering::SeqCst) >= 9,
        "deferred image producer reached backpressure",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        sent_images.load(Ordering::SeqCst) < 20,
        "images bypassed the deferred payload bound"
    );
    assert!(!sent
        .lock()
        .unwrap()
        .iter()
        .any(|value| value.starts_with("Image")));
    cancel.cancel();
    completion.notify_one();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        RealtimeAgentEnd::Cancelled
    );
    producer.await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), driver)
        .await
        .unwrap();
}

#[tokio::test]
async fn deferred_images_count_toward_the_session_limit_before_replay() {
    let completion = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: false,
        release: None,
        approved: AtomicBool::new(false),
    });
    let (orch, calls) = fixture_with_completion(gate, Some(completion.clone())).await;
    let prepared = prepare_bound(&orch).await;
    let (control, events, driver, provider, sent) = connection().await;
    let (input_tx, inputs) = mpsc::channel(8);
    input_tx
        .try_send(RealtimeAgentInput::Provider(RealtimeInput::CommitAudio))
        .unwrap();
    let (output, mut output_rx) = mpsc::channel(64);
    let png = bytes::Bytes::from(
        base64::engine::general_purpose::STANDARD
            .decode(VISUAL_TEST_PNG_BASE64)
            .unwrap(),
    );
    let limits = RealtimeAgentLimits {
        max_image_bytes_per_session: png.len() + 1,
        ..Default::default()
    };
    let task = tokio::spawn(async move {
        orch.run_realtime_agent(
            prepared,
            control,
            events,
            inputs,
            output,
            limits,
            true,
            CancellationToken::new(),
        )
        .await
    });
    next_agent_started(&mut output_rx).await;
    for kind in ["start", "tool"] {
        provider
            .unbounded_send(Ok(RealtimeFrame::text(kind)))
            .unwrap();
    }
    wait_until(|| calls.load(Ordering::SeqCst) == 1, "old tool started").await;
    input_tx
        .send(RealtimeAgentInput::Provider(RealtimeInput::Interrupt))
        .await
        .unwrap();
    input_tx
        .send(RealtimeAgentInput::Provider(RealtimeInput::Audio {
            data: bytes::Bytes::from_static(&[1, 2]),
            format: RealtimeAudioFormat::Pcm16 {
                sample_rate_hz: 24_000,
            },
        }))
        .await
        .unwrap();
    for _ in 0..2 {
        input_tx
            .send(RealtimeAgentInput::Provider(RealtimeInput::Image {
                data: png.clone(),
                mime_type: "image/png".into(),
            }))
            .await
            .unwrap();
    }
    completion.notify_one();
    let error = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("image session byte limit"));
    assert!(!sent
        .lock()
        .unwrap()
        .iter()
        .any(|value| value.starts_with("Image")));
    let _ = tokio::time::timeout(Duration::from_secs(2), driver)
        .await
        .unwrap();
}

#[tokio::test]
async fn realtime_rejects_unbound_and_foreign_account_contexts() {
    for scenario in ["unbound", "foreign-account", "session-changed"] {
        let gate = Arc::new(Gate {
            calls: AtomicUsize::new(0),
            deny: false,
            release: None,
            approved: AtomicBool::new(false),
        });
        let (orch, calls) = fixture(gate).await;
        let mut prepared = orch.prepare_realtime_agent().await.unwrap();
        assert_eq!(
            prepared.account_scope.as_deref(),
            Some(lingxi_core::host::provider_accounts::UNBOUND_ACCOUNT_SCOPE)
        );
        if scenario != "unbound" {
            prepared.account_scope = Some("explicit-service.realtime-test".into());
        }
        if scenario == "foreign-account" {
            prepared.profile_name = Some("foreign-profile".into());
        }
        if scenario == "session-changed" {
            orch.session().lock().await.model_profile = Some("new-session-profile".into());
        }
        let (control, events, driver, _provider, _) = connection().await;
        let (_inputs_tx, inputs) = admitted_inputs();
        let (output, _output_rx) = mpsc::channel(4);
        let error = orch
            .run_realtime_agent(
                prepared,
                control,
                events,
                inputs,
                output,
                RealtimeAgentLimits::default(),
                false,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("provider profile changed"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let _ = tokio::time::timeout(Duration::from_secs(2), driver).await;
    }
}

#[tokio::test]
async fn explicit_audio_profile_can_differ_from_the_unchanged_session_profile() {
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: false,
        release: None,
        approved: AtomicBool::new(false),
    });
    let (orch, _) = fixture(gate).await;
    let mut prepared = prepare_bound(&orch).await;
    assert_eq!(prepared.session_profile_name, "exact-profile");
    prepared.audio_profile_name = "audio-provider".into();
    prepared.profile_name = Some("audio-provider".into());
    let (control, events, driver, _provider, _) = connection().await;
    let (_input, inputs) = admitted_inputs();
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
                RealtimeAgentLimits::default(),
                false,
                cancel,
            )
            .await
        }
    });
    next_agent_started(&mut output_rx).await;
    cancel.cancel();
    assert!(tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .is_ok());
    let _ = tokio::time::timeout(Duration::from_secs(2), driver).await;
}

#[tokio::test]
async fn realtime_rejects_context_for_another_fixed_mode() {
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: false,
        release: None,
        approved: AtomicBool::new(false),
    });
    let (orch, calls) = fixture(gate).await;
    let mut prepared = prepare_bound(&orch).await;
    assert_eq!(prepared.conversation_mode.mode, ConversationMode::Code);
    assert!(orch
        .ensure_conversation_mode(ConversationMode::Chat)
        .await
        .is_err());
    prepared.conversation_mode.mode = ConversationMode::Chat;
    let (control, events, driver, _provider, _) = connection().await;
    let (_inputs_tx, inputs) = admitted_inputs();
    let (output, _output_rx) = mpsc::channel(4);
    let error = orch
        .run_realtime_agent(
            prepared,
            control,
            events,
            inputs,
            output,
            RealtimeAgentLimits::default(),
            false,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("conversation mode changed before realtime admission"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let _ = tokio::time::timeout(Duration::from_secs(2), driver).await;
}

#[tokio::test]
async fn realtime_running_turn_rejects_mode_change_without_interrupting_owner() {
    let release = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: false,
        release: Some(release.clone()),
        approved: AtomicBool::new(false),
    });
    let (orch, _) = fixture(gate.clone()).await;
    let prepared = prepare_bound(&orch).await;
    let (control, events, driver, provider, _) = connection().await;
    let (_inputs_tx, inputs) = admitted_inputs();
    let (output, mut output_rx) = mpsc::channel(16);
    let cancel = CancellationToken::new();
    let first = tokio::spawn({
        let orch = orch.clone();
        let prepared = prepared.clone();
        let cancel = cancel.clone();
        async move {
            orch.run_realtime_agent(
                prepared,
                control,
                events,
                inputs,
                output,
                RealtimeAgentLimits::default(),
                false,
                cancel,
            )
            .await
        }
    });
    next_agent_started(&mut output_rx).await;
    provider
        .unbounded_send(Ok(RealtimeFrame::text("tool")))
        .unwrap();
    wait_until(
        || gate.calls.load(Ordering::SeqCst) > 0,
        "previous realtime turn owns gate",
    )
    .await;
    assert!(orch
        .ensure_conversation_mode(ConversationMode::Chat)
        .await
        .is_err());
    assert_eq!(orch.conversation_mode().await.mode, ConversationMode::Code);
    cancel.cancel();
    release.notify_one();
    let _ = tokio::time::timeout(Duration::from_secs(2), first)
        .await
        .unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), driver).await;

    assert_eq!(orch.conversation_mode().await.mode, ConversationMode::Code);
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
    let prepared = prepare_bound(&orch).await;
    let (control, events, driver, events_tx, sent) = connection().await;
    let (_inputs_tx, inputs) = admitted_inputs();
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
                RealtimeAgentLimits {
                    idle_timeout: Duration::from_millis(20),
                    max_duration: Duration::from_secs(2),
                    ..Default::default()
                },
                false,
                cancel,
            )
            .await
        }
    });
    next_agent_started(&mut output_rx).await;
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
        || sent.lock().unwrap().len() >= 3,
        "tool result followed by continuation",
    )
    .await;
    assert!(sent.lock().unwrap()[1].starts_with("ToolResult"));
    assert_eq!(sent.lock().unwrap()[2], "ContinueResponse");
    cancel.cancel();
    assert_eq!(task.await.unwrap().unwrap(), RealtimeAgentEnd::Cancelled);
    let _ = driver.await;
    assert_eq!(orch.snapshot_history().await.len(), 2);
}

#[tokio::test]
async fn agent_terminal_waits_for_permission_tools_and_the_continuation() {
    // Cover both the tool waiting at provider completion and the tool having
    // finished before that completion is observed.
    for release_before_done in [false, true] {
        let release = Arc::new(tokio::sync::Notify::new());
        let gate = Arc::new(Gate {
            calls: AtomicUsize::new(0),
            deny: false,
            release: Some(release.clone()),
            approved: AtomicBool::new(false),
        });
        let (orch, _) = fixture(gate.clone()).await;
        let prepared = prepare_bound(&orch).await;
        let (control, events, driver, provider, sent) = connection().await;
        let (_inputs_tx, inputs) = admitted_inputs();
        let (output, mut output_rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();
        let task = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                orch.run_realtime_agent(
                    prepared,
                    control,
                    events,
                    inputs,
                    output,
                    Default::default(),
                    false,
                    cancel,
                )
                .await
            }
        });
        let id = Some(next_agent_started(&mut output_rx).await);
        for kind in ["start", "tool"] {
            provider
                .unbounded_send(Ok(RealtimeFrame::text(kind)))
                .unwrap();
        }
        wait_until(
            || gate.calls.load(Ordering::SeqCst) > 0,
            "permission requested",
        )
        .await;
        if release_before_done {
            release.notify_one();
            wait_until(
                || {
                    sent.lock()
                        .unwrap()
                        .iter()
                        .any(|value| value == "ContinueResponse")
                },
                "continuation requested",
            )
            .await;
        }
        provider
            .unbounded_send(Ok(RealtimeFrame::text("done")))
            .unwrap();
        loop {
            match tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                RealtimeAgentOutput::AgentResponseStarted { .. } => {
                    panic!("unexpected second admission")
                }
                RealtimeAgentOutput::AgentTurnCompleted { .. } => {
                    panic!("tool response cannot finish the Agent input")
                }
                RealtimeAgentOutput::Provider(RealtimeEvent::TurnCompleted { .. }) => break,
                _ => {}
            }
        }
        assert!(output_rx.try_recv().is_err());
        if !release_before_done {
            release.notify_one();
        }
        wait_until(
            || {
                sent.lock()
                    .unwrap()
                    .iter()
                    .any(|value| value == "ContinueResponse")
            },
            "continuation requested",
        )
        .await;
        for kind in ["start-continuation", "done-continuation"] {
            provider
                .unbounded_send(Ok(RealtimeFrame::text(kind)))
                .unwrap();
        }
        loop {
            match tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                RealtimeAgentOutput::AgentTurnCompleted { turn_id } => {
                    assert_eq!(Some(turn_id), id);
                    break;
                }
                RealtimeAgentOutput::AgentResponseStarted { .. } => {
                    panic!("continuation changed the local input identity")
                }
                _ => {}
            }
        }
        cancel.cancel();
        assert_eq!(task.await.unwrap().unwrap(), RealtimeAgentEnd::Cancelled);
        let _ = driver.await;
    }
}

#[tokio::test]
async fn agent_terminal_supports_automatic_continuation_without_provider_start() {
    let release = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: false,
        release: Some(release.clone()),
        approved: AtomicBool::new(false),
    });
    let (orch, _) = fixture(gate.clone()).await;
    let prepared = prepare_bound(&orch).await;
    let (control, events, driver, provider, sent) = gemini_connection().await;
    let (inputs_tx, inputs) = mpsc::channel(4);
    let (output, mut output_rx) = mpsc::channel(32);
    let cancel = CancellationToken::new();
    let task = tokio::spawn({
        let cancel = cancel.clone();
        async move {
            orch.run_realtime_agent(
                prepared,
                control,
                events,
                inputs,
                output,
                RealtimeAgentLimits {
                    completion_boundary: RealtimeCompletionBoundary::Gemini38,
                    ..Default::default()
                },
                false,
                cancel,
            )
            .await
        }
    });
    inputs_tx
        .send(RealtimeAgentInput::Provider(RealtimeInput::CommitAudio))
        .await
        .unwrap();
    let id = loop {
        if let RealtimeAgentOutput::AgentResponseStarted { turn_id } =
            output_rx.recv().await.unwrap()
        {
            break turn_id;
        }
    };
    provider.unbounded_send(Ok(RealtimeFrame::text(r#"{"serverContent":{"turnComplete":true,"generationComplete":true,"interactionStatus":"IN_PROGRESS"}}"#))).unwrap();
    loop {
        match output_rx.recv().await.unwrap() {
            RealtimeAgentOutput::AgentTurnCompleted { .. } => panic!("IN_PROGRESS is not final"),
            RealtimeAgentOutput::Provider(RealtimeEvent::ProviderEvent { name, .. })
                if name == "serverContent" =>
            {
                break
            }
            _ => {}
        }
    }
    provider.unbounded_send(Ok(RealtimeFrame::text(r#"{"toolCall":{"functionCalls":[{"id":"call","name":"fixture","args":{}},{"id":"call-second","name":"fixture","args":{}}]}}"#))).unwrap();
    wait_until(
        || gate.calls.load(Ordering::SeqCst) > 0,
        "Gemini permission requested",
    )
    .await;
    assert!(output_rx.try_recv().is_ok());
    release.notify_one();
    wait_until(
        || {
            sent.lock()
                .unwrap()
                .iter()
                .filter(|value| value.contains("toolResponse"))
                .count()
                == 2
        },
        "both tool results",
    )
    .await;
    provider.unbounded_send(Ok(RealtimeFrame::text(r#"{"serverContent":{"turnComplete":true,"generationComplete":true,"interactionStatus":"IDLE"}}"#))).unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
            .await
            .unwrap()
            .unwrap()
        {
            RealtimeAgentOutput::AgentResponseStarted { .. } => {
                panic!("continuation changed the local input identity")
            }
            RealtimeAgentOutput::AgentTurnCompleted { turn_id } => {
                assert_eq!(turn_id, id);
                break;
            }
            _ => {}
        }
    }
    cancel.cancel();
    assert_eq!(task.await.unwrap().unwrap(), RealtimeAgentEnd::Cancelled);
    let _ = driver.await;
}

#[tokio::test]
async fn cancelled_provider_waits_for_cancelled_tool_cleanup_before_local_terminal() {
    let release = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: false,
        release: Some(release.clone()),
        approved: AtomicBool::new(false),
    });
    let (orch, _) = fixture(gate.clone()).await;
    let prepared = prepare_bound(&orch).await;
    let (control, events, driver, provider, _) = connection().await;
    let (_inputs_tx, inputs) = admitted_inputs();
    let (output, mut output_rx) = mpsc::channel(32);
    let cancel = CancellationToken::new();
    let task = tokio::spawn({
        let cancel = cancel.clone();
        async move {
            orch.run_realtime_agent(
                prepared,
                control,
                events,
                inputs,
                output,
                Default::default(),
                false,
                cancel,
            )
            .await
        }
    });
    let id = Some(next_agent_started(&mut output_rx).await);
    for kind in ["start", "tool"] {
        provider
            .unbounded_send(Ok(RealtimeFrame::text(kind)))
            .unwrap();
    }
    wait_until(
        || gate.calls.load(Ordering::SeqCst) > 0,
        "permission requested",
    )
    .await;
    provider
        .unbounded_send(Ok(RealtimeFrame::text("done-cancelled")))
        .unwrap();
    loop {
        match output_rx.recv().await.unwrap() {
            RealtimeAgentOutput::AgentResponseStarted { .. } => {
                panic!("unexpected second admission")
            }
            RealtimeAgentOutput::Provider(RealtimeEvent::TurnCompleted { .. }) => break,
            RealtimeAgentOutput::AgentTurnCompleted { .. } => {
                panic!("terminal before provider cancellation")
            }
            _ => {}
        }
    }
    release.notify_one();
    loop {
        if let RealtimeAgentOutput::AgentTurnCompleted { turn_id } =
            tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
                .await
                .unwrap()
                .unwrap()
        {
            assert_eq!(Some(turn_id), id);
            break;
        }
    }
    cancel.cancel();
    assert_eq!(task.await.unwrap().unwrap(), RealtimeAgentEnd::Cancelled);
    let _ = driver.await;
}

#[tokio::test]
async fn gemini_interruption_in_a_separate_frame_waits_for_tool_drain() {
    for release_before_done in [false, true] {
        let release = Arc::new(tokio::sync::Notify::new());
        let gate = Arc::new(Gate {
            calls: AtomicUsize::new(0),
            deny: false,
            release: Some(release.clone()),
            approved: AtomicBool::new(false),
        });
        let (orch, _) = fixture(gate.clone()).await;
        let prepared = prepare_bound(&orch).await;
        let (control, events, driver, provider, _) = gemini_connection().await;
        let (inputs_tx, inputs) = mpsc::channel(4);
        let (output, mut output_rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();
        let task = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                orch.run_realtime_agent(
                    prepared,
                    control,
                    events,
                    inputs,
                    output,
                    RealtimeAgentLimits {
                        completion_boundary: RealtimeCompletionBoundary::Gemini38,
                        ..Default::default()
                    },
                    false,
                    cancel,
                )
                .await
            }
        });
        inputs_tx
            .send(RealtimeAgentInput::Provider(RealtimeInput::CommitAudio))
            .await
            .unwrap();
        let id = loop {
            if let RealtimeAgentOutput::AgentResponseStarted { turn_id } =
                output_rx.recv().await.unwrap()
            {
                break turn_id;
            }
        };
        provider
            .unbounded_send(Ok(RealtimeFrame::text(
                r#"{"toolCall":{"functionCalls":[{"id":"call","name":"fixture","args":{}}]}}"#,
            )))
            .unwrap();
        wait_until(
            || gate.calls.load(Ordering::SeqCst) > 0,
            "Gemini permission requested",
        )
        .await;
        provider
            .unbounded_send(Ok(RealtimeFrame::text(
                r#"{"serverContent":{"interrupted":true}}"#,
            )))
            .unwrap();
        loop {
            if matches!(
                output_rx.recv().await.unwrap(),
                RealtimeAgentOutput::Provider(RealtimeEvent::Interrupted)
            ) {
                break;
            }
        }
        if release_before_done {
            release.notify_one();
        }
        provider
            .unbounded_send(Ok(RealtimeFrame::text(
                r#"{"serverContent":{"turnComplete":true,"interactionStatus":"IDLE"}}"#,
            )))
            .unwrap();
        if !release_before_done {
            release.notify_one();
        }
        loop {
            match tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                RealtimeAgentOutput::AgentTurnCompleted { turn_id } => {
                    assert_eq!(turn_id, id);
                    break;
                }
                RealtimeAgentOutput::AgentResponseStarted { .. } => {
                    panic!("interruption created another input")
                }
                _ => {}
            }
        }
        cancel.cancel();
        assert_eq!(task.await.unwrap().unwrap(), RealtimeAgentEnd::Cancelled);
        let _ = driver.await;
    }
}

#[tokio::test]
async fn stale_provider_completion_cannot_finish_new_local_input() {
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: false,
        release: None,
        approved: AtomicBool::new(false),
    });
    let (orch, _) = fixture(gate).await;
    let prepared = prepare_bound(&orch).await;
    let (control, events, driver, provider, _) = connection().await;
    let (inputs_tx, inputs) = admitted_inputs();
    let (output, mut output_rx) = mpsc::channel(32);
    let cancel = CancellationToken::new();
    let task = tokio::spawn({
        let cancel = cancel.clone();
        async move {
            orch.run_realtime_agent(
                prepared,
                control,
                events,
                inputs,
                output,
                Default::default(),
                false,
                cancel,
            )
            .await
        }
    });
    next_agent_started(&mut output_rx).await;
    for kind in ["start", "done"] {
        provider
            .unbounded_send(Ok(RealtimeFrame::text(kind)))
            .unwrap();
    }
    let first = loop {
        if let RealtimeAgentOutput::AgentTurnCompleted { turn_id } = output_rx.recv().await.unwrap()
        {
            break turn_id;
        }
    };
    inputs_tx
        .send(RealtimeAgentInput::Provider(RealtimeInput::CommitAudio))
        .await
        .unwrap();
    let second = loop {
        match output_rx.recv().await.unwrap() {
            RealtimeAgentOutput::AgentResponseStarted { turn_id } => {
                assert_ne!(turn_id, first);
                break turn_id;
            }
            _ => {}
        }
    };
    for kind in ["start-continuation", "done", "ready"] {
        provider
            .unbounded_send(Ok(RealtimeFrame::text(kind)))
            .unwrap();
    }
    loop {
        match output_rx.recv().await.unwrap() {
            RealtimeAgentOutput::Provider(RealtimeEvent::SessionReady) => break,
            RealtimeAgentOutput::Provider(RealtimeEvent::TurnCompleted {
                turn_id: Some(id),
                ..
            }) if id == "response-first" => panic!("old response reached new playback"),
            RealtimeAgentOutput::AgentTurnCompleted { .. } => {
                panic!("old response finished the new input")
            }
            _ => {}
        }
    }
    assert!(output_rx.try_recv().is_err());
    provider
        .unbounded_send(Ok(RealtimeFrame::text("done-continuation")))
        .unwrap();
    loop {
        if let RealtimeAgentOutput::AgentTurnCompleted { turn_id } = output_rx.recv().await.unwrap()
        {
            assert_eq!(turn_id, second);
            break;
        }
    }
    cancel.cancel();
    assert_eq!(task.await.unwrap().unwrap(), RealtimeAgentEnd::Cancelled);
    let _ = driver.await;
}

#[tokio::test]
async fn gemini_missing_idle_status_fails_closed_without_a_terminal() {
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: false,
        release: None,
        approved: AtomicBool::new(false),
    });
    let (orch, _) = fixture(gate).await;
    let prepared = prepare_bound(&orch).await;
    let (control, events, driver, provider, _) = connection().await;
    let (inputs_tx, inputs) = mpsc::channel(4);
    let (output, mut output_rx) = mpsc::channel(32);
    let task = tokio::spawn(async move {
        orch.run_realtime_agent(
            prepared,
            control,
            events,
            inputs,
            output,
            RealtimeAgentLimits {
                completion_boundary: RealtimeCompletionBoundary::Gemini38,
                ..Default::default()
            },
            false,
            CancellationToken::new(),
        )
        .await
    });
    inputs_tx
        .send(RealtimeAgentInput::Provider(RealtimeInput::CommitAudio))
        .await
        .unwrap();
    next_agent_started(&mut output_rx).await;
    provider
        .unbounded_send(Ok(RealtimeFrame::text("gemini-missing")))
        .unwrap();
    let error = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("cannot confirm interactionStatus IDLE"));
    while let Ok(event) = output_rx.try_recv() {
        assert!(!matches!(
            event,
            RealtimeAgentOutput::AgentTurnCompleted { .. }
        ));
    }
    let _ = driver.await;
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
    let prepared = prepare_bound(&orch).await;
    let (control, events, driver, events_tx, _) = connection().await;
    let (_inputs_tx, inputs) = admitted_inputs();
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
                false,
                CancellationToken::new(),
            )
            .await
        }
    });
    next_agent_started(&mut output_rx).await;
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
        RealtimeAgentOutput::Provider(RealtimeEvent::ToolCancelled { .. })
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
async fn current_catalog_filters_media_resources_and_permission_denial_never_executes_tool() {
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: true,
        release: None,
        approved: AtomicBool::new(false),
    });
    let (orch, calls) = fixture(gate.clone()).await;
    let prepared = prepare_bound(&orch).await;
    assert_eq!(prepared.profile_name.as_deref(), Some("exact-profile"));
    let names = prepared
        .tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names, ["fixture", "media_capture", "media_observe"]);
    let manifest = orch.context_manifest().await;
    assert!(manifest.projected);
    let manifest_tools = manifest
        .blocks
        .iter()
        .filter_map(|block| block.id.strip_prefix("tool:"))
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        manifest_tools,
        names
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
    );
    let (control, events, driver, events_tx, sent) = connection().await;
    let (_inputs_tx, inputs) = admitted_inputs();
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
                false,
                cancel,
            )
            .await
        }
    });
    next_agent_started(&mut output_rx).await;
    events_tx
        .unbounded_send(Ok(RealtimeFrame::text("tool")))
        .unwrap();
    wait_until(
        || sent.lock().unwrap().len() >= 3,
        "tool result followed by continuation",
    )
    .await;
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(sent.lock().unwrap()[1].contains("is_error"));
    cancel.cancel();
    assert_eq!(task.await.unwrap().unwrap(), RealtimeAgentEnd::Cancelled);
    let _ = driver.await;
    assert_eq!(orch.snapshot_history().await.len(), 2);
}

#[tokio::test]
async fn visual_tool_result_sends_real_image_bytes_after_tool_result() {
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: false,
        release: None,
        approved: AtomicBool::new(false),
    });
    let (orch, calls) = visual_fixture(gate).await;
    let prepared = prepare_bound(&orch).await;
    let captured = Arc::new(CapturedMediaInputs::default());
    let (control, events, driver, events_tx, _) =
        connection_with_codec(Arc::new(ImageRecordingCodec(captured.clone()))).await;
    let (_inputs_tx, inputs) = admitted_inputs();
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
                true,
                cancel,
            )
            .await
        }
    });
    next_agent_started(&mut output_rx).await;
    events_tx
        .unbounded_send(Ok(RealtimeFrame::text("image-tool")))
        .unwrap();
    wait_until(
        || captured.images.lock().unwrap().len() == 1,
        "tool image reaches realtime codec",
    )
    .await;
    wait_until(
        || captured.order.lock().unwrap().len() >= 4,
        "tool result, image, and continuation reach realtime codec in order",
    )
    .await;

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let order = captured.order.lock().unwrap().clone();
    assert_eq!(order, ["other", "tool_result", "image", "continue"]);
    let images = captured.images.lock().unwrap();
    let (mime_type, bytes) = &images[0];
    assert_eq!(mime_type, "image/png");
    assert_eq!(
        bytes,
        &base64::engine::general_purpose::STANDARD
            .decode(VISUAL_TEST_PNG_BASE64)
            .unwrap()
    );
    let tool_output = captured.tool_outputs.lock().unwrap();
    assert!(!tool_output[0].contains(VISUAL_TEST_PNG_BASE64));
    assert!(tool_output[0].contains("image_input"));

    let history = orch.snapshot_history().await;
    let persisted_image = history.iter().find_map(|message| match message {
        ConversationMessage::User { content, .. } => content.iter().find_map(|block| match block {
            ContentBlock::ToolResult {
                content_blocks: Some(content_blocks),
                ..
            } => content_blocks.string_at("/0/source/data"),
            _ => None,
        }),
        _ => None,
    });
    assert_eq!(persisted_image, Some(VISUAL_TEST_PNG_BASE64));

    cancel.cancel();
    assert_eq!(task.await.unwrap().unwrap(), RealtimeAgentEnd::Cancelled);
    let _ = driver.await;
}

#[tokio::test]
async fn visual_tool_result_fails_when_realtime_provider_has_no_image_input() {
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        deny: false,
        release: None,
        approved: AtomicBool::new(false),
    });
    let (orch, _) = visual_fixture(gate).await;
    let prepared = prepare_bound(&orch).await;
    let captured = Arc::new(CapturedMediaInputs::default());
    let (control, events, driver, events_tx, _) =
        connection_with_codec(Arc::new(ImageRecordingCodec(captured.clone()))).await;
    let (_inputs_tx, inputs) = admitted_inputs();
    let (output, mut output_rx) = mpsc::channel(16);
    let task = tokio::spawn(async move {
        orch.run_realtime_agent(
            prepared,
            control,
            events,
            inputs,
            output,
            RealtimeAgentLimits::default(),
            false,
            CancellationToken::new(),
        )
        .await
    });
    next_agent_started(&mut output_rx).await;
    events_tx
        .unbounded_send(Ok(RealtimeFrame::text("image-tool")))
        .unwrap();
    let error = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err()
        .to_string();
    assert!(error.contains("does not support image input"));
    assert!(captured.images.lock().unwrap().is_empty());
    let _ = driver.await;
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
    let prepared = prepare_bound(&orch).await;
    let (control, events, driver, events_tx, _) = connection().await;
    let (inputs_tx, inputs) = admitted_inputs();
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
                false,
                cancel,
            )
            .await
        }
    });
    next_agent_started(&mut output_rx).await;
    for event in ["audio-committed", "input", "audio", "output"] {
        events_tx
            .unbounded_send(Ok(RealtimeFrame::text(event)))
            .unwrap();
        while !matches!(
            tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
                .await
                .unwrap()
                .unwrap(),
            RealtimeAgentOutput::Provider(_)
        ) {}
    }
    assert_eq!(orch.snapshot_history().await.len(), 1);
    inputs_tx
        .send(RealtimeAgentInput::PlaybackCompleted {
            item_id: Some("item-heard".into()),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while orch.snapshot_history().await.len() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    for event in ["output", "audio-unheard", "interrupt", "output-unheard"] {
        events_tx
            .unbounded_send(Ok(RealtimeFrame::text(event)))
            .unwrap();
        while !matches!(
            tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
                .await
                .unwrap()
                .unwrap(),
            RealtimeAgentOutput::Provider(_)
        ) {}
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

async fn next_agent_started(output: &mut mpsc::Receiver<RealtimeAgentOutput>) -> String {
    loop {
        match tokio::time::timeout(Duration::from_secs(2), output.recv())
            .await
            .unwrap()
            .unwrap()
        {
            RealtimeAgentOutput::AgentResponseStarted { turn_id } => return turn_id,
            RealtimeAgentOutput::AgentTurnCompleted { .. } => {
                panic!("turn completed before admission")
            }
            _ => {}
        }
    }
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
    let prepared = prepare_bound(&orch).await;
    let (control, events, driver, provider, _) = connection().await;
    let (_inputs, input_rx) = admitted_inputs();
    let (output, mut output_rx) = mpsc::channel(8);
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
                false,
                CancellationToken::new(),
            )
            .await
        }
    });
    next_agent_started(&mut output_rx).await;
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
    assert_eq!(prepare_bound(&orch).await.profile_name, Some(expected));
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
    let prepared = prepare_bound(&orch).await;
    let (control, events, driver, provider, sent) = connection().await;
    let (_inputs, inputs) = admitted_inputs();
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
                false,
                cancel,
            )
            .await
        }
    });
    next_agent_started(&mut output_rx).await;
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
    while !matches!(output_rx.recv().await.unwrap(),RealtimeAgentOutput::Provider(RealtimeEvent::ToolCall {call_id,..}) if call_id=="call-second")
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
        || sent.lock().unwrap().len() == 4,
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
