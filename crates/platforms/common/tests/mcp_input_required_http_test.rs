//! Ordinary remote transport uses the same local registry and retains headers.
use async_trait::async_trait;
use axum::{extract::State, http::HeaderMap, routing::post, Json, Router};
use lingxi_core::host::{
    McpConnectOptions, McpError, McpProtocolEra, McpTransport, McpTransportSpec,
};
use platform_common::RemoteMcpTransport;
use serde_json::{json, Value};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
#[derive(Clone, Default)]
struct Mock {
    calls: Arc<Mutex<Vec<(Value, HeaderMap)>>>,
}
async fn reply(
    State(state): State<Mock>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    state.calls.lock().unwrap().push((body.clone(), headers));
    let result = match body["method"].as_str().unwrap_or("") {
        "server/discover" => {
            json!({"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}}})
        }
        "tools/call" if body["params"].get("inputResponses").is_none() => {
            json!({"resultType":"input_required","requestState":"opaque","inputRequests":{"original-key":{"method":"roots/list","id":"untrusted"}}})
        }
        "tools/call" => {
            assert_eq!(
                body["params"]["inputResponses"]["original-key"],
                json!({"roots":[]})
            );
            json!({"resultType":"complete","content":[{"type":"text","text":"done"}]})
        }
        _ => json!({}),
    };
    Json(json!({"jsonrpc":"2.0","id":body["id"],"result":result}))
}
struct Roots(Arc<Mutex<Vec<jsonrpc::Id>>>);
#[async_trait]
impl jsonrpc::InboundHandler for Roots {
    async fn handle(&self, request: jsonrpc::Request) -> jsonrpc::Response {
        self.0.lock().unwrap().push(request.id.clone());
        jsonrpc::Response::success(request.id, json!({"roots":[]}))
    }
}
async fn setup() -> (
    RemoteMcpTransport,
    lingxi_core::host::McpRawConnection,
    Mock,
) {
    let state = Mock::default();
    let app = Router::new()
        .route("/mcp", post(reply))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let transport = RemoteMcpTransport::default();
    let mut headers = lingxi_core::host::McpHeaders::new();
    headers.insert("X-Ordinary-Input".into(), "retained".into());
    let result = transport
        .connect_and_initialize(
            &McpTransportSpec::Http {
                url: format!("http://{addr}/mcp"),
                headers,
                headers_helper: None,
                oauth: None,
            },
            McpConnectOptions {
                expected_era: Some(McpProtocolEra::Modern),
                deadline_ms: 5000,
                probe_timeout_ms: Some(5000),
                elicitation: lingxi_core::host::McpElicitationCapabilities::default(),
            },
        )
        .await
        .unwrap();
    assert_eq!(result.negotiated.era, McpProtocolEra::Modern);
    (transport, result.connection, state)
}
#[tokio::test]
async fn ordinary_http_tool_fulfils_registered_local_handler_and_retains_headers() {
    let (transport, raw, state) = setup().await;
    let ids = Arc::new(Mutex::new(Vec::new()));
    transport
        .connection_for(raw.connection_id)
        .unwrap()
        .register_handler("roots/list", Arc::new(Roots(ids.clone())))
        .await;
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        transport.call_tool(&raw, "worker", json!({"q":"original"})),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result.content[0]["text"], "done");
    assert_eq!(
        *ids.lock().unwrap(),
        vec![jsonrpc::Id::String("original-key".into())]
    );
    let calls = state.calls.lock().unwrap();
    let tools: Vec<_> = calls
        .iter()
        .filter(|(v, _)| v["method"] == "tools/call")
        .collect();
    assert_eq!(tools.len(), 2);
    for (body, headers) in tools {
        assert_eq!(headers["x-ordinary-input"], "retained");
        assert_eq!(body["params"]["arguments"], json!({"q":"original"}));
        assert_eq!(
            body["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"],
            "2026-07-28"
        );
    }
    assert!(!calls.iter().any(|(v, _)| v["method"] == "roots/list"));
}
#[tokio::test]
async fn ordinary_http_missing_local_handler_is_native_capability_error() {
    let (transport, raw, state) = setup().await;
    let error = transport
        .call_tool(&raw, "worker", json!({}))
        .await
        .unwrap_err();
    let McpError::Result(error) = error else {
        panic!("typed native error expected")
    };
    assert_eq!(error.code, "CAPABILITY_NOT_SUPPORTED");
    assert_eq!(
        error.data,
        Some(json!({"key":"original-key","method":"roots/list"}))
    );
    assert_eq!(
        state
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(v, _)| v["method"] == "tools/call")
            .count(),
        1
    );
}

async fn read_http_request(stream: &mut tokio::net::TcpStream) -> Value {
    use tokio::io::AsyncReadExt;
    let mut headers = Vec::new();
    loop {
        let mut byte = [0];
        stream.read_exact(&mut byte).await.unwrap();
        headers.push(byte[0]);
        if headers.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let headers = String::from_utf8(headers).unwrap();
    let length = headers
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                .map(|(_, value)| value.trim().parse::<usize>().unwrap())
        })
        .unwrap();
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}
async fn write_http_result(stream: &mut tokio::net::TcpStream, id: &Value, result: Value) {
    use tokio::io::AsyncWriteExt;
    let body = json!({"jsonrpc":"2.0","id":id,"result":result}).to_string();
    let response=format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",body.len());
    stream.write_all(response.as_bytes()).await.unwrap();
}
#[tokio::test]
async fn retry_timeout_aborts_actual_http_socket_and_keeps_connection_usable() {
    use lingxi_core::host::mcp_result::{
        drive_modern_request, JsonrpcMcpResultIo, McpInputRequiredOptions, McpResultError,
    };
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let requests = captured.clone();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let first = read_http_request(&mut socket).await;
        requests.lock().unwrap().push(first.clone());
        write_http_result(
            &mut socket,
            &first["id"],
            json!({"resultType":"input_required","inputRequests":{"r":{"method":"roots/list"}}}),
        )
        .await;
        let retry = read_http_request(&mut socket).await;
        requests.lock().unwrap().push(retry);
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), socket.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0,
            "cancel must disconnect this POST before response headers"
        );
        let (mut socket, _) = listener.accept().await.unwrap();
        let followup = read_http_request(&mut socket).await;
        requests.lock().unwrap().push(followup.clone());
        write_http_result(
            &mut socket,
            &followup["id"],
            json!({"resultType":"complete","content":[]}),
        )
        .await;
    });
    let connection = Arc::new(
        platform_common::connect_http(
            &format!("http://{address}/mcp"),
            None,
            &std::collections::HashMap::<String, String>::new(),
            None,
        )
        .await
        .unwrap(),
    );
    connection
        .register_handler(
            "roots/list",
            Arc::new(Roots(Arc::new(Mutex::new(Vec::new())))),
        )
        .await;
    let io = JsonrpcMcpResultIo {
        connection: connection.clone(),
        client_capabilities: json!({"roots":{}}),
    };
    let error = drive_modern_request(
        &io,
        "tools/call",
        json!({"name":"worker"}),
        McpInputRequiredOptions {
            per_request_timeout: Duration::from_millis(50),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(error,McpResultError::Sdk(error) if error.code=="REQUEST_TIMEOUT"));
    let result = drive_modern_request(
        &io,
        "tools/call",
        json!({"name":"followup"}),
        McpInputRequiredOptions {
            per_request_timeout: Duration::from_secs(1),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(result, json!({"content":[]}));
    server.await.unwrap();
    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests.iter().all(|r| r["method"] == "tools/call"));
}
#[tokio::test]
async fn cancelled_queued_post_never_reaches_http_server() {
    let state = Mock::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/mcp", post(reply))
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let connection = platform_common::connect_http(
        &format!("http://{address}/mcp"),
        None,
        &std::collections::HashMap::<String, String>::new(),
        None,
    )
    .await
    .unwrap();
    // This current-thread runtime cannot poll the transport between enqueue and
    // synchronous drop: the queued id's token is cancelled before its POST starts.
    let expired = connection
        .start_call_unbounded("expired", json!({}))
        .unwrap();
    drop(expired);
    let _: Value = connection.call("live", json!({})).await.unwrap();
    let calls = state.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0["method"], "live");
    assert_eq!(calls[0].0["id"], 2);
    connection.close();
    server.abort();
}

#[derive(Clone, Default)]
struct OpenStream {
    requests: Arc<Mutex<Vec<Value>>>,
    closed: Arc<std::sync::atomic::AtomicUsize>,
    released: Arc<tokio::sync::Notify>,
}
struct StreamGuard(Arc<std::sync::atomic::AtomicUsize>);
impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}
async fn open_stream_reply(
    State(state): State<OpenStream>,
    Json(request): Json<Value>,
) -> axum::response::Response {
    use axum::response::{
        sse::{Event, Sse},
        IntoResponse,
    };
    state.requests.lock().unwrap().push(request.clone());
    if request["params"].get("inputResponses").is_some() {
        return Json(json!({"jsonrpc":"2.0","id":request["id"],"result":{"resultType":"complete","content":[]}})).into_response();
    }
    let result = if request["params"]["name"] == "flow" {
        json!({"resultType":"input_required","inputRequests":{"r":{"method":"roots/list"}}})
    } else {
        json!({"resultType":"complete","content":[]})
    };
    let stream = futures::stream::unfold(
        (0, StreamGuard(state.closed.clone()), state, request, result),
        |(phase, guard, state, request, result)| async move {
            let value = match phase {
                0 => json!({"jsonrpc":"2.0","id":request["id"],"result":result}),
                1 => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed","params":{}})
                }
                _ => {
                    state.released.notified().await;
                    return None;
                }
            };
            Some((
                Ok::<_, std::convert::Infallible>(Event::default().data(value.to_string())),
                (phase + 1, guard, state, request, result),
            ))
        },
    );
    Sse::new(stream).into_response()
}
#[tokio::test]
async fn successful_response_keeps_sse_notifications_and_input_required_retry_runs_concurrently() {
    use lingxi_core::host::mcp_result::{
        drive_modern_request, JsonrpcMcpResultIo, McpInputRequiredOptions,
    };
    for name in ["complete", "flow"] {
        let state = OpenStream::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/mcp", post(open_stream_reply))
            .with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let connection = Arc::new(
            platform_common::connect_http(
                &format!("http://{address}/mcp"),
                None,
                &std::collections::HashMap::<String, String>::new(),
                None,
            )
            .await
            .unwrap(),
        );
        connection
            .register_handler(
                "roots/list",
                Arc::new(Roots(Arc::new(Mutex::new(Vec::new())))),
            )
            .await;
        let mut notifications = connection.notifications();
        let io = JsonrpcMcpResultIo {
            connection: connection.clone(),
            client_capabilities: json!({"roots":{}}),
        };
        let result = drive_modern_request(
            &io,
            "tools/call",
            json!({"name":name}),
            McpInputRequiredOptions {
                per_request_timeout: Duration::from_secs(1),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(result, json!({"content":[]}));
        let note = tokio::time::timeout(Duration::from_secs(1), notifications.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(note.method, "notifications/tools/list_changed");
        assert_eq!(
            state.closed.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "successful response must not abort its request stream"
        );
        assert_eq!(
            state.requests.lock().unwrap().len(),
            if name == "flow" { 2 } else { 1 }
        );
        connection.close();
        tokio::time::timeout(Duration::from_secs(2), async {
            while state.closed.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        server.abort();
    }
}
#[derive(Clone, Default)]
struct SessionRace {
    headers: Arc<Mutex<Vec<(i64, HeaderMap)>>>,
}
async fn session_race_reply(
    State(state): State<SessionRace>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let id = request["id"].as_i64().unwrap();
    state.headers.lock().unwrap().push((id, headers));
    if id == 1 {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut response = Json(json!({"jsonrpc":"2.0","id":id,"result":{"ok":true}})).into_response();
    if id <= 2 {
        response.headers_mut().insert(
            "mcp-session-id",
            axum::http::HeaderValue::from_static(if id == 1 {
                "late-first"
            } else {
                "early-second"
            }),
        );
    }
    response
}
#[tokio::test]
async fn concurrent_initialization_session_headers_follow_response_arrival_order() {
    let state = SessionRace::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/mcp", post(session_race_reply))
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let connection = platform_common::connect_http(
        &format!("http://{address}/mcp"),
        None,
        &std::collections::HashMap::<String, String>::new(),
        None,
    )
    .await
    .unwrap();
    let params = json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"race","version":"1"}});
    let first = connection
        .start_call_unbounded("initialize", params.clone())
        .unwrap();
    let second = connection
        .start_call_unbounded("initialize", params)
        .unwrap();
    second.wait_value().await.unwrap();
    first.wait_value().await.unwrap();
    let _: Value = connection.call("ping", json!({})).await.unwrap();
    let headers = state.headers.lock().unwrap();
    let (_, last) = headers.iter().find(|(id, _)| *id == 3).unwrap();
    assert_eq!(last["mcp-session-id"], "late-first");
    connection.close();
    server.abort();
}
