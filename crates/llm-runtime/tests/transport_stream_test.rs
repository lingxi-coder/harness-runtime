use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use llm_runtime::client::DefaultLlmClient;
use llm_runtime::{
    AuthStrategy, BoxFuture, Capabilities, ClientConfig, ContentDelta, CredentialConfig,
    FrameStream, LlmError, LlmEvent, LlmRequest, Message, ModelProfile, PricingConfig,
    ProtocolFamily, ProviderId, ProviderProfile, ProviderRequest, ProviderResponse, RawStreamFrame,
    ResponsesWebSocketSession, ResponsesWebSocketTransportSession, StreamingResponse, Transport,
};

struct ScriptedFrames {
    items: VecDeque<Result<RawStreamFrame, LlmError>>,
}

impl FrameStream for ScriptedFrames {
    fn next_frame(&mut self) -> BoxFuture<'_, Result<Option<RawStreamFrame>, LlmError>> {
        let next = match self.items.pop_front() {
            Some(Ok(frame)) => Ok(Some(frame)),
            Some(Err(error)) => Err(error),
            None => Ok(None),
        };
        Box::pin(async move { next })
    }
}

struct StreamTransport {
    status: u16,
    headers: BTreeMap<String, String>,
    frames: Mutex<Option<Vec<Result<RawStreamFrame, LlmError>>>>,
}

impl StreamTransport {
    fn scripted(status: u16, frames: Vec<Result<RawStreamFrame, LlmError>>) -> Self {
        Self {
            status,
            headers: BTreeMap::new(),
            frames: Mutex::new(Some(frames)),
        }
    }

    fn scripted_with_headers(
        status: u16,
        headers: BTreeMap<String, String>,
        frames: Vec<Result<RawStreamFrame, LlmError>>,
    ) -> Self {
        Self {
            status,
            headers,
            frames: Mutex::new(Some(frames)),
        }
    }
}

impl Transport for StreamTransport {
    fn execute<'a>(
        &'a self,
        _request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        Box::pin(async move {
            Err(LlmError::Transport {
                message: "execute not scripted".to_string(),
            })
        })
    }

    fn open_stream<'a>(
        &'a self,
        _request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        let frames = self
            .frames
            .lock()
            .expect("frames lock")
            .take()
            .expect("open_stream called once");
        let status = self.status;
        let headers = self.headers.clone();
        Box::pin(async move {
            Ok(StreamingResponse {
                status,
                headers,
                frames: Box::new(ScriptedFrames {
                    items: frames.into(),
                }),
            })
        })
    }
}

// Result-wrapped to match the scripted frame vec's element type.
#[allow(clippy::unnecessary_wraps)]
fn frame(payload: &str) -> Result<RawStreamFrame, LlmError> {
    Ok(RawStreamFrame::new(payload.as_bytes().to_vec()))
}

fn client(protocol: ProtocolFamily, provider_id: ProviderId, base_url: &str) -> DefaultLlmClient {
    DefaultLlmClient::from_config(ClientConfig {
        providers: vec![ProviderProfile {
            wire_profile: None,
            regions: llm_runtime::Region::all(),
            provider_id,
            profile_name: "p".to_string(),
            base_url: base_url.to_string(),
            protocol,
            auth: AuthStrategy::None,
            credential: CredentialConfig::None,
            models: vec![ModelProfile {
                display_model: "p-model".to_string(),
                request_model: "p-model".to_string(),
                billing_model: "p-model".to_string(),
                aliases: vec![],
                description: None,
                metadata: Default::default(),
                capabilities: Capabilities {
                    streaming: true,
                    tools: true,
                    ..Default::default()
                },
            }],
            pricing: PricingConfig::default(),
            signing: None,
            azure: None,
            supports_websockets: false,
            supports_websocket_compression: false,
            websocket_connect_timeout_ms: None,
            vision_delegate: None,
            connection: Default::default(),
        }],
    })
    .expect("client")
}

fn anthropic_client() -> DefaultLlmClient {
    client(
        ProtocolFamily::AnthropicMessages,
        ProviderId::AnthropicFirstParty,
        "https://api.anthropic.com",
    )
}

fn openai_responses_client() -> DefaultLlmClient {
    client(
        ProtocolFamily::OpenAiResponses,
        ProviderId::OpenAICompatible {
            name: "openai-responses".to_string(),
        },
        "https://api.openai.com/v1",
    )
}

fn openai_responses_websocket_client() -> DefaultLlmClient {
    DefaultLlmClient::from_config(ClientConfig {
        providers: vec![ProviderProfile {
            wire_profile: None,
            regions: lingxi_llm_client::protocol::Region::all(),
            provider_id: ProviderId::OpenAI,
            profile_name: "p".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            protocol: ProtocolFamily::OpenAiResponses,
            auth: AuthStrategy::None,
            credential: CredentialConfig::None,
            models: vec![ModelProfile {
                display_model: "p-model".to_string(),
                request_model: "p-model".to_string(),
                billing_model: "p-model".to_string(),
                aliases: vec![],
                description: None,
                metadata: Default::default(),
                capabilities: Capabilities {
                    streaming: true,
                    tools: true,
                    ..Default::default()
                },
            }],
            pricing: PricingConfig::default(),
            signing: None,
            azure: None,
            supports_websockets: true,
            supports_websocket_compression: false,
            websocket_connect_timeout_ms: Some(250),
            vision_delegate: None,
            connection: Default::default(),
        }],
    })
    .expect("client")
}

fn streaming_request() -> LlmRequest {
    let mut request = LlmRequest::new("p-model").with_user_text("hello");
    request.stream = true;
    request
}

fn append_user_text(request: &mut LlmRequest, text: &str) {
    request.messages.push(Message {
        role: "user".to_string(),
        content: vec![llm_runtime::ContentBlock::Text {
            text: text.to_string(),
            cache_control: None,
        }],
    });
}

async fn collect_events(
    stream: &mut llm_runtime::LlmEventStream,
) -> (Vec<LlmEvent>, Option<LlmError>) {
    let mut events = Vec::new();
    loop {
        match stream.next_event().await {
            Ok(Some(event)) => events.push(event),
            Ok(None) => return (events, None),
            Err(error) => return (events, Some(error)),
        }
    }
}

struct ScriptedWsSession {
    sent_bodies: std::sync::Arc<Mutex<Vec<serde_json::Value>>>,
    responses: std::sync::Arc<Mutex<VecDeque<Vec<Result<RawStreamFrame, LlmError>>>>>,
    close_count: std::sync::Arc<Mutex<u32>>,
}

impl ResponsesWebSocketTransportSession for ScriptedWsSession {
    fn send<'a>(
        &'a mut self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        self.sent_bodies
            .lock()
            .expect("sent_bodies")
            .push(request.body_json.clone());
        let frames = self
            .responses
            .lock()
            .expect("responses")
            .pop_front()
            .expect("scripted websocket response");
        Box::pin(async move {
            Ok(StreamingResponse {
                status: 101,
                headers: BTreeMap::new(),
                frames: Box::new(ScriptedFrames {
                    items: frames.into(),
                }),
            })
        })
    }

    fn close(&mut self) -> BoxFuture<'_, Result<(), LlmError>> {
        *self.close_count.lock().expect("close_count") += 1;
        Box::pin(async { Ok(()) })
    }
}

struct ResponsesWsTransport {
    sent_bodies: std::sync::Arc<Mutex<Vec<serde_json::Value>>>,
    responses: std::sync::Arc<Mutex<VecDeque<Vec<Result<RawStreamFrame, LlmError>>>>>,
    close_count: std::sync::Arc<Mutex<u32>>,
    open_count: Mutex<u32>,
    open_error: Mutex<Option<LlmError>>,
    http_streams: Mutex<VecDeque<Vec<Result<RawStreamFrame, LlmError>>>>,
    http_count: Mutex<u32>,
}

impl ResponsesWsTransport {
    fn new(responses: Vec<Vec<Result<RawStreamFrame, LlmError>>>) -> Self {
        Self {
            sent_bodies: std::sync::Arc::new(Mutex::new(Vec::new())),
            responses: std::sync::Arc::new(Mutex::new(responses.into())),
            close_count: std::sync::Arc::new(Mutex::new(0)),
            open_count: Mutex::new(0),
            open_error: Mutex::new(None),
            http_streams: Mutex::new(VecDeque::new()),
            http_count: Mutex::new(0),
        }
    }

    fn with_426_fallback(http_streams: Vec<Vec<Result<RawStreamFrame, LlmError>>>) -> Self {
        Self {
            sent_bodies: std::sync::Arc::new(Mutex::new(Vec::new())),
            responses: std::sync::Arc::new(Mutex::new(VecDeque::new())),
            close_count: std::sync::Arc::new(Mutex::new(0)),
            open_count: Mutex::new(0),
            open_error: Mutex::new(Some(LlmError::Transport {
                message: "non-success HTTP status 426: upgrade required".to_string(),
            })),
            http_streams: Mutex::new(http_streams.into()),
            http_count: Mutex::new(0),
        }
    }

    fn sent_bodies(&self) -> Vec<serde_json::Value> {
        self.sent_bodies.lock().expect("sent_bodies").clone()
    }

    fn open_count(&self) -> u32 {
        *self.open_count.lock().expect("open_count")
    }

    fn http_count(&self) -> u32 {
        *self.http_count.lock().expect("http_count")
    }

    fn close_count(&self) -> u32 {
        *self.close_count.lock().expect("close_count")
    }
}

impl Transport for ResponsesWsTransport {
    fn execute<'a>(
        &'a self,
        _request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        Box::pin(async move {
            Err(LlmError::Transport {
                message: "execute not scripted".to_string(),
            })
        })
    }

    fn open_stream<'a>(
        &'a self,
        _request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        *self.http_count.lock().expect("http_count") += 1;
        let frames = self
            .http_streams
            .lock()
            .expect("http_streams")
            .pop_front()
            .expect("scripted http stream");
        Box::pin(async move {
            Ok(StreamingResponse {
                status: 200,
                headers: BTreeMap::new(),
                frames: Box::new(ScriptedFrames {
                    items: frames.into(),
                }),
            })
        })
    }

    fn open_responses_websocket_session<'a>(
        &'a self,
        _request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<Box<dyn ResponsesWebSocketTransportSession>, LlmError>> {
        *self.open_count.lock().expect("open_count") += 1;
        if let Some(error) = self.open_error.lock().expect("open_error").clone() {
            return Box::pin(async move { Err(error) });
        }
        let sent_bodies = std::sync::Arc::clone(&self.sent_bodies);
        let responses = std::sync::Arc::clone(&self.responses);
        let close_count = std::sync::Arc::clone(&self.close_count);
        Box::pin(async move {
            Ok(Box::new(ScriptedWsSession {
                sent_bodies,
                responses,
                close_count,
            })
                as Box<dyn ResponsesWebSocketTransportSession>)
        })
    }
}

fn responses_completed(id: &str) -> Vec<Result<RawStreamFrame, LlmError>> {
    vec![
        frame(&format!(
            r#"{{"type":"response.created","response":{{"id":"{id}","model":"p-model"}}}}"#
        )),
        frame(&format!(
            r#"{{"type":"response.completed","response":{{"id":"{id}","model":"p-model","status":"completed"}}}}"#
        )),
    ]
}

#[tokio::test]
async fn execute_stream_decodes_anthropic_event_sequence() {
    let transport = Arc::new(StreamTransport::scripted(
        200,
        vec![
            frame(
                r#"{"type":"message_start","message":{"id":"msg_1","model":"p-model","content":[],"usage":{"input_tokens":2,"output_tokens":0}}}"#,
            ),
            frame(
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            ),
            frame(
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
            ),
            frame(r#"{"type":"content_block_stop","index":0}"#),
            frame(
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":2,"output_tokens":1}}"#,
            ),
            frame(r#"{"type":"message_stop"}"#),
        ],
    ));
    let client = anthropic_client();

    let mut stream = client
        .execute_stream(&streaming_request(), transport.clone())
        .await
        .expect("stream");
    let (events, error) = collect_events(&mut stream).await;

    assert!(error.is_none(), "unexpected error: {error:?}");
    assert!(matches!(events[0], LlmEvent::MessageStart { .. }));
    assert!(matches!(
        events[2],
        LlmEvent::ContentBlockDelta { delta: ContentDelta::TextDelta { ref text }, .. } if text == "hi"
    ));
    assert!(matches!(
        events[4],
        LlmEvent::MessageDelta { delta: llm_runtime::MessageDeltaPayload { stop_reason: Some(ref reason), stop_details: None }, .. }
            if reason == "end_turn"
    ));
    assert!(matches!(events.last(), Some(LlmEvent::MessageStop)));
    assert!(stream.next_event().await.expect("after end").is_none());
}

#[tokio::test]
async fn execute_stream_threads_provider_metadata_headers_to_responses_decoder() {
    let mut headers = BTreeMap::new();
    headers.insert("openai-model".to_string(), "gpt-5-2026-06-01".to_string());
    headers.insert("x-models-etag".to_string(), "etag-1".to_string());
    headers.insert("x-codex-turn-state".to_string(), "turn-state".to_string());
    headers.insert("x-ratelimit-limit-requests".to_string(), "1000".to_string());

    let transport = Arc::new(StreamTransport::scripted_with_headers(
        200,
        headers.clone(),
        vec![
            frame(
                r#"{"type":"response.created","response":{"id":"resp_1","model":"p-model","status":"in_progress"}}"#,
            ),
            frame(
                r#"{"type":"response.completed","response":{"id":"resp_1","model":"p-model","status":"completed","usage":{"input_tokens":2,"output_tokens":3,"total_tokens":5}}}"#,
            ),
        ],
    ));
    let client = openai_responses_client();
    let mut stream = client
        .execute_stream(&streaming_request(), transport.clone())
        .await
        .expect("stream");
    let (events, error) = collect_events(&mut stream).await;

    assert!(error.is_none(), "unexpected stream error: {error:?}");
    assert!(matches!(
        &events[0],
        LlmEvent::MessageStart { response }
            if response.provider_metadata["openai-model"] == "gpt-5-2026-06-01"
                && response.provider_metadata["x-models-etag"] == "etag-1"
                && response.provider_metadata["x-codex-turn-state"] == "turn-state"
                && response.provider_metadata["x-ratelimit-limit-requests"] == "1000"
    ));
}

#[tokio::test]
async fn prewarm_websocket_sends_generate_false_and_records_response_id() {
    let client = openai_responses_websocket_client();
    let transport = Arc::new(ResponsesWsTransport::new(vec![responses_completed(
        "resp_warm",
    )]));
    let mut session = ResponsesWebSocketSession::new();

    client
        .prewarm_websocket(&streaming_request(), transport.clone(), &mut session)
        .await
        .expect("prewarm");

    let sent = transport.sent_bodies();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["generate"], false);
    assert!(sent[0].get("previous_response_id").is_none());
    assert_eq!(session.last_response_id().as_deref(), Some("resp_warm"));
}

#[tokio::test]
async fn prewarmed_response_id_is_consumed_as_wire_delta_without_replacing_logical_snapshot() {
    let client = openai_responses_websocket_client();
    let transport = Arc::new(ResponsesWsTransport::new(vec![
        responses_completed("resp_warm"),
        responses_completed("resp_real"),
    ]));
    let mut session = ResponsesWebSocketSession::new();
    let request = streaming_request();

    client
        .prewarm_websocket(&request, transport.clone(), &mut session)
        .await
        .expect("prewarm");
    assert!(session.last_response_from_prewarm());

    let mut stream = client
        .execute_stream_with_session(&request, transport.clone(), &mut session)
        .await
        .expect("real stream");
    let (_, error) = collect_events(&mut stream).await;
    assert!(error.is_none(), "real stream error: {error:?}");

    let sent = transport.sent_bodies();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1]["previous_response_id"], "resp_warm");
    assert_eq!(sent[1]["input"], serde_json::json!([]));

    let snapshot = session.last_request_snapshot();
    let logical = snapshot.logical_request_body.expect("logical body");
    let wire = snapshot.wire_request_body.expect("wire body");
    assert_eq!(logical["input"].as_array().expect("logical input").len(), 1);
    assert_eq!(wire["input"], serde_json::json!([]));
    assert!(snapshot.wire_used_previous_response_id);
    assert!(snapshot.wire_used_prewarm_response_id);
    assert!(!session.last_response_from_prewarm());
}

#[tokio::test]
async fn responses_websocket_session_close_resets_state_and_closes_transport() {
    let client = openai_responses_websocket_client();
    let transport = Arc::new(ResponsesWsTransport::new(vec![responses_completed(
        "resp_1",
    )]));
    let mut session = ResponsesWebSocketSession::new();

    let mut stream = client
        .execute_stream_with_session(&streaming_request(), transport.clone(), &mut session)
        .await
        .expect("stream");
    let (_, error) = collect_events(&mut stream).await;
    assert!(error.is_none(), "stream error: {error:?}");
    assert_eq!(session.last_response_id().as_deref(), Some("resp_1"));

    session.close().await.expect("close");

    assert_eq!(transport.close_count(), 1);
    assert_eq!(session.last_response_id(), None);
    assert!(session.last_request_snapshot().wire_request_body.is_none());
}

#[tokio::test]
async fn websocket_session_second_compatible_request_uses_previous_response_delta() {
    let client = openai_responses_websocket_client();
    let transport = Arc::new(ResponsesWsTransport::new(vec![
        responses_completed("resp_1"),
        responses_completed("resp_2"),
    ]));
    let mut session = ResponsesWebSocketSession::new();

    let first_request = streaming_request();
    let mut first_stream = client
        .execute_stream_with_session(&first_request, transport.clone(), &mut session)
        .await
        .expect("first stream");
    let (_, first_error) = collect_events(&mut first_stream).await;
    assert!(first_error.is_none(), "first error: {first_error:?}");

    let mut second_request = first_request.clone();
    append_user_text(&mut second_request, "again");
    let mut second_stream = client
        .execute_stream_with_session(&second_request, transport.clone(), &mut session)
        .await
        .expect("second stream");
    let (_, second_error) = collect_events(&mut second_stream).await;
    assert!(second_error.is_none(), "second error: {second_error:?}");

    let sent = transport.sent_bodies();
    assert_eq!(sent.len(), 2);
    assert!(sent[0].get("previous_response_id").is_none());
    assert_eq!(sent[1]["previous_response_id"], "resp_1");
    assert_eq!(
        sent[1]["input"].as_array().expect("delta input").len(),
        1,
        "second request should only send the newly-added input item"
    );
}

#[tokio::test]
async fn websocket_session_non_input_change_disables_incremental_delta() {
    let client = openai_responses_websocket_client();
    let transport = Arc::new(ResponsesWsTransport::new(vec![
        responses_completed("resp_1"),
        responses_completed("resp_2"),
    ]));
    let mut session = ResponsesWebSocketSession::new();

    let first_request = streaming_request();
    let mut first_stream = client
        .execute_stream_with_session(&first_request, transport.clone(), &mut session)
        .await
        .expect("first stream");
    let (_, first_error) = collect_events(&mut first_stream).await;
    assert!(first_error.is_none(), "first error: {first_error:?}");

    let mut second_request = first_request.clone();
    second_request.temperature = Some(0.7);
    append_user_text(&mut second_request, "again");
    let mut second_stream = client
        .execute_stream_with_session(&second_request, transport.clone(), &mut session)
        .await
        .expect("second stream");
    let (_, second_error) = collect_events(&mut second_stream).await;
    assert!(second_error.is_none(), "second error: {second_error:?}");

    let sent = transport.sent_bodies();
    assert_eq!(sent.len(), 2);
    assert!(sent[1].get("previous_response_id").is_none());
    assert_eq!(
        sent[1]["input"].as_array().expect("full input").len(),
        2,
        "changed non-input fields must force a full request"
    );
}

#[tokio::test]
async fn websocket_session_incomplete_response_disables_next_incremental_delta() {
    let client = openai_responses_websocket_client();
    let incomplete = vec![frame(
        r#"{"type":"response.incomplete","response":{"id":"resp_1","model":"p-model","status":"incomplete"}}"#,
    )];
    let transport = Arc::new(ResponsesWsTransport::new(vec![
        incomplete,
        responses_completed("resp_2"),
    ]));
    let mut session = ResponsesWebSocketSession::new();

    let first_request = streaming_request();
    let mut first_stream = client
        .execute_stream_with_session(&first_request, transport.clone(), &mut session)
        .await
        .expect("first stream");
    let (_, first_error) = collect_events(&mut first_stream).await;
    assert!(first_error.is_none(), "first error: {first_error:?}");
    assert_eq!(session.last_response_id(), None);

    let mut second_request = first_request.clone();
    append_user_text(&mut second_request, "again");
    let mut second_stream = client
        .execute_stream_with_session(&second_request, transport.clone(), &mut session)
        .await
        .expect("second stream");
    let (_, second_error) = collect_events(&mut second_stream).await;
    assert!(second_error.is_none(), "second error: {second_error:?}");

    let sent = transport.sent_bodies();
    assert_eq!(sent.len(), 2);
    assert!(sent[1].get("previous_response_id").is_none());
}

#[tokio::test]
async fn websocket_session_426_fallback_latches_http_for_later_streams() {
    let client = openai_responses_websocket_client();
    let transport = Arc::new(ResponsesWsTransport::with_426_fallback(vec![
        responses_completed("http_1"),
        responses_completed("http_2"),
    ]));
    let mut session = ResponsesWebSocketSession::new();

    let mut first_stream = client
        .execute_stream_with_session(&streaming_request(), transport.clone(), &mut session)
        .await
        .expect("first fallback stream");
    let (_, first_error) = collect_events(&mut first_stream).await;
    assert!(first_error.is_none(), "first error: {first_error:?}");

    let mut second_stream = client
        .execute_stream_with_session(&streaming_request(), transport.clone(), &mut session)
        .await
        .expect("second fallback stream");
    let (_, second_error) = collect_events(&mut second_stream).await;
    assert!(second_error.is_none(), "second error: {second_error:?}");

    assert!(session.fallback_to_http());
    assert_eq!(transport.open_count(), 1, "426 should latch per session");
    assert_eq!(transport.http_count(), 2);
    assert!(transport.sent_bodies().is_empty());
}

#[tokio::test]
async fn finish_emits_terminal_events_when_frames_end() {
    let transport = Arc::new(StreamTransport::scripted(
        200,
        vec![frame(
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"hi"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":2,"candidatesTokenCount":1,"totalTokenCount":3}}"#,
        )],
    ));
    let client = client(
        ProtocolFamily::GeminiGenerateContent,
        ProviderId::Gemini,
        "https://generativelanguage.googleapis.com/v1beta",
    );

    let mut stream = client
        .execute_stream(&streaming_request(), transport.clone())
        .await
        .expect("stream");
    let (events, error) = collect_events(&mut stream).await;

    assert!(error.is_none(), "unexpected error: {error:?}");
    assert!(matches!(
        events.first(),
        Some(LlmEvent::MessageStart { .. })
    ));
    assert!(matches!(
        events.iter().find(|event| matches!(event, LlmEvent::MessageDelta { .. })),
        Some(LlmEvent::MessageDelta { usage: Some(usage), .. }) if usage.billable_tokens.input == 2
    ));
    assert!(matches!(events.last(), Some(LlmEvent::MessageStop)));
}

#[tokio::test]
async fn execute_stream_requires_stream_flag() {
    let transport = Arc::new(StreamTransport::scripted(200, vec![]));
    let client = anthropic_client();

    let error = client
        .execute_stream(
            &LlmRequest::new("p-model").with_user_text("hello"),
            transport.clone(),
        )
        .await
        .expect_err("must require stream flag");

    assert!(matches!(error, LlmError::InvalidRequest { message } if message.contains("stream")));
}

#[tokio::test]
async fn error_status_drains_frames_into_taxonomy() {
    let body = r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#;
    let (first, second) = body.as_bytes().split_at(20);
    let mut transport = StreamTransport::scripted(
        429,
        vec![
            Ok(RawStreamFrame::new(first.to_vec())),
            Ok(RawStreamFrame::new(second.to_vec())),
        ],
    );
    transport
        .headers
        .insert("retry-after".to_string(), "7".to_string());
    let transport = Arc::new(transport);
    let client = anthropic_client();

    let error = client
        .execute_stream(&streaming_request(), transport.clone())
        .await
        .expect_err("error status");

    assert!(matches!(
        error,
        LlmError::RateLimited { retry_after: Some(after), .. } if after == Duration::from_secs(7)
    ));
}

#[tokio::test]
async fn mid_stream_failure_after_events_is_stream_interrupted() {
    let transport = Arc::new(StreamTransport::scripted(
        200,
        vec![
            frame(
                r#"{"type":"message_start","message":{"id":"msg_1","model":"p-model","content":[],"usage":{"input_tokens":1,"output_tokens":0}}}"#,
            ),
            Err(LlmError::Transport {
                message: "connection reset".to_string(),
            }),
        ],
    ));
    let client = anthropic_client();

    let mut stream = client
        .execute_stream(&streaming_request(), transport.clone())
        .await
        .expect("stream");

    assert!(matches!(
        stream.next_event().await.expect("first event"),
        Some(LlmEvent::MessageStart { .. })
    ));
    assert!(matches!(
        stream.next_event().await.expect_err("interrupted"),
        LlmError::StreamInterrupted { message } if message.contains("connection reset")
    ));
    assert!(stream.next_event().await.expect("terminal").is_none());
}

#[tokio::test]
async fn failure_before_any_event_stays_transport() {
    let transport = Arc::new(StreamTransport::scripted(
        200,
        vec![Err(LlmError::Transport {
            message: "connection refused".to_string(),
        })],
    ));
    let client = anthropic_client();

    let mut stream = client
        .execute_stream(&streaming_request(), transport.clone())
        .await
        .expect("stream");

    assert!(matches!(
        stream.next_event().await.expect_err("transport failure"),
        LlmError::Transport { message } if message.contains("connection refused")
    ));
    assert!(stream.next_event().await.expect("terminal").is_none());
}

#[tokio::test]
async fn cancelling_a_websocket_response_invalidates_continuation_and_connection() {
    let client = openai_responses_websocket_client();
    let transport = Arc::new(ResponsesWsTransport::new(vec![
        responses_completed("before"),
        vec![frame(
            r#"{"type":"response.created","response":{"id":"cancelled","model":"gpt-5.2"}}"#,
        )],
        responses_completed("after"),
    ]));
    let mut session = ResponsesWebSocketSession::new();
    let mut request = streaming_request();
    let mut stream = client
        .execute_stream_with_session(&request, transport.clone(), &mut session)
        .await
        .unwrap();
    assert!(collect_events(&mut stream).await.1.is_none());
    append_user_text(&mut request, "second");
    let stream = client
        .execute_stream_with_session(&request, transport.clone(), &mut session)
        .await
        .unwrap();
    drop(stream);
    assert_eq!(session.last_response_id(), None);
    append_user_text(&mut request, "third");
    let mut stream = client
        .execute_stream_with_session(&request, transport.clone(), &mut session)
        .await
        .unwrap();
    assert!(collect_events(&mut stream).await.1.is_none());
    assert_eq!(transport.open_count(), 2);
    let sent = transport.sent_bodies();
    assert_eq!(sent.len(), 3);
    assert!(sent[2].get("previous_response_id").is_none());
}

#[tokio::test]
async fn responses_stream_exposes_continuation_only_after_completion() {
    let client = openai_responses_client();
    let transport = Arc::new(StreamTransport::scripted(
        200,
        responses_completed("resp_scoped"),
    ));
    let mut request = streaming_request();
    request.account_scope = Some("account-a".into());
    let mut stream = client
        .execute_stream(&request, transport)
        .await
        .expect("stream");
    assert!(stream.continuation().is_none());

    let (_, error) = collect_events(&mut stream).await;
    assert!(error.is_none(), "stream error: {error:?}");
    let continuation = stream.continuation().expect("completed continuation");
    assert_eq!(continuation.response_id.as_str(), "resp_scoped");
    assert_eq!(continuation.account_scope, "account-a");
    assert_eq!(continuation.profile_name, "p");
    assert_eq!(continuation.request_model, "p-model");

    request.continuation = Some(continuation.clone());
    let prepared = client
        .prepare(&request)
        .await
        .expect("same-account follow-up");
    assert_eq!(
        prepared.provider_request.body_json["previous_response_id"],
        "resp_scoped"
    );
    request.account_scope = Some("account-b".into());
    assert!(matches!(
        client.prepare(&request).await,
        Err(LlmError::InvalidRequest { .. })
    ));
}

#[tokio::test]
async fn interrupted_responses_stream_does_not_expose_continuation() {
    let client = openai_responses_client();
    let transport = Arc::new(StreamTransport::scripted(
        200,
        vec![
            frame(
                r#"{"type":"response.created","response":{"id":"resp_partial","model":"p-model"}}"#,
            ),
            Err(LlmError::Transport {
                message: "connection closed".into(),
            }),
        ],
    ));
    let mut request = streaming_request();
    request.account_scope = Some("account-a".into());
    let mut stream = client
        .execute_stream(&request, transport)
        .await
        .expect("stream");
    let (_, error) = collect_events(&mut stream).await;
    assert!(error.is_some());
    assert!(stream.continuation().is_none());
}

#[tokio::test]
async fn responses_websocket_account_change_rebinds_session() {
    let client = openai_responses_websocket_client();
    let transport = Arc::new(ResponsesWsTransport::new(vec![
        responses_completed("resp_account_a"),
        responses_completed("resp_account_b"),
    ]));
    let mut session = ResponsesWebSocketSession::new();
    let mut first_request = streaming_request();
    first_request.account_scope = Some("account-a".into());
    let mut first_stream = client
        .execute_stream_with_session(&first_request, transport.clone(), &mut session)
        .await
        .expect("account-a stream");
    assert!(collect_events(&mut first_stream).await.1.is_none());
    assert_eq!(
        first_stream
            .continuation()
            .expect("account-a continuation")
            .account_scope,
        "account-a"
    );

    let mut second_request = first_request.clone();
    second_request.account_scope = Some("account-b".into());
    append_user_text(&mut second_request, "continue under account b");
    let mut second_stream = client
        .execute_stream_with_session(&second_request, transport.clone(), &mut session)
        .await
        .expect("account-b stream");
    assert!(collect_events(&mut second_stream).await.1.is_none());
    let second_continuation = second_stream
        .continuation()
        .expect("account-b continuation");
    let sent = transport.sent_bodies();
    assert_eq!(sent.len(), 2);
    assert!(
        transport.open_count() == 2 && sent[1].get("previous_response_id").is_none(),
        "account change must establish a fresh connection without the old account's continuation; opens={}, wire_previous_response_id={}, returned_account_scope={}",
        transport.open_count(),
        sent[1]["previous_response_id"],
        second_continuation.account_scope,
    );
}

#[tokio::test]
async fn responses_websocket_preserves_explicit_continuation() {
    let client = openai_responses_websocket_client();
    let transport = Arc::new(ResponsesWsTransport::new(vec![
        responses_completed("resp_session_cached"),
        responses_completed("resp_after_explicit"),
    ]));
    let mut session = ResponsesWebSocketSession::new();
    let mut first_request = streaming_request();
    first_request.account_scope = Some("account-a".into());
    let mut first_stream = client
        .execute_stream_with_session(&first_request, transport.clone(), &mut session)
        .await
        .expect("first stream");
    assert!(collect_events(&mut first_stream).await.1.is_none());
    let mut explicit = first_stream
        .continuation()
        .expect("scoped continuation")
        .clone();
    explicit.response_id = lingxi_llm_client::protocol::ResponseId::new("resp_external_branch");

    let mut second_request = first_request.clone();
    second_request.continuation = Some(explicit);
    append_user_text(&mut second_request, "continue the explicitly chosen branch");
    let prepared = client
        .prepare(&second_request)
        .await
        .expect("valid explicit continuation");
    assert_eq!(
        prepared.provider_request.body_json["previous_response_id"],
        "resp_external_branch"
    );
    let mut second_stream = client
        .execute_stream_with_session(&second_request, transport.clone(), &mut session)
        .await
        .expect("explicit continuation stream");
    assert!(collect_events(&mut second_stream).await.1.is_none());
    let sent = transport.sent_bodies();
    assert_eq!(sent.len(), 2);
    assert_eq!(
        sent[1]["previous_response_id"],
        "resp_external_branch",
        "WebSocket session compression must preserve the explicitly selected and validated continuation",
    );
}
