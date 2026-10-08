//! Real HTTP and SSE round trips for the shared remote-only transport.

use axum::{
    extract::State,
    response::sse::{Event, Sse},
    routing::{get, post},
    Json, Router,
};
use futures::{stream, Stream, StreamExt};
use lingxi_core::host::{McpConnectOptions, McpProtocolEra, McpTransport, McpTransportSpec};
use platform_common::RemoteMcpTransport;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

fn spec(kind: &str, url: String) -> McpTransportSpec {
    let headers = lingxi_core::host::McpHeaders::new();
    match kind {
        "sse" => McpTransportSpec::Sse {
            url,
            headers,
            headers_helper: None,
            oauth: None,
        },
        _ => McpTransportSpec::Http {
            url,
            headers,
            headers_helper: None,
            oauth: None,
        },
    }
}

fn reply_for(body: &Value) -> Option<Value> {
    let id = body.get("id")?.clone();
    let method = body.get("method").and_then(Value::as_str).unwrap_or("");
    let result = match method {
        "initialize" => json!({
            "protocolVersion": "2025-11-25",
            "capabilities": { "tools": {}, "resources": {}, "prompts": {} },
            "serverInfo": { "name": "common-remote", "version": "test" }
        }),
        "tools/list" => json!({"tools": [{
            "name": "echo", "description": "Echo", "inputSchema": {"type": "object"}
        }]}),
        "tools/call" => json!({
            "content": [{"type": "text", "text": body.pointer("/params/arguments/text").and_then(Value::as_str).unwrap_or("")}],
            "isError": false
        }),
        "resources/list" => {
            json!({"resources": [{"uri": "test://one", "name": "one", "mimeType": "text/plain"}]})
        }
        "resources/read" => json!({"contents": [{"uri": "test://one", "text": "resource"}]}),
        "prompts/list" => {
            json!({"prompts": [{"name": "hello", "description": "Hello", "arguments": []}]})
        }
        _ => json!({}),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

async fn http_handler(Json(body): Json<Value>) -> Json<Value> {
    Json(reply_for(&body).unwrap_or_else(|| json!({})))
}

#[derive(Clone)]
struct SseState {
    replies: broadcast::Sender<Value>,
}

async fn sse_get(
    State(state): State<SseState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.replies.subscribe();
    // A legacy HTTP+SSE server names its POST url in an `endpoint` event before
    // anything else. This mock deliberately names a DIFFERENT path from the
    // stream's, the way real ones do, so the round trip only succeeds if the
    // client honours it.
    let endpoint = stream::once(async {
        Ok::<_, Infallible>(Event::default().event("endpoint").data("/messages"))
    });
    let warmup = stream::once(async {
        tokio::time::sleep(Duration::from_millis(30)).await;
        None::<Value>
    });
    let replies = stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Ok(value) => Some((Some(value), rx)),
            Err(broadcast::error::RecvError::Lagged(_)) => Some((None, rx)),
            Err(broadcast::error::RecvError::Closed) => None,
        }
    });
    Sse::new(
        endpoint.chain(warmup.chain(replies).filter_map(|value| async move {
            value.map(|value| Ok(Event::default().data(value.to_string())))
        })),
    )
}

async fn sse_post(State(state): State<SseState>, Json(body): Json<Value>) -> &'static str {
    if let Some(reply) = reply_for(&body) {
        let _ = state.replies.send(reply);
    }
    ""
}

async fn spawn_http() -> String {
    let app = Router::new().route("/mcp", post(http_handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/mcp")
}

async fn spawn_sse() -> String {
    let (replies, _) = broadcast::channel(32);
    let state = SseState { replies };
    let app = Router::new()
        .route("/mcp", get(sse_get))
        // No POST route on the stream url: the client must use the endpoint the
        // server named, or this test 404s.
        .route("/messages", post(sse_post))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/mcp")
}

#[derive(Clone)]
struct NegotiationState {
    requests: Arc<std::sync::Mutex<Vec<Value>>>,
}

async fn negotiation_http_handler(
    State(state): State<NegotiationState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    state.requests.lock().unwrap().push(body.clone());
    let id = body.get("id").cloned().unwrap_or(Value::Null);
    let method = body.get("method").and_then(Value::as_str).unwrap_or("");
    if method == "server/discover" {
        // Deliberately use a wrong id. The disposable probe must classify this
        // as compatibility and close before the legacy redial.
        return Json(json!({
            "jsonrpc": "2.0",
            "id": 999,
            "result": {"protocolVersion": "2026-07-28"}
        }));
    }
    Json(json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": "2025-11-25",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "wrong-id", "version": "1"}
        }
    }))
}

async fn spawn_negotiation_http() -> (String, NegotiationState) {
    let state = NegotiationState {
        requests: Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let app = Router::new()
        .route("/mcp", post(negotiation_http_handler))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/mcp"), state)
}

async fn exercise(kind: &str, url: String) {
    let transport = RemoteMcpTransport::new();
    let conn = tokio::time::timeout(Duration::from_secs(5), transport.connect(&spec(kind, url)))
        .await
        .unwrap()
        .unwrap();
    let caps = transport.initialize(&conn).await.unwrap();
    assert!(caps.tools && caps.resources && caps.prompts);
    assert_eq!(
        transport.list_tools(&conn).await.unwrap()[0].tool_name,
        "echo"
    );
    let result = transport
        .call_tool(&conn, "echo", json!({"text": "hello"}))
        .await
        .unwrap();
    assert_eq!(
        result.content.pointer("/0/text").and_then(Value::as_str),
        Some("hello")
    );
    assert_eq!(transport.list_resources(&conn).await.unwrap().len(), 1);
    assert_eq!(
        transport
            .read_resource(&conn, "test://one")
            .await
            .unwrap()
            .content,
        "resource"
    );
    assert_eq!(transport.list_prompts(&conn).await.unwrap().len(), 1);
    transport.disconnect(conn.connection_id).await.unwrap();
    assert!(matches!(
        transport.ping(conn.connection_id).await,
        Err(lingxi_core::host::McpError::Connection(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_remote_http_roundtrip() {
    exercise("http", spawn_http().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_remote_sse_roundtrip() {
    exercise("sse", spawn_sse().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_disposable_probe_classifies_wrong_id_and_redials_legacy() {
    let transport = RemoteMcpTransport::new();
    let (url, state) = spawn_negotiation_http().await;
    let result = transport
        .connect_and_initialize(
            &spec("http", url),
            McpConnectOptions {
                expected_era: Some(McpProtocolEra::Modern),
                deadline_ms: 5_000,
                probe_timeout_ms: Some(50_000),
                elicitation: lingxi_core::host::McpElicitationCapabilities::default(),
            },
        )
        .await
        .expect("wrong-id probe should fall back to legacy");
    assert_eq!(result.negotiated.era, McpProtocolEra::Legacy);
    let requests = state.requests.lock().unwrap().clone();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request["method"] == "server/discover")
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request["method"] == "initialize")
            .count(),
        1
    );
    transport
        .disconnect(result.connection.connection_id)
        .await
        .unwrap();
}
