//! Capability authority reaches real HTTP handshakes and URL input-required flows.
use async_trait::async_trait;
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use lingxi_core::host::{
    mcp_result::{drive_modern_request, JsonrpcMcpResultIo, McpInputRequiredOptions},
    McpConnectOptions, McpElicitationCapabilities, McpElicitationMode, McpHeaders, McpProtocolEra,
    McpTransport, McpTransportSpec,
};
use platform_common::RemoteMcpTransport;
use serde_json::{json, Value};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

const INPUT_KEY: &str = "original-url-key";
const INPUT_MESSAGE: &str = "Continue this operation in the browser";
const INPUT_URL: &str = "https://example.test/elicitation";
const CAPABILITIES_KEY: &str = "io.modelcontextprotocol/clientCapabilities";
const PROTOCOL_KEY: &str = "io.modelcontextprotocol/protocolVersion";
const WAIT: Duration = Duration::from_secs(3);

#[derive(Clone)]
struct CapturedRequest {
    bytes: Vec<u8>,
    body: Value,
    headers: HeaderMap,
}

#[derive(Clone, Default)]
struct WireState {
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
}

async fn reply(State(state): State<WireState>, headers: HeaderMap, bytes: Bytes) -> Response {
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    state.requests.lock().unwrap().push(CapturedRequest {
        bytes: bytes.to_vec(),
        body: body.clone(),
        headers,
    });
    let Some(id) = body.get("id") else {
        return StatusCode::ACCEPTED.into_response();
    };
    let result = match body["method"].as_str().unwrap_or("") {
        "initialize" => json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "elicitation-wire", "version": "1"},
        }),
        "server/discover" => json!({
            "resultType": "complete",
            "supportedVersions": ["2026-07-28"],
            "capabilities": {"tools": {}},
            "_meta": {
                "io.modelcontextprotocol/serverInfo": {"name": "elicitation-wire", "version": "1"},
            },
        }),
        "tools/call" if body["params"].get("inputResponses").is_none() => json!({
            "resultType": "input_required",
            "requestState": "opaque-url-state",
            "inputRequests": {
                INPUT_KEY: {
                    "method": "elicitation/create",
                    "id": "untrusted-wire-id",
                    "params": {
                        "mode": "url",
                        "message": INPUT_MESSAGE,
                        // Native 2.1.287 URL request schema requires this field.
                        "elicitationId": "native-url-elicitation",
                        "url": INPUT_URL,
                    },
                },
            },
        }),
        "tools/call" => json!({
            "resultType": "complete",
            "content": [{"type": "text", "text": "cancel received"}],
        }),
        method => panic!("unexpected wire request {method}"),
    };
    Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
}

struct WireServer {
    url: String,
    state: WireState,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for WireServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl WireServer {
    async fn start() -> Self {
        let state = WireState::default();
        let app = Router::new()
            .route("/mcp", post(reply))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            url: format!("http://{address}/mcp"),
            state,
            task,
        }
    }

    fn spec(&self) -> McpTransportSpec {
        let mut headers = McpHeaders::new();
        headers.insert("X-Elicitation-Test".into(), "retained".into());
        McpTransportSpec::Http {
            url: self.url.clone(),
            headers,
            headers_helper: None,
            oauth: None,
        }
    }

    fn requests(&self) -> Vec<CapturedRequest> {
        self.state.requests.lock().unwrap().clone()
    }
}

fn assert_legacy_wire(server: &WireServer, expected: Value, exact_bytes: &[u8]) {
    let requests = server.requests();
    let initializes: Vec<_> = requests
        .iter()
        .filter(|request| request.body["method"] == "initialize")
        .collect();
    assert_eq!(initializes.len(), 1);
    let request = initializes[0];
    assert_eq!(request.body["params"]["protocolVersion"], "2025-11-25");
    assert_eq!(request.body["params"]["capabilities"], expected);
    assert!(request
        .bytes
        .windows(exact_bytes.len())
        .any(|part| part == exact_bytes));
    assert_eq!(request.headers["x-elicitation-test"], "retained");
    assert!(!requests
        .iter()
        .any(|request| request.body["method"] == "server/discover"));
}

#[tokio::test]
async fn direct_http_initialize_advertises_full_elicitation_bytes() {
    let server = WireServer::start().await;
    let transport = RemoteMcpTransport::default();
    let raw = tokio::time::timeout(WAIT, transport.connect(&server.spec()))
        .await
        .unwrap()
        .unwrap();
    let capabilities = tokio::time::timeout(WAIT, transport.initialize(&raw))
        .await
        .unwrap()
        .unwrap();
    assert!(capabilities.tools);
    assert_eq!(
        transport
            .server_metadata(raw.connection_id)
            .unwrap()
            .server_info,
        Some(json!({"name": "elicitation-wire", "version": "1"})),
    );
    assert_legacy_wire(
        &server,
        json!({"roots":{"listChanged":true},"elicitation":{"form":{},"url":{}}}),
        br#""capabilities":{"roots":{"listChanged":true},"elicitation":{"form":{},"url":{}}}"#,
    );
    transport.disconnect(raw.connection_id).await.unwrap();
}

#[tokio::test]
async fn explicit_bare_http_initialize_advertises_empty_elicitation_bytes() {
    let server = WireServer::start().await;
    let transport = RemoteMcpTransport::default();
    let result = tokio::time::timeout(
        WAIT,
        transport.connect_and_initialize(
            &server.spec(),
            McpConnectOptions {
                expected_era: Some(McpProtocolEra::Legacy),
                deadline_ms: 2_000,
                probe_timeout_ms: None,
                elicitation: McpElicitationCapabilities {
                    legacy: McpElicitationMode::Bare,
                    modern: McpElicitationMode::FormAndUrl,
                },
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result.negotiated.era, McpProtocolEra::Legacy);
    assert_eq!(result.negotiated.version, "2025-11-25");
    assert!(result.capabilities.tools);
    assert_eq!(
        transport
            .server_metadata(result.connection.connection_id)
            .unwrap()
            .server_info,
        Some(json!({"name": "elicitation-wire", "version": "1"})),
    );
    assert_legacy_wire(
        &server,
        json!({"roots":{"listChanged":true},"elicitation":{}}),
        br#""capabilities":{"roots":{"listChanged":true},"elicitation":{}}"#,
    );
    transport
        .disconnect(result.connection.connection_id)
        .await
        .unwrap();
}

struct CancelUrl {
    requests: Arc<Mutex<Vec<jsonrpc::Request>>>,
}

#[async_trait]
impl jsonrpc::InboundHandler for CancelUrl {
    async fn handle(&self, request: jsonrpc::Request) -> jsonrpc::Response {
        assert_eq!(request.method, "elicitation/create");
        assert_eq!(request.id, jsonrpc::Id::String(INPUT_KEY.into()));
        let params = request.params.as_ref().unwrap();
        assert_eq!(params["mode"], "url");
        assert_eq!(params["message"], INPUT_MESSAGE);
        assert_eq!(params["url"], INPUT_URL);
        let id = request.id.clone();
        self.requests.lock().unwrap().push(request);
        // This trusted local callback has no browser UI. Its actual decision
        // is cancellation and must reach the peer as the input response.
        jsonrpc::Response::success(id, json!({"action": "cancel"}))
    }
}

async fn modern_url_flow(use_open_driver: bool) {
    let server = WireServer::start().await;
    let transport = RemoteMcpTransport::default();
    let result = tokio::time::timeout(
        WAIT,
        transport.connect_and_initialize(
            &server.spec(),
            McpConnectOptions {
                expected_era: Some(McpProtocolEra::Modern),
                deadline_ms: 2_000,
                probe_timeout_ms: Some(2_000),
                elicitation: McpElicitationCapabilities {
                    legacy: McpElicitationMode::Bare,
                    modern: McpElicitationMode::FormAndUrl,
                },
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result.negotiated.era, McpProtocolEra::Modern);
    assert_eq!(result.negotiated.version, "2026-07-28");
    let discovery = server
        .requests()
        .into_iter()
        .find(|request| request.body["method"] == "server/discover")
        .unwrap();
    let metadata = discovery.body["params"]["_meta"].clone();
    assert_eq!(
        metadata[CAPABILITIES_KEY]["elicitation"],
        json!({"form": {}, "url": {}})
    );
    let connection = transport
        .connection_for(result.connection.connection_id)
        .unwrap();
    let local_requests = Arc::new(Mutex::new(Vec::new()));
    connection
        .register_handler(
            "elicitation/create",
            Arc::new(CancelUrl {
                requests: local_requests.clone(),
            }),
        )
        .await;
    if use_open_driver {
        // Feed the open result driver the capability object actually announced
        // by this connection, including its independently frozen modern mode.
        let io = JsonrpcMcpResultIo {
            connection,
            client_capabilities: metadata[CAPABILITIES_KEY].clone(),
        };
        let value = tokio::time::timeout(
            WAIT,
            drive_modern_request(
                &io,
                "tools/call",
                json!({"name": "worker", "arguments": {"q": "original"}, "_meta": metadata}),
                McpInputRequiredOptions {
                    per_request_timeout: Duration::from_secs(2),
                    max_total_timeout: Some(Duration::from_secs(2)),
                    ..Default::default()
                },
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            value,
            json!({"content": [{"type": "text", "text": "cancel received"}]})
        );
    } else {
        let value = tokio::time::timeout(
            WAIT,
            transport.call_tool(&result.connection, "worker", json!({"q": "original"})),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            value.content,
            json!([{"type": "text", "text": "cancel received"}])
        );
    }
    assert_eq!(local_requests.lock().unwrap().len(), 1);
    let requests = server.requests();
    assert_eq!(requests.len(), 3, "one discovery and two tool POSTs");
    let tools: Vec<_> = requests
        .iter()
        .filter(|request| request.body["method"] == "tools/call")
        .collect();
    assert_eq!(tools.len(), 2);
    for request in &requests {
        assert_eq!(request.headers["x-elicitation-test"], "retained");
        assert_eq!(request.body["params"]["_meta"][PROTOCOL_KEY], "2026-07-28");
        assert_eq!(
            request.body["params"]["_meta"][CAPABILITIES_KEY],
            json!({"roots":{"listChanged":true},"elicitation":{"form":{},"url":{}}})
        );
        let full_caps = br#""io.modelcontextprotocol/clientCapabilities":{"roots":{"listChanged":true},"elicitation":{"form":{},"url":{}}}"#;
        assert!(request
            .bytes
            .windows(full_caps.len())
            .any(|bytes| bytes == full_caps));
    }
    for request in &tools {
        assert_eq!(request.body["params"]["name"], "worker");
        assert_eq!(
            request.body["params"]["arguments"],
            json!({"q": "original"})
        );
    }
    assert!(tools[0].body["params"].get("inputResponses").is_none());
    assert_eq!(tools[1].body["params"]["requestState"], "opaque-url-state");
    assert_eq!(
        tools[1].body["params"]["inputResponses"],
        json!({INPUT_KEY: {"action": "cancel"}})
    );
    transport
        .disconnect(result.connection.connection_id)
        .await
        .unwrap();
}

#[tokio::test]
async fn modern_remote_url_fulfilment_uses_modern_capability_despite_legacy_bare() {
    modern_url_flow(false).await;
}

#[tokio::test]
async fn modern_open_driver_url_fulfilment_uses_announced_capabilities() {
    modern_url_flow(true).await;
}
