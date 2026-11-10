//! Provider-neutral HTTP/SSE contracts captured from Claude Code 2.1.322.
use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{get, post},
    Json, Router,
};
use futures::{stream, StreamExt};
use lingxi_core::host::McpHeaders;
use serde_json::{json, Value};
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio::sync::{broadcast, Mutex, Notify};

#[derive(Clone, Copy)]
enum Reply {
    Json,
    Sse,
    Array,
    WrongType,
    MissingType,
    NoContent,
    PlainListener,
}

struct HttpState {
    reply: Reply,
    frames: Mutex<Vec<(String, HeaderMap)>>,
    deleted: Notify,
}

fn response(body: impl Into<Body>, content_type: Option<&str>) -> Response {
    let mut response = Response::new(body.into());
    if let Some(content_type) = content_type {
        response
            .headers_mut()
            .insert("content-type", content_type.parse().unwrap());
    }
    response
}

async fn http_post(
    State(state): State<Arc<HttpState>>,
    headers: HeaderMap,
    Json(frame): Json<Value>,
) -> Response {
    let method = frame["method"].as_str().unwrap_or("");
    state.frames.lock().await.push((method.to_owned(), headers));
    if frame.get("id").is_none() {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::ACCEPTED;
        return response;
    }
    let result = if method == "initialize" {
        json!({"protocolVersion":"2025-11-25","capabilities":{},"serverInfo":{"name":"wire","version":"1"}})
    } else {
        json!({"text":"correct"})
    };
    let frame = json!({"jsonrpc":"2.0","id":frame["id"],"result":result});
    if method == "initialize" {
        let mut response = response(frame.to_string(), Some("application/json"));
        response
            .headers_mut()
            .insert("mcp-session-id", "wire-session-322".parse().unwrap());
        return response;
    }
    match state.reply {
        Reply::Json | Reply::PlainListener => {
            response(frame.to_string(), Some("Application/JSON; charset=utf-8"))
        }
        Reply::Array => response(json!([frame]).to_string(), Some("application/json")),
        Reply::WrongType => response(frame.to_string(), Some("text/plain")),
        Reply::MissingType => response(frame.to_string(), None),
        Reply::NoContent => {
            let mut response = Response::new(Body::empty());
            *response.status_mut() = StatusCode::NO_CONTENT;
            response
        }
        Reply::Sse => {
            let wrong = json!({"jsonrpc":"2.0","id":frame["id"],"result":{"text":"wrong"}});
            let correct = frame
                .to_string()
                .replacen(",\"result\":", ",\r\ndata: \"result\":", 1);
            response(
                format!("event: other\r\ndata: {wrong}\r\n\r\nevent: message\r\ndata: {correct}\r\n\r\n"),
                Some("Text/Event-Stream; charset=utf-8"),
            )
        }
    }
}

async fn http_get(State(state): State<Arc<HttpState>>) -> Response {
    if matches!(state.reply, Reply::PlainListener) {
        response(
            "event: other\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"wrong-notification\"}\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"wire-notification\"}\n\n",
            Some("text/plain"),
        )
    } else {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
        response
    }
}

async fn http_delete(State(state): State<Arc<HttpState>>, headers: HeaderMap) -> StatusCode {
    state.frames.lock().await.push(("DELETE".into(), headers));
    state.deleted.notify_one();
    StatusCode::OK
}

async fn http_fixture(reply: Reply) -> (String, Arc<HttpState>, tokio::task::JoinHandle<()>) {
    let state = Arc::new(HttpState {
        reply,
        frames: Mutex::new(Vec::new()),
        deleted: Notify::new(),
    });
    let app = Router::new()
        .route("/mcp", post(http_post).get(http_get).delete(http_delete))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, state, server)
}

#[tokio::test]
async fn native_322_http_mime_arrays_and_named_events_roundtrip() {
    for reply in [Reply::Json, Reply::Array, Reply::Sse] {
        let (url, _, server) = http_fixture(reply).await;
        let connection = platform_common::connect_http(&url, None, &McpHeaders::new(), None)
            .await
            .unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            connection.call::<_, Value>("probe", json!({})),
        )
        .await
        .expect("native-compatible response must complete promptly")
        .unwrap();
        assert_eq!(result, json!({"text":"correct"}));
        connection.close();
        server.abort();
    }
    for (reply, content_type) in [
        (Reply::WrongType, "text/plain"),
        (Reply::MissingType, "null"),
        (Reply::NoContent, "null"),
    ] {
        let (url, _, server) = http_fixture(reply).await;
        let connection = platform_common::connect_http(&url, None, &McpHeaders::new(), None)
            .await
            .unwrap();
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            connection.call::<_, Value>("probe", json!({})),
        )
        .await
        .expect("invalid MIME must fail without waiting for the RPC deadline")
        .unwrap_err();
        assert!(error
            .to_string()
            .contains(&format!("Unexpected content type: {content_type}")));
        connection.close();
        server.abort();
    }
}

#[tokio::test]
async fn native_322_http_header_overrides_initialize_and_explicit_cleanup() {
    let (url, state, server) = http_fixture(Reply::Json).await;
    let configured = McpHeaders::from_iter([
        ("Accept".into(), "application/custom".into()),
        ("Content-Type".into(), "text/custom".into()),
        ("Mcp-Session-Id".into(), "configured-session".into()),
        ("Mcp-Protocol-Version".into(), "configured-version".into()),
        ("X-Dupe".into(), "one".into()),
        ("x-dupe".into(), "two".into()),
    ]);
    let (connection, session) =
        platform_common::mcp_http::connect_http_with_session(&url, None, &configured, None)
            .await
            .unwrap();
    let _: Value = connection.call("initialize", json!({})).await.unwrap();
    let _: Value = connection.call("probe", json!({})).await.unwrap();
    let frames = state.frames.lock().await;
    let initial = &frames
        .iter()
        .find(|(method, _)| method == "initialize")
        .unwrap()
        .1;
    assert!(!initial.contains_key("mcp-session-id"));
    assert_eq!(initial["mcp-protocol-version"], "configured-version");
    let later = &frames
        .iter()
        .find(|(method, _)| method == "probe")
        .unwrap()
        .1;
    assert_eq!(
        later["accept"],
        "application/custom, application/json, text/event-stream"
    );
    assert_eq!(later["content-type"], "application/json");
    assert_eq!(later["x-dupe"], "one, two");
    assert_eq!(
        later["mcp-session-id"],
        "wire-session-322, configured-session"
    );
    assert_eq!(
        later["mcp-protocol-version"],
        "2025-11-25, configured-version"
    );
    drop(frames);
    session.terminate_session().await.unwrap();
    connection.close();
    tokio::time::timeout(Duration::from_secs(2), state.deleted.notified())
        .await
        .unwrap();
    let frames = state.frames.lock().await;
    let cleanup = &frames
        .iter()
        .find(|(method, _)| method == "DELETE")
        .unwrap()
        .1;
    assert_eq!(cleanup["accept"], "application/custom");
    assert_eq!(cleanup["content-type"], "text/custom");
    assert_eq!(
        cleanup["mcp-session-id"],
        "wire-session-322, configured-session"
    );
    server.abort();
}

#[tokio::test]
async fn native_322_streamable_get_processes_message_events_without_a_mime_guard() {
    let (url, _, server) = http_fixture(Reply::PlainListener).await;
    let connection = platform_common::connect_http(&url, None, &McpHeaders::new(), None)
        .await
        .unwrap();
    let _: Value = connection.call("initialize", json!({})).await.unwrap();
    let mut notifications = connection.notifications();
    connection
        .notify("notifications/initialized", json!({}))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let notification = notifications.recv().await.unwrap();
            assert_ne!(notification.method, "wrong-notification");
            if notification.method == "wire-notification" {
                break;
            }
        }
    })
    .await
    .unwrap();
    connection.close();
    server.abort();
}

struct LegacyState {
    frames: Mutex<Vec<HeaderMap>>,
    replies: broadcast::Sender<String>,
    stream_ended: Arc<Notify>,
}
struct StreamEnd(Arc<Notify>);
impl Drop for StreamEnd {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

async fn legacy_get(State(state): State<Arc<LegacyState>>) -> Response {
    let first = stream::once(async {
        Ok::<_, Infallible>(Bytes::from_static(b"event: endpoint\ndata: /messages\n\n"))
    });
    let replies = stream::unfold(
        (
            state.replies.subscribe(),
            StreamEnd(state.stream_ended.clone()),
        ),
        |(mut receiver, guard)| async move {
            receiver
                .recv()
                .await
                .ok()
                .map(|data| (Ok::<_, Infallible>(Bytes::from(data)), (receiver, guard)))
        },
    );
    response(
        Body::from_stream(first.chain(replies)),
        Some("text/event-stream"),
    )
}

async fn legacy_post(
    State(state): State<Arc<LegacyState>>,
    headers: HeaderMap,
    Json(frame): Json<Value>,
) -> StatusCode {
    state.frames.lock().await.push(headers);
    let result = if frame["method"] == "initialize" {
        json!({"protocolVersion":"2025-11-25"})
    } else {
        json!({"text":"correct"})
    };
    let wrong = json!({"jsonrpc":"2.0","id":frame["id"],"result":{"text":"wrong"}});
    let correct = json!({"jsonrpc":"2.0","id":frame["id"],"result":result});
    state
        .replies
        .send(format!(
            "event: other\r\ndata: {wrong}\r\n\r\nevent: message\r\ndata: {correct}\r\n\r\n"
        ))
        .unwrap();
    StatusCode::ACCEPTED
}

#[tokio::test]
async fn native_322_legacy_sse_protocol_headers_events_and_close() {
    let (replies, _) = broadcast::channel(8);
    let state = Arc::new(LegacyState {
        frames: Mutex::new(Vec::new()),
        replies,
        stream_ended: Arc::new(Notify::new()),
    });
    let app = Router::new()
        .route("/sse", get(legacy_get))
        .route("/messages", post(legacy_post))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/sse", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let connection = platform_common::connect_sse(
        &url,
        None,
        &McpHeaders::new(),
        platform_common::SseEndpointMode::EndpointEvent,
    )
    .await
    .unwrap();
    let initial: Value = connection.call("initialize", json!({})).await.unwrap();
    assert_eq!(initial["protocolVersion"], "2025-11-25");
    let actual: Value = connection.call("probe", json!({})).await.unwrap();
    assert_eq!(actual, json!({"text":"correct"}));
    let frames = state.frames.lock().await;
    assert_eq!(frames[0]["accept"], "application/json, text/event-stream");
    assert!(!frames[0].contains_key("mcp-protocol-version"));
    assert_eq!(frames[1]["mcp-protocol-version"], "2025-11-25");
    drop(frames);
    connection.close();
    tokio::time::timeout(Duration::from_secs(2), state.stream_ended.notified())
        .await
        .unwrap();
    server.abort();
}

#[tokio::test]
async fn native_322_legacy_eventsource_rejects_wrong_or_uppercase_mime_before_endpoint() {
    for content_type in ["text/plain", "Text/Event-Stream"] {
        let app = Router::new().route(
            "/sse",
            get(move || async move {
                response("event: endpoint\ndata: /messages\n\n", Some(content_type))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/sse", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let error = platform_common::connect_sse(
            &url,
            None,
            &McpHeaders::new(),
            platform_common::SseEndpointMode::EndpointEvent,
        )
        .await
        .err()
        .expect("legacy EventSource must reject invalid MIME");
        assert!(error.to_string().contains("Invalid content type"));
        server.abort();
    }
}
