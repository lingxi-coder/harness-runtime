use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use http_client::ReqwestHttp;
use llm_runtime::LlmTransportBridge;
use platform_api::http::{
    RawByteStream, RawByteStreamWithMeta, SseStream, WebSocketMessageStreamWithMeta,
};
use platform_api::{HttpError, HttpTransport};
use protocol::{HttpRequest, HttpResponse, SseEvent};

type ScriptedSse = Mutex<Option<Result<Vec<Result<SseEvent, HttpError>>, HttpError>>>;
type ScriptedRaw = Mutex<Option<Result<RawByteStreamWithMeta, HttpError>>>;
type ScriptedWebSocket = Mutex<Option<Result<WebSocketMessageStreamWithMeta, HttpError>>>;

#[derive(Default)]
struct FakeHttp {
    response: Mutex<Option<Result<HttpResponse, HttpError>>>,
    sse: ScriptedSse,
    raw: ScriptedRaw,
    websocket: ScriptedWebSocket,
    seen: Mutex<Option<HttpRequest>>,
    websocket_seen: Mutex<Option<HttpRequest>>,
}

#[async_trait]
impl HttpTransport for FakeHttp {
    async fn request(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        *self.seen.lock().expect("seen") = Some(req);
        self.response
            .lock()
            .expect("response")
            .take()
            .expect("scripted response")
    }

    async fn stream_sse(&self, req: HttpRequest) -> Result<SseStream, HttpError> {
        *self.seen.lock().expect("seen") = Some(req);
        let events = self
            .sse
            .lock()
            .expect("sse")
            .take()
            .expect("scripted sse")?;
        Ok(Box::pin(futures_util::stream::iter(events)))
    }

    async fn stream_raw_bytes_with_meta(
        &self,
        req: HttpRequest,
    ) -> Result<RawByteStreamWithMeta, HttpError> {
        *self.seen.lock().expect("seen") = Some(req);
        self.raw.lock().expect("raw").take().expect("scripted raw")
    }

    async fn stream_websocket_messages_with_meta(
        &self,
        req: HttpRequest,
    ) -> Result<WebSocketMessageStreamWithMeta, HttpError> {
        *self.websocket_seen.lock().expect("websocket_seen") = Some(req);
        self.websocket
            .lock()
            .expect("websocket")
            .take()
            .expect("scripted websocket")
    }
}

fn provider_request() -> llm_runtime::ProviderRequest {
    let mut request = llm_runtime::ProviderRequest::post_json(
        "https://api.anthropic.com/v1/messages",
        serde_json::json!({"model": "m"}),
    );
    request
        .headers
        .insert("x-api-key".to_string(), "k".to_string());
    request
}

#[tokio::test]
async fn execute_maps_request_and_response() {
    let fake = FakeHttp::default();
    *fake.response.lock().unwrap() = Some(Ok(HttpResponse {
        status: 200,
        headers: vec![
            ("Request-Id".to_string(), "req_1".to_string()),
            ("Retry-After".to_string(), "7".to_string()),
        ],
        body: r#"{"id":"msg_1"}"#.to_string(),
        body_bytes: Vec::new(),
    }));
    let bridge = LlmTransportBridge::new(fake);

    let response = llm_runtime::Transport::execute(&bridge, &provider_request())
        .await
        .expect("response");

    assert_eq!(response.status, 200);
    assert_eq!(
        response.headers.get("retry-after").map(String::as_str),
        Some("7")
    );
    assert_eq!(response.request_id.as_deref(), Some("req_1"));
    assert_eq!(response.body_json["id"], "msg_1");

    let seen = bridge
        .inner()
        .seen
        .lock()
        .unwrap()
        .take()
        .expect("request sent");
    assert!(matches!(seen.method, protocol::HttpMethod::Post));
    assert_eq!(seen.url, "https://api.anthropic.com/v1/messages");
    assert!(seen
        .headers
        .iter()
        .any(|(k, v)| k == "x-api-key" && v == "k"));
    assert_eq!(seen.body.as_deref(), Some(r#"{"model":"m"}"#));
}

/// A `ProviderRequest` carrying `body_bytes` must reach the transport with
/// those bytes verbatim and NO string body (the JSON body is suppressed so a
/// transport never double-sends).
#[tokio::test]
async fn execute_body_bytes_pass_through_verbatim_and_suppress_json_body() {
    let fake = FakeHttp::default();
    *fake.response.lock().unwrap() = Some(Ok(HttpResponse {
        status: 200,
        headers: vec![],
        body: "{}".to_string(),
        body_bytes: Vec::new(),
    }));
    let bridge = LlmTransportBridge::new(fake);

    let mut request = provider_request();
    // PNG magic — deliberately not valid UTF-8 JSON.
    request.body_bytes = Some(vec![0x89, 0x50, 0x4E, 0x47, 0x00, 0xFF]);

    llm_runtime::Transport::execute(&bridge, &request)
        .await
        .expect("response");

    let seen = bridge
        .inner()
        .seen
        .lock()
        .unwrap()
        .take()
        .expect("request sent");
    assert_eq!(
        seen.body_bytes.as_deref(),
        Some(&[0x89u8, 0x50, 0x4E, 0x47, 0x00, 0xFF][..]),
        "raw bytes must pass through verbatim"
    );
    assert!(
        seen.body.is_none(),
        "string body must be None when body_bytes is set; got: {:?}",
        seen.body
    );
}

/// Regression pin: without `body_bytes` the bridge keeps the existing JSON
/// body behavior unchanged.
#[tokio::test]
async fn execute_without_body_bytes_keeps_json_body_behavior() {
    let fake = FakeHttp::default();
    *fake.response.lock().unwrap() = Some(Ok(HttpResponse {
        status: 200,
        headers: vec![],
        body: "{}".to_string(),
        body_bytes: Vec::new(),
    }));
    let bridge = LlmTransportBridge::new(fake);

    llm_runtime::Transport::execute(&bridge, &provider_request())
        .await
        .expect("response");

    let seen = bridge
        .inner()
        .seen
        .lock()
        .unwrap()
        .take()
        .expect("request sent");
    assert_eq!(seen.body.as_deref(), Some(r#"{"model":"m"}"#));
    assert!(
        seen.body_bytes.is_none(),
        "no raw bytes unless explicitly set"
    );
}

#[tokio::test]
async fn execute_passes_error_statuses_through_as_data() {
    let fake = FakeHttp::default();
    *fake.response.lock().unwrap() = Some(Ok(HttpResponse {
        status: 429,
        headers: vec![],
        body: r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow"}}"#
            .to_string(),
        body_bytes: Vec::new(),
    }));
    let bridge = LlmTransportBridge::new(fake);

    let response = llm_runtime::Transport::execute(&bridge, &provider_request())
        .await
        .expect("error status is data, not Err");

    assert_eq!(response.status, 429);
    assert_eq!(response.body_json["error"]["type"], "rate_limit_error");
}

#[tokio::test]
async fn http_status_error_variant_also_becomes_data() {
    let fake = FakeHttp::default();
    *fake.response.lock().unwrap() = Some(Err(HttpError::Status {
        status: 500,
        body: r#"{"type":"error","error":{"type":"api_error","message":"boom"}}"#.to_string(),
    }));
    let bridge = LlmTransportBridge::new(fake);

    let response = llm_runtime::Transport::execute(&bridge, &provider_request())
        .await
        .expect("status error is data");

    assert_eq!(response.status, 500);
    assert_eq!(response.body_json["error"]["type"], "api_error");
}

#[tokio::test]
async fn connection_errors_map_to_llm_transport_error() {
    let fake = FakeHttp::default();
    *fake.response.lock().unwrap() = Some(Err(HttpError::Connection("dns".to_string())));
    let bridge = LlmTransportBridge::new(fake);

    let error = llm_runtime::Transport::execute(&bridge, &provider_request())
        .await
        .expect_err("connection error");

    assert!(
        matches!(error, llm_runtime::LlmError::Transport { message } if message.contains("dns"))
    );
}

#[allow(clippy::unnecessary_wraps)]
fn sse(data: &str) -> Result<SseEvent, HttpError> {
    Ok(SseEvent {
        event_type: Some("message".to_string()),
        data: data.to_string(),
        id: None,
    })
}

#[tokio::test]
async fn open_stream_maps_sse_events_to_frames() {
    let fake = FakeHttp::default();
    // The second payload is an artificial terminator demonstrating verbatim
    // pass-through ([DONE] handling belongs to codecs, not the bridge).
    *fake.sse.lock().unwrap() = Some(Ok(vec![sse(r#"{"type":"message_stop"}"#), sse("[DONE]")]));
    let bridge = LlmTransportBridge::new(fake);

    let mut streaming = llm_runtime::Transport::open_stream(&bridge, &provider_request())
        .await
        .expect("stream");

    assert_eq!(streaming.status, 200);
    let first = streaming.frames.next_frame().await.unwrap().expect("frame");
    assert_eq!(first.bytes, br#"{"type":"message_stop"}"#);
    let second = streaming.frames.next_frame().await.unwrap().expect("frame");
    assert_eq!(second.bytes, b"[DONE]");
    assert!(streaming.frames.next_frame().await.unwrap().is_none());
}

#[tokio::test]
async fn open_stream_status_error_yields_status_and_body_frame() {
    let fake = FakeHttp::default();
    *fake.sse.lock().unwrap() = Some(Err(HttpError::Status {
        status: 429,
        body: r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow"}}"#
            .to_string(),
    }));
    let bridge = LlmTransportBridge::new(fake);

    let mut streaming = llm_runtime::Transport::open_stream(&bridge, &provider_request())
        .await
        .expect("status error is data");

    assert_eq!(streaming.status, 429);
    let body = streaming
        .frames
        .next_frame()
        .await
        .unwrap()
        .expect("body frame");
    assert!(String::from_utf8(body.bytes)
        .unwrap()
        .contains("rate_limit_error"));
    assert!(streaming.frames.next_frame().await.unwrap().is_none());
}

#[tokio::test]
async fn mid_stream_http_error_maps_to_llm_transport_error() {
    let fake = FakeHttp::default();
    *fake.sse.lock().unwrap() = Some(Ok(vec![
        sse(r#"{"type":"message_start","message":{"id":"m","model":"x","content":[],"usage":{}}}"#),
        Err(HttpError::Connection("reset".to_string())),
    ]));
    let bridge = LlmTransportBridge::new(fake);

    let mut streaming = llm_runtime::Transport::open_stream(&bridge, &provider_request())
        .await
        .expect("stream");

    let first = streaming
        .frames
        .next_frame()
        .await
        .unwrap()
        .expect("first frame");
    assert!(String::from_utf8(first.bytes)
        .unwrap()
        .contains("message_start"));
    let error = streaming
        .frames
        .next_frame()
        .await
        .expect_err("mid-stream error");
    assert!(
        matches!(error, llm_runtime::LlmError::Transport { message } if message.contains("reset"))
    );
}

#[tokio::test]
async fn bridge_drives_llm_runtime_event_stream_end_to_end() {
    let fake = FakeHttp::default();
    *fake.sse.lock().unwrap() = Some(Ok(vec![
        sse(
            r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-sonnet-4-20250514","content":[],"usage":{"input_tokens":1,"output_tokens":0}}}"#,
        ),
        sse(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        ),
        sse(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
        ),
        sse(r#"{"type":"content_block_stop","index":0}"#),
        sse(
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":1,"output_tokens":1}}"#,
        ),
        sse(r#"{"type":"message_stop"}"#),
    ]));
    let bridge = LlmTransportBridge::new(fake);

    let client = llm_runtime::client::DefaultLlmClient::from_config(llm_runtime::ClientConfig {
        providers: vec![llm_runtime::ProviderProfile {
            wire_profile: None,
            regions: llm_runtime::Region::all(),
            provider_id: llm_runtime::ProviderId::AnthropicFirstParty,
            profile_name: "anthropic".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
            protocol: llm_runtime::ProtocolFamily::AnthropicMessages,
            auth: llm_runtime::AuthStrategy::None,
            credential: llm_runtime::CredentialConfig::None,
            models: vec![llm_runtime::ModelProfile {
                display_model: "claude-sonnet-4-20250514".to_string(),
                request_model: "claude-sonnet-4-20250514".to_string(),
                billing_model: "claude-sonnet-4".to_string(),
                aliases: vec![],
                description: None,
                metadata: Default::default(),
                capabilities: llm_runtime::Capabilities {
                    streaming: true,
                    tools: true,
                    ..Default::default()
                },
            }],
            pricing: llm_runtime::PricingConfig::default(),
            signing: None,
            azure: None,
            supports_websockets: false,
            supports_websocket_compression: false,
            websocket_connect_timeout_ms: None,
            vision_delegate: None,
            connection: Default::default(),
        }],
    })
    .expect("client");

    let mut request =
        llm_runtime::LlmRequest::new("claude-sonnet-4-20250514").with_user_text("hello");
    request.stream = true;

    let mut events = client
        .execute_stream(&request, &bridge)
        .await
        .expect("stream");
    let mut texts = String::new();
    let mut stop_reason = None;
    while let Some(event) = events.next_event().await.expect("event") {
        match event {
            llm_runtime::LlmEvent::ContentBlockDelta {
                delta: llm_runtime::ContentDelta::TextDelta { text },
                ..
            } => texts.push_str(&text),
            llm_runtime::LlmEvent::MessageDelta { delta, .. } => stop_reason = delta.stop_reason,
            _ => {}
        }
    }
    assert_eq!(texts, "hi");
    assert_eq!(stop_reason.as_deref(), Some("end_turn"));
}

/// When `stream_framing == AwsEventStream` the bridge calls
/// `stream_raw_bytes_with_meta` instead of `stream_sse_with_meta`, and raw
/// byte chunks arrive as `RawStreamFrame`s without SSE splitting.
#[tokio::test]
async fn open_stream_aws_event_stream_routes_to_raw_bytes_path() {
    let chunk1 = b"\x00\x00\x00\x10".to_vec(); // first 4 bytes of a fake frame
    let chunk2 = b"\xFF\xFE\xFD\xFC".to_vec();

    let raw_stream: RawByteStream = Box::pin(futures_util::stream::iter(vec![
        Ok::<Vec<u8>, HttpError>(chunk1.clone()),
        Ok::<Vec<u8>, HttpError>(chunk2.clone()),
    ]));

    let fake = FakeHttp::default();
    *fake.raw.lock().unwrap() = Some(Ok(RawByteStreamWithMeta {
        status: 200,
        headers: vec![("x-amzn-requestid".to_string(), "req-123".to_string())],
        stream: raw_stream,
    }));
    let bridge = LlmTransportBridge::new(fake);

    // Build a request with AwsEventStream framing.
    let mut request = provider_request();
    request.stream_framing = llm_runtime::StreamFraming::AwsEventStream;

    let mut streaming = llm_runtime::Transport::open_stream(&bridge, &request)
        .await
        .expect("raw stream");

    assert_eq!(streaming.status, 200);
    assert_eq!(
        streaming
            .headers
            .get("x-amzn-requestid")
            .map(String::as_str),
        Some("req-123")
    );

    // Frames are raw byte chunks — NOT SSE-parsed.
    let f1 = streaming
        .frames
        .next_frame()
        .await
        .unwrap()
        .expect("chunk1");
    assert_eq!(f1.bytes, chunk1);
    let f2 = streaming
        .frames
        .next_frame()
        .await
        .unwrap()
        .expect("chunk2");
    assert_eq!(f2.bytes, chunk2);
    assert!(streaming.frames.next_frame().await.unwrap().is_none());
}

#[tokio::test]
async fn open_stream_responses_websocket_wraps_request_and_maps_messages() {
    let ws_stream = Box::pin(futures_util::stream::iter(vec![
        Ok::<Vec<u8>, HttpError>(
            br#"{"type":"response.created","response":{"id":"resp_1","model":"gpt-5"}}"#.to_vec(),
        ),
        Ok::<Vec<u8>, HttpError>(
            br#"{"type":"response.completed","response":{"id":"resp_1","model":"gpt-5","status":"completed"}}"#.to_vec(),
        ),
    ]));

    let fake = FakeHttp::default();
    *fake.websocket.lock().unwrap() = Some(Ok(WebSocketMessageStreamWithMeta {
        status: 101,
        headers: vec![("openai-model".to_string(), "gpt-5".to_string())],
        stream: ws_stream,
    }));
    let bridge = LlmTransportBridge::new(fake);

    let mut request = llm_runtime::ProviderRequest::post_json(
        "https://api.openai.com/v1/responses",
        serde_json::json!({"model": "gpt-5", "stream": true}),
    );
    request
        .headers
        .insert("authorization".to_string(), "Bearer test".to_string());
    request.stream_transport = llm_runtime::ProviderStreamTransport::ResponsesWebSocket;
    request.websocket_connect_timeout_ms = Some(42);

    let mut streaming = llm_runtime::Transport::open_stream(&bridge, &request)
        .await
        .expect("websocket stream");

    assert_eq!(streaming.status, 101);
    assert_eq!(
        streaming.headers.get("openai-model").map(String::as_str),
        Some("gpt-5")
    );
    let first = streaming.frames.next_frame().await.unwrap().expect("first");
    assert!(String::from_utf8(first.bytes)
        .unwrap()
        .contains("response.created"));
    let second = streaming
        .frames
        .next_frame()
        .await
        .unwrap()
        .expect("second");
    assert!(String::from_utf8(second.bytes)
        .unwrap()
        .contains("response.completed"));
    assert!(streaming.frames.next_frame().await.unwrap().is_none());

    let seen = bridge
        .inner()
        .websocket_seen
        .lock()
        .unwrap()
        .take()
        .expect("websocket request sent");
    assert_eq!(seen.url, "https://api.openai.com/v1/responses");
    assert_eq!(seen.timeout, Some(std::time::Duration::from_millis(42)));
    assert!(seen
        .headers
        .iter()
        .any(|(k, v)| k == "authorization" && v == "Bearer test"));
    let body: serde_json::Value =
        serde_json::from_str(seen.body.as_deref().expect("websocket body")).unwrap();
    assert_eq!(body["type"], "response.create");
    assert_eq!(body["model"], "gpt-5");
    assert_eq!(body["stream"], true);
}

#[tokio::test]
async fn open_stream_responses_websocket_426_falls_back_to_http_sse_once() {
    let fake = FakeHttp::default();
    *fake.websocket.lock().unwrap() = Some(Err(HttpError::Status {
        status: 426,
        body: "upgrade required".to_string(),
    }));
    *fake.sse.lock().unwrap() = Some(Ok(vec![sse(r#"{"type":"fallback"}"#)]));
    let bridge = LlmTransportBridge::new(fake);

    let mut request = llm_runtime::ProviderRequest::post_json(
        "https://api.openai.com/v1/responses",
        serde_json::json!({"model": "gpt-5", "stream": true}),
    );
    request.stream_transport = llm_runtime::ProviderStreamTransport::ResponsesWebSocket;

    let mut streaming = llm_runtime::Transport::open_stream(&bridge, &request)
        .await
        .expect("fallback stream");

    assert_eq!(streaming.status, 200);
    let frame = streaming
        .frames
        .next_frame()
        .await
        .unwrap()
        .expect("fallback frame");
    assert_eq!(frame.bytes, br#"{"type":"fallback"}"#);
    assert!(streaming.frames.next_frame().await.unwrap().is_none());

    let websocket_seen = bridge
        .inner()
        .websocket_seen
        .lock()
        .unwrap()
        .take()
        .expect("websocket attempted first");
    let body: serde_json::Value =
        serde_json::from_str(websocket_seen.body.as_deref().expect("websocket body")).unwrap();
    assert_eq!(body["type"], "response.create");
    let http_seen = bridge
        .inner()
        .seen
        .lock()
        .unwrap()
        .take()
        .expect("sse fallback sent");
    let http_body: serde_json::Value =
        serde_json::from_str(http_seen.body.as_deref().expect("http body")).unwrap();
    assert_eq!(
        http_body,
        serde_json::json!({"model": "gpt-5", "stream": true})
    );
}

#[derive(Debug, Default)]
struct SeenWebSocketHandshake {
    uri: String,
    headers: std::collections::BTreeMap<String, String>,
}

#[tokio::test]
async fn reqwest_http_responses_websocket_converts_url_headers_and_streams_text() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
    use tokio_tungstenite::tungstenite::Message;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(SeenWebSocketHandshake::default()));
    let first_message = Arc::new(Mutex::new(None::<String>));

    let server_seen = Arc::clone(&seen);
    let server_first_message = Arc::clone(&first_message);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let callback = move |req: &Request, mut response: Response| {
            let mut headers = std::collections::BTreeMap::new();
            for (name, value) in req.headers() {
                headers.insert(
                    name.as_str().to_ascii_lowercase(),
                    value.to_str().unwrap_or("").to_string(),
                );
            }
            *server_seen.lock().unwrap() = SeenWebSocketHandshake {
                uri: req.uri().to_string(),
                headers,
            };
            response
                .headers_mut()
                .insert("openai-model", http::HeaderValue::from_static("gpt-5"));
            response
                .headers_mut()
                .insert("x-models-etag", http::HeaderValue::from_static("etag-ws"));
            Ok(response)
        };
        let mut socket = tokio_tungstenite::accept_hdr_async(stream, callback)
            .await
            .unwrap();
        let message = socket.next().await.unwrap().unwrap();
        let Message::Text(text) = message else {
            panic!("expected response.create text frame, got {message:?}");
        };
        *server_first_message.lock().unwrap() = Some(text);

        socket.send(Message::Ping(vec![1, 2, 3])).await.unwrap();
        let pong = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
            .await
            .expect("pong timeout")
            .expect("pong message")
            .expect("pong ok");
        assert_eq!(pong, Message::Pong(vec![1, 2, 3]));

        socket
            .send(Message::Text(
                r#"{"type":"response.created","response":{"id":"resp_1","model":"gpt-5"}}"#
                    .to_string(),
            ))
            .await
            .unwrap();
        socket
            .send(Message::Text(
                r#"{"type":"response.completed","response":{"id":"resp_1","model":"gpt-5","status":"completed"}}"#
                    .to_string(),
            ))
            .await
            .unwrap();
    });

    let transport = ReqwestHttp::new();
    let mut response = transport
        .stream_websocket_messages_with_meta(HttpRequest {
            method: protocol::HttpMethod::Post,
            url: format!("http://{addr}/v1/responses?query=1"),
            headers: vec![
                ("authorization".to_string(), "Bearer test".to_string()),
                ("content-type".to_string(), "application/json".to_string()),
                ("x-custom".to_string(), "custom".to_string()),
                ("OpenAI-Beta".to_string(), "existing_beta=1".to_string()),
            ],
            body: Some(r#"{"type":"response.create","model":"gpt-5"}"#.to_string()),
            body_bytes: None,
            timeout: Some(std::time::Duration::from_secs(3)),
        })
        .await
        .expect("websocket open");

    assert_eq!(response.status, 101);
    assert_eq!(
        response
            .headers
            .iter()
            .find(|(name, _)| name == "openai-model")
            .map(|(_, value)| value.as_str()),
        Some("gpt-5")
    );
    assert_eq!(
        response
            .headers
            .iter()
            .find(|(name, _)| name == "x-models-etag")
            .map(|(_, value)| value.as_str()),
        Some("etag-ws")
    );

    let created = response.stream.next().await.unwrap().unwrap();
    assert!(String::from_utf8(created)
        .unwrap()
        .contains("response.created"));
    let completed = response.stream.next().await.unwrap().unwrap();
    assert!(String::from_utf8(completed)
        .unwrap()
        .contains("response.completed"));
    assert!(response.stream.next().await.is_none());
    server.await.unwrap();

    let seen = seen.lock().unwrap();
    assert_eq!(seen.uri, "/v1/responses?query=1");
    assert_eq!(
        seen.headers.get("authorization").map(String::as_str),
        Some("Bearer test")
    );
    assert_eq!(
        seen.headers.get("x-custom").map(String::as_str),
        Some("custom")
    );
    assert!(
        !seen.headers.contains_key("content-type"),
        "body-only content-type header must not be forwarded to websocket handshake"
    );
    let beta = seen
        .headers
        .get("openai-beta")
        .expect("OpenAI-Beta websocket header");
    assert!(beta.contains("existing_beta=1"));
    assert!(beta.contains("responses_websockets=2026-02-06"));
    let payload: serde_json::Value = serde_json::from_str(
        first_message
            .lock()
            .unwrap()
            .as_deref()
            .expect("first websocket text frame"),
    )
    .unwrap();
    assert_eq!(payload["type"], "response.create");
    assert_eq!(payload["model"], "gpt-5");
}

#[tokio::test]
async fn reqwest_http_responses_websocket_connection_reuses_single_socket() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
    use tokio_tungstenite::tungstenite::Message;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(SeenWebSocketHandshake::default()));
    let accept_count = Arc::new(Mutex::new(0_u32));
    let (message_tx, mut message_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    let server_seen = Arc::clone(&seen);
    let server_accept_count = Arc::clone(&accept_count);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        *server_accept_count.lock().unwrap() += 1;
        let callback = move |req: &Request, response: Response| {
            let mut headers = std::collections::BTreeMap::new();
            for (name, value) in req.headers() {
                headers.insert(
                    name.as_str().to_ascii_lowercase(),
                    value.to_str().unwrap_or("").to_string(),
                );
            }
            *server_seen.lock().unwrap() = SeenWebSocketHandshake {
                uri: req.uri().to_string(),
                headers,
            };
            Ok(response)
        };
        let mut socket = tokio_tungstenite::accept_hdr_async(stream, callback)
            .await
            .unwrap();

        for idx in 1..=2 {
            let message = socket.next().await.unwrap().unwrap();
            let Message::Text(text) = message else {
                panic!("expected text frame, got {message:?}");
            };
            message_tx.send(text).unwrap();
            socket
                .send(Message::Text(format!(
                    r#"{{"type":"response.created","response":{{"id":"resp_{idx}","model":"gpt-5"}}}}"#
                )))
                .await
                .unwrap();
            socket
                .send(Message::Text(format!(
                    r#"{{"type":"response.completed","response":{{"id":"resp_{idx}","model":"gpt-5","status":"completed"}}}}"#
                )))
                .await
                .unwrap();
        }
    });

    let transport = ReqwestHttp::new();
    let mut connection = transport
        .open_websocket_connection_with_meta(HttpRequest {
            method: protocol::HttpMethod::Post,
            url: format!("http://{addr}/v1/responses"),
            headers: vec![("authorization".to_string(), "Bearer test".to_string())],
            body: None,
            body_bytes: None,
            timeout: Some(std::time::Duration::from_secs(3)),
        })
        .await
        .expect("websocket preconnect");

    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), message_rx.recv())
            .await
            .is_err(),
        "preconnect must not send a response.create body"
    );

    let mut first = connection
        .connection
        .send_text_with_meta(r#"{"type":"response.create","model":"gpt-5","n":1}"#.to_string())
        .await
        .expect("first send");
    assert!(
        String::from_utf8(first.stream.next().await.unwrap().unwrap())
            .unwrap()
            .contains("response.created")
    );
    assert!(
        String::from_utf8(first.stream.next().await.unwrap().unwrap())
            .unwrap()
            .contains("response.completed")
    );

    let mut second = connection
        .connection
        .send_text_with_meta(r#"{"type":"response.create","model":"gpt-5","n":2}"#.to_string())
        .await
        .expect("second send");
    assert!(
        String::from_utf8(second.stream.next().await.unwrap().unwrap())
            .unwrap()
            .contains("response.created")
    );
    assert!(
        String::from_utf8(second.stream.next().await.unwrap().unwrap())
            .unwrap()
            .contains("response.completed")
    );

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&message_rx.recv().await.unwrap()).unwrap()["n"],
        1
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&message_rx.recv().await.unwrap()).unwrap()["n"],
        2
    );
    server.await.unwrap();

    assert_eq!(*accept_count.lock().unwrap(), 1);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.uri, "/v1/responses");
    assert_eq!(
        seen.headers.get("authorization").map(String::as_str),
        Some("Bearer test")
    );
    assert!(
        !seen
            .headers
            .get("sec-websocket-extensions")
            .is_some_and(|value| value.contains("permessage-deflate")),
        "tokio-tungstenite 0.21 exposes no stable permessage-deflate config; lock disabled behavior"
    );
}

#[tokio::test]
async fn reqwest_http_responses_websocket_rejects_binary_frames() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let _ = socket.next().await.unwrap().unwrap();
        socket.send(Message::Binary(vec![0, 1, 2])).await.unwrap();
    });

    let transport = ReqwestHttp::new();
    let mut response = transport
        .stream_websocket_messages_with_meta(HttpRequest {
            method: protocol::HttpMethod::Post,
            url: format!("http://{addr}/v1/responses"),
            headers: vec![],
            body: Some(r#"{"type":"response.create","model":"gpt-5"}"#.to_string()),
            body_bytes: None,
            timeout: Some(std::time::Duration::from_secs(3)),
        })
        .await
        .expect("websocket open");

    let err = response.stream.next().await.unwrap().unwrap_err();
    assert!(matches!(err, HttpError::InvalidResponse(message) if message.contains("binary")));
    server.await.unwrap();
}

#[tokio::test]
async fn reqwest_http_responses_websocket_errors_on_close_before_terminal_event() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let _ = socket.next().await.unwrap().unwrap();
        socket
            .send(Message::Text(
                r#"{"type":"response.created","response":{"id":"resp_1","model":"gpt-5"}}"#
                    .to_string(),
            ))
            .await
            .unwrap();
        socket.close(None).await.unwrap();
    });

    let transport = ReqwestHttp::new();
    let mut response = transport
        .stream_websocket_messages_with_meta(HttpRequest {
            method: protocol::HttpMethod::Post,
            url: format!("http://{addr}/v1/responses"),
            headers: vec![],
            body: Some(r#"{"type":"response.create","model":"gpt-5"}"#.to_string()),
            body_bytes: None,
            timeout: Some(std::time::Duration::from_secs(3)),
        })
        .await
        .expect("websocket open");

    let first = response.stream.next().await.unwrap().unwrap();
    assert!(String::from_utf8(first)
        .unwrap()
        .contains("response.created"));
    let err = response.stream.next().await.unwrap().unwrap_err();
    assert!(
        matches!(err, HttpError::Connection(message) if message.contains("before response.completed"))
    );
    server.await.unwrap();
}

#[tokio::test]
#[ignore = "requires OPENAI_API_KEY; optional OPENAI_RESPONSES_WS_MODEL"]
async fn live_openai_responses_websocket_smoke_env_gated() {
    use futures_util::StreamExt;

    let Ok(api_key) = std::env::var("OPENAI_API_KEY") else {
        eprintln!("skipping live OpenAI Responses WebSocket smoke: OPENAI_API_KEY is not set");
        return;
    };
    let model = std::env::var("OPENAI_RESPONSES_WS_MODEL").unwrap_or_else(|_| "gpt-5".to_string());

    let transport = ReqwestHttp::new();
    let mut response = transport
        .stream_websocket_messages_with_meta(HttpRequest {
            method: protocol::HttpMethod::Post,
            url: "https://api.openai.com/v1/responses".to_string(),
            headers: vec![("authorization".to_string(), format!("Bearer {api_key}"))],
            body: Some(
                serde_json::json!({
                    "type": "response.create",
                    "model": model,
                    "input": [{
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "Reply with exactly: ok"}]
                    }],
                    "stream": true,
                    "store": false
                })
                .to_string(),
            ),
            body_bytes: None,
            timeout: Some(std::time::Duration::from_secs(15)),
        })
        .await
        .expect("live websocket open");

    assert_eq!(response.status, 101);
    let mut saw_completed = false;
    let mut saw_text_event = false;
    while let Some(frame) = response.stream.next().await {
        let bytes = frame.expect("live websocket frame");
        let text = String::from_utf8(bytes).expect("utf8 frame");
        let value: serde_json::Value = serde_json::from_str(&text).expect("json frame");
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("response.output_text.delta") => saw_text_event = true,
            Some("response.completed") => {
                saw_completed = true;
                break;
            }
            Some("error") => panic!("live websocket error frame: {value}"),
            _ => {}
        }
    }

    assert!(saw_completed, "live websocket must complete");
    assert!(saw_text_event, "live websocket should stream text");
    assert!(
        response
            .headers
            .iter()
            .any(|(name, _)| name == "openai-model")
            || response
                .headers
                .iter()
                .any(|(name, _)| name == "x-models-etag"),
        "live websocket should expose provider metadata headers when present"
    );
}
