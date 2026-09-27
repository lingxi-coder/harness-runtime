//! MCP-01: a streamable-HTTP server that rejects the `initialize` POST is
//! re-dialled over legacy HTTP+SSE.
//!
//! Upstream does this behind `tengu_mcp_legacy_sse_fallback`, which defaults
//! TRUE, and decides on TWO things: the status is 400/404/405, **and** the
//! rejection body is not a JSON-RPC message. A server that answers with a real
//! JSON-RPC error has a protocol failure, not a wrong-protocol problem, and
//! re-dialling it would turn protocol errors into silent transport churn.

use axum::{
    extract::State,
    http::StatusCode,
    response::sse::{Event, Sse},
    routing::{get, post},
    Json, Router,
};
use futures::{stream, Stream, StreamExt};
use platform_api::{McpConnectOptions, McpTransport, McpTransportSpec};
use platform_common::mcp_remote::RemoteMcpTransport;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::time::Duration;
use tokio::sync::broadcast;

#[derive(Clone)]
struct FallbackState {
    /// What the streamable POST answers with.
    reject_status: StatusCode,
    reject_body: String,
    /// Replies pushed to the SSE stream.
    replies: broadcast::Sender<Value>,
    /// Whether the stream GET should answer 401 instead of streaming.
    stream_get_401: bool,
}

fn initialize_reply(id: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "legacy", "version": "1"}
        }
    })
}

/// The streamable endpoint: always rejects, with a configurable status/body.
async fn streamable_post(
    State(state): State<FallbackState>,
    Json(_body): Json<Value>,
) -> (StatusCode, String) {
    (state.reject_status, state.reject_body.clone())
}

/// The legacy stream: names `/messages`, then relays replies.
async fn stream_get(
    State(state): State<FallbackState>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    if state.stream_get_401 {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let rx = state.replies.subscribe();
    let endpoint = stream::once(async {
        Ok::<_, Infallible>(Event::default().event("endpoint").data("/messages"))
    });
    let replies = stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Ok(value) => Some((Some(value), rx)),
            Err(broadcast::error::RecvError::Lagged(_)) => Some((None, rx)),
            Err(broadcast::error::RecvError::Closed) => None,
        }
    })
    .filter_map(|v| async move { v.map(|v| Ok(Event::default().data(v.to_string()))) });
    Ok(Sse::new(endpoint.chain(replies)))
}

/// The legacy POST endpoint the server named.
async fn messages_post(
    State(state): State<FallbackState>,
    Json(body): Json<Value>,
) -> &'static str {
    if body.get("method").and_then(Value::as_str) == Some("initialize") {
        let _ = state.replies.send(initialize_reply(
            body.get("id").cloned().unwrap_or(Value::Null),
        ));
    }
    ""
}

async fn spawn(reject_status: StatusCode, reject_body: &str, stream_get_401: bool) -> String {
    let (replies, _) = broadcast::channel(8);
    let state = FallbackState {
        reject_status,
        reject_body: reject_body.to_string(),
        replies,
        stream_get_401,
    };
    let app = Router::new()
        .route("/mcp", post(streamable_post))
        .route("/mcp", get(stream_get))
        .route("/messages", post(messages_post))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/mcp")
}

fn http_spec(url: String) -> McpTransportSpec {
    McpTransportSpec::Http {
        url,
        headers: Default::default(),
        headers_helper: None,
        oauth: None,
    }
}

async fn connect(url: String) -> Result<platform_api::McpConnectResult, platform_api::McpError> {
    let transport = RemoteMcpTransport::new();
    tokio::time::timeout(
        Duration::from_secs(8),
        transport.connect_and_initialize(
            &http_spec(url),
            McpConnectOptions {
                deadline_ms: 6_000,
                ..Default::default()
            },
        ),
    )
    .await
    .expect("the connect must settle inside the test's own budget")
}

#[tokio::test]
async fn a_405_with_no_jsonrpc_body_is_rescued_over_legacy_sse() {
    // The classic "this server is SSE-only" signal.
    let url = spawn(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed", false).await;

    let result = connect(url).await.expect("the rescue must connect");
    assert!(
        result.capabilities.tools,
        "the capabilities came back over the legacy transport"
    );
    assert_eq!(
        result.negotiated.era,
        platform_api::McpProtocolEra::Legacy,
        "a rescued connection is a legacy one"
    );
}

#[tokio::test]
async fn a_404_is_also_rescued() {
    let url = spawn(StatusCode::NOT_FOUND, "not found", false).await;
    connect(url).await.expect("404 is in the predicate's set");
}

#[tokio::test]
async fn a_rejection_carrying_a_jsonrpc_error_is_not_rescued() {
    // 🚨 The half that is easy to drop. This server DID answer in JSON-RPC: it
    // has a protocol failure, and re-dialling it would convert that into
    // silent transport churn. The status alone must not be enough.
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "error": {"code": -32600, "message": "Invalid Request"}
    })
    .to_string();
    let url = spawn(StatusCode::METHOD_NOT_ALLOWED, &body, false).await;

    let err = connect(url)
        .await
        .expect_err("a JSON-RPC rejection must surface, not trigger a re-dial");
    let text = err.to_string();
    assert!(
        text.contains("405"),
        "the original POST rejection is what surfaces: {text}"
    );
}

#[tokio::test]
async fn an_sse_framed_jsonrpc_rejection_is_not_rescued_either() {
    // Upstream reads the `data:` line before deciding, so an SSE-framed error
    // is still a JSON-RPC answer.
    let body = format!(
        "event: message\ndata: {}\n\n",
        json!({"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"Invalid Request"}})
    );
    let url = spawn(StatusCode::METHOD_NOT_ALLOWED, &body, false).await;

    connect(url)
        .await
        .expect_err("an SSE-framed JSON-RPC error is still a JSON-RPC error");
}

#[tokio::test]
async fn a_500_is_not_rescued() {
    // Outside the status set entirely: a server error is a server error.
    let url = spawn(StatusCode::INTERNAL_SERVER_ERROR, "boom", false).await;
    connect(url)
        .await
        .expect_err("500 is not a wrong-protocol signal");
}

#[tokio::test]
async fn a_401_on_the_stream_get_after_a_400_does_not_start_oauth() {
    // postMethodNotAllowed's only job upstream. A 405 says "wrong method
    // here", which is evidence this url is an SSE endpoint; a 400 is not, so a
    // 401 on the stream GET must not point an auth flow at it.
    let url = spawn(StatusCode::BAD_REQUEST, "bad request", true).await;

    let err = connect(url)
        .await
        .expect_err("the rescue must not authenticate");
    let text = err.to_string();
    // Upstream surfaces the guard itself here, not the original 400: the
    // rescue's error is neither a timeout, nor a structured transport failure,
    // nor auth-ish (it is a refusal to TREAT it as auth), so the three-arm rule
    // falls through to `throw Ie`.
    assert!(
        text.contains("not starting OAuth against this URL"),
        "the refusal must say why it refused: {text}"
    );
    assert!(
        !text.contains("www-authenticate") && !text.contains("WWW-Authenticate"),
        "and it must not look like an auth challenge to the caller: {text}"
    );
}
