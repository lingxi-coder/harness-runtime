//! Loopback regressions paired with the native 2.1.322 remote gap receipts.
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::post,
    Json, Router,
};
use lingxi_core::{host::McpHeaders, types::utf16_json::Utf16JsonProjection};
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{Mutex, Notify},
};

#[derive(Clone, Copy)]
enum Mode {
    Schema,
    Atomic,
    Syntax,
    SseSyntax,
    SseSchema,
    Resume,
    Retry,
    Soak,
    Session,
    LongIdle,
    Fanout,
    Empty,
    HttpError,
    ModernRpcError,
    JsonOverflow,
    EventOverflow,
}
struct Peer {
    mode: Mode,
    gets: AtomicUsize,
    deletes: AtomicUsize,
    frames: Mutex<Vec<(String, HeaderMap, Value)>>,
    cursor_id: Mutex<Value>,
    posts: AtomicUsize,
}
fn response(body: impl Into<Body>, kind: &str) -> Response {
    Response::builder()
        .header("content-type", kind)
        .body(body.into())
        .unwrap()
}
async fn post_frame(
    State(peer): State<Arc<Peer>>,
    headers: HeaderMap,
    Json(frame): Json<Value>,
) -> Response {
    peer.posts.fetch_add(1, Ordering::SeqCst);
    let method = frame["method"].as_str().unwrap_or("");
    peer.frames
        .lock()
        .await
        .push(("POST".into(), headers, frame.clone()));
    if frame.get("id").is_none() {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::ACCEPTED;
        return response;
    }
    let result = if method == "initialize" {
        json!({"protocolVersion":"2025-11-25","capabilities":{},"serverInfo":{"name":"gap","version":"1"}})
    } else {
        json!({"content":[{"type":"text","text":"REMOTE_GAP_RESULT_322"}]})
    };
    let answer = json!({"jsonrpc":"2.0","id":frame["id"],"result":result});
    if method == "initialize" {
        let mut response = response(answer.to_string(), "application/json");
        response
            .headers_mut()
            .insert("mcp-session-id", "gap-session".parse().unwrap());
        return response;
    }
    if method != "after" {
        match peer.mode {
            Mode::JsonOverflow | Mode::EventOverflow => {
                let answer=json!({"jsonrpc":"2.0","id":frame["id"],"result":{"content":[{"type":"text","text":"x".repeat(17*1024*1024)}]}}).to_string();
                return if matches!(peer.mode, Mode::EventOverflow) {
                    response(format!("data: {answer}\n\n"), "text/event-stream")
                } else {
                    response(answer, "application/json")
                };
            }
            Mode::Fanout if frame["params"]["name"] == "hold" => {
                return response(
                    Body::from_stream(futures::stream::pending::<
                        Result<axum::body::Bytes, std::convert::Infallible>,
                    >()),
                    "application/json",
                )
            }
            Mode::Fanout => return response("{", "application/json"),
            Mode::Empty => return response("", "text/event-stream"),
            Mode::HttpError => {
                let mut reply = response("REMOTE_ERROR_Ω", "text/plain; charset=iso-8859-1");
                *reply.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                return reply;
            }
            Mode::ModernRpcError => {
                let mut reply=response(json!({"jsonrpc":"2.0","id":frame["id"],"error":{"code":-32602,"message":"native modern error"}}).to_string(),"application/json");
                *reply.status_mut() = StatusCode::BAD_REQUEST;
                return reply;
            }
            Mode::Schema => {
                return response(
                    json!({"jsonrpc":"2.0","id":frame["id"],"result":[]}).to_string(),
                    "application/json",
                )
            }
            Mode::Atomic => {
                return response(
                    json!([answer,{"invalid":"frame"}]).to_string(),
                    "application/json",
                )
            }
            Mode::Syntax => return response("{", "application/json"),
            Mode::SseSyntax => {
                return response(
                    format!("data: {{\n\ndata: {answer}\n\n"),
                    "text/event-stream",
                )
            }
            Mode::SseSchema => {
                return response(
                    format!(
                    "data: {{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":[]}}\n\ndata: {answer}\n\n",
                    frame["id"]
                ),
                    "text/event-stream",
                )
            }
            Mode::Resume => {
                *peer.cursor_id.lock().await = frame["id"].clone();
                return response("retry: 5\n\nid: post-cursor\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{\"progressToken\":\"unmatched\",\"progress\":1}}\n\n","text/event-stream");
            }
            _ => {}
        }
    }
    response(answer.to_string(), "application/json")
}
async fn get_stream(State(peer): State<Arc<Peer>>, headers: HeaderMap) -> Response {
    peer.frames
        .lock()
        .await
        .push(("GET".into(), headers.clone(), Value::Null));
    let count = peer.gets.fetch_add(1, Ordering::SeqCst) + 1;
    match peer.mode {
        Mode::LongIdle => response(
            Body::from_stream(futures::stream::pending::<
                Result<axum::body::Bytes, std::convert::Infallible>,
            >()),
            "text/event-stream",
        ),
        Mode::Resume
            if headers
                .get("last-event-id")
                .is_some_and(|value| value == "post-cursor") =>
        {
            response(
                format!(
                    "data: {}\n\n",
                    json!({"jsonrpc":"2.0","id":peer.cursor_id.lock().await.clone(),"result":{"content":[{"type":"text","text":"REMOTE_GAP_RESULT_322"}]}})
                ),
                "text/event-stream",
            )
        }
        Mode::Retry if count > 1 => {
            let mut response = response("retry failed", "text/plain");
            *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
            response
        }
        Mode::Retry | Mode::Soak => response(
            format!(
                "retry: 1\n\nid: cursor-{count}\ndata: {}\n\n",
                json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":"unused","progress":count}})
            ),
            "text/event-stream",
        ),
        _ => {
            let mut response = Response::new(Body::empty());
            *response.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
            response
        }
    }
}
async fn delete(State(peer): State<Arc<Peer>>) -> StatusCode {
    peer.deletes.fetch_add(1, Ordering::SeqCst);
    StatusCode::NO_CONTENT
}
async fn fixture(mode: Mode) -> (String, Arc<Peer>, tokio::task::JoinHandle<()>) {
    let peer = Arc::new(Peer {
        mode,
        gets: AtomicUsize::new(0),
        deletes: AtomicUsize::new(0),
        frames: Mutex::new(vec![]),
        cursor_id: Mutex::new(Value::Null),
        posts: AtomicUsize::new(0),
    });
    let app = Router::new()
        .route("/mcp", post(post_frame).get(get_stream).delete(delete))
        .with_state(peer.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, peer, task)
}
async fn until(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn native_322_bad_json_closes_but_schema_rejects_only_that_call() {
    for (mode, kind) in [
        (Mode::Syntax, "syntax"),
        (Mode::SseSyntax, "syntax"),
        (Mode::Schema, "schema"),
        (Mode::Atomic, "schema"),
    ] {
        let (url, _, server) = fixture(mode).await;
        let connection = Arc::new(
            platform_common::connect_http(&url, None, &McpHeaders::new(), None)
                .await
                .unwrap(),
        );
        let client =
            mcp::McpClient::new("gap", std::env::current_dir().unwrap(), connection.clone()).await;
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            client.call_tool_with_timeout(
                "mcp__gap__echo",
                Utf16JsonProjection::parse("{}").unwrap(),
                Duration::from_secs(10),
            ),
        )
        .await
        .expect("transport failure must arrive before close or RPC timeout")
        .unwrap_err();
        let text = error
            .source_diagnostic()
            .unwrap()
            .string_at("")
            .unwrap()
            .to_owned();
        if kind == "syntax" {
            assert_eq!(text,"The connection to MCP server \"gap\" was closed because a message from the server was too large or could not be parsed, so the result of tool \"echo\" was not read. The tool may have run: check whether it did before running it again.");
            until(|| connection.is_closed()).await;
        } else {
            assert!(text.starts_with("MCP server \"gap\" returned a malformed result that failed schema validation: [\n"),"{text}");
            let response: Value = connection.call("after", json!({})).await.unwrap();
            assert!(response.get("content").is_some());
        }
        connection.close();
        server.abort();
    }
    let (url, _, server) = fixture(Mode::SseSchema).await;
    let connection = platform_common::connect_http(&url, None, &McpHeaders::new(), None)
        .await
        .unwrap();
    let answer: Value = connection.call("probe", json!({})).await.unwrap();
    assert!(answer.get("content").is_some());
    connection.close();
    server.abort();
}

#[tokio::test]
async fn native_322_post_sse_resumes_via_get_without_replaying_the_post() {
    let (url, peer, server) = fixture(Mode::Resume).await;
    let connection = platform_common::connect_http(&url, None, &McpHeaders::new(), None)
        .await
        .unwrap();
    let answer = tokio::time::timeout(
        Duration::from_secs(2),
        connection.call::<_, Value>("tools/call", json!({"name":"echo"})),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(answer.get("content").is_some());
    let frames = peer.frames.lock().await;
    assert_eq!(frames.iter().filter(|row| row.0 == "POST").count(), 1);
    assert_eq!(
        frames.iter().find(|row| row.0 == "GET").unwrap().1["last-event-id"],
        "post-cursor"
    );
    drop(frames);
    connection.close();
    server.abort();
}

#[tokio::test]
async fn native_322_syntax_close_reports_uncertain_results_to_every_pending_tool() {
    let (url, peer, server) = fixture(Mode::Fanout).await;
    let connection = Arc::new(
        platform_common::connect_http(&url, None, &McpHeaders::new(), None)
            .await
            .unwrap(),
    );
    let client = Arc::new(
        mcp::McpClient::new("gap", std::env::current_dir().unwrap(), connection.clone()).await,
    );
    let held = client.clone();
    let first = tokio::spawn(async move {
        held.call_tool("mcp__gap__hold", Utf16JsonProjection::parse("{}").unwrap())
            .await
    });
    until(|| peer.posts.load(Ordering::SeqCst) >= 1).await;
    let second = client.call_tool("mcp__gap__bad", Utf16JsonProjection::parse("{}").unwrap());
    let (first, second) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(first, second)
    })
    .await
    .unwrap();
    for (tool, error) in [
        ("hold", first.unwrap().unwrap_err()),
        ("bad", second.unwrap_err()),
    ] {
        let text = error.source_diagnostic().unwrap();
        let text = text.string_at("").unwrap();
        assert!(
            text.contains(&format!("result of tool \"{tool}\" was not read")),
            "{text}"
        );
    }
    connection.close();
    server.abort();
}

#[tokio::test]
async fn native_322_non_resumable_empty_stream_leaves_the_rpc_timer_in_charge() {
    use jsonrpc::{ConnectionError, RouterError};
    let (url, peer, server) = fixture(Mode::Empty).await;
    let connection = platform_common::connect_http(&url, None, &McpHeaders::new(), None)
        .await
        .unwrap();
    let error = connection
        .call_with_timeout::<_, Value>("probe", json!({}), Duration::from_millis(40))
        .await
        .unwrap_err();
    assert!(
        matches!(error, ConnectionError::Router(RouterError::Timeout(_))),
        "{error}"
    );
    assert_eq!(peer.gets.load(Ordering::SeqCst), 0);
    let _: Value = connection.call("after", json!({})).await.unwrap();
    connection.close();
    server.abort();
}

#[tokio::test]
async fn native_322_http_transport_diagnostics_and_modern_rpc_errors_stay_distinct() {
    let (url, _, server) = fixture(Mode::HttpError).await;
    let connection = Arc::new(
        platform_common::connect_http(&url, None, &McpHeaders::new(), None)
            .await
            .unwrap(),
    );
    let client =
        mcp::McpClient::new("gap", std::env::current_dir().unwrap(), connection.clone()).await;
    let error = client
        .call_tool("mcp__gap__echo", Utf16JsonProjection::parse("{}").unwrap())
        .await
        .unwrap_err();
    assert_eq!(
        error.source_diagnostic().unwrap().string_at(""),
        Some("Error POSTing to endpoint: REMOTE_ERROR_Ω")
    );
    connection.close();
    server.abort();
    let (url, _, server) = fixture(Mode::ModernRpcError).await;
    let connection = platform_common::connect_http(&url, None, &McpHeaders::new(), None)
        .await
        .unwrap();
    let error = connection
        .call::<_, Value>(
            "probe",
            json!({"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}),
        )
        .await
        .unwrap_err();
    let jsonrpc::ConnectionError::Router(jsonrpc::RouterError::Remote(error)) = error else {
        panic!("expected server JSON-RPC error");
    };
    assert_eq!(error.code, -32602);
    assert_eq!(error.message, "native modern error");
    assert!(error.data.is_none());
    connection.close();
    server.abort();
}

#[tokio::test]
async fn native_322_json_body_overflow_is_per_request_but_sse_overflow_is_fatal() {
    for mode in [Mode::JsonOverflow, Mode::EventOverflow] {
        let (url, _, server) = fixture(mode).await;
        let connection = Arc::new(
            platform_common::connect_http(&url, None, &McpHeaders::new(), None)
                .await
                .unwrap(),
        );
        let client =
            mcp::McpClient::new("gap", std::env::current_dir().unwrap(), connection.clone()).await;
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            client.call_tool("mcp__gap__echo", Utf16JsonProjection::parse("{}").unwrap()),
        )
        .await
        .unwrap()
        .unwrap_err();
        let text = error.source_diagnostic().unwrap();
        let text = text.string_at("").unwrap();
        if matches!(mode, Mode::JsonOverflow) {
            assert_eq!(text,"The response from MCP server \"gap\" for tool \"echo\" was larger than 16MB, so it was not read. The tool may have run: check whether it did before running it again, and when you run it again, ask for less data.");
            assert!(!connection.is_closed());
            let _: Value = connection.call("after", json!({})).await.unwrap();
        } else {
            assert!(text.contains("was closed because a message from the server was too large or could not be parsed"),"{text}");
            until(|| connection.is_closed()).await;
        }
        connection.close();
        server.abort();
    }
}

#[tokio::test]
async fn native_322_get_retry_budget_and_replay_soak_preserve_post_calls() {
    for (mode, count) in [(Mode::Retry, 3), (Mode::Soak, 50)] {
        let (url, peer, server) = fixture(mode).await;
        let connection = platform_common::connect_http(&url, None, &McpHeaders::new(), None)
            .await
            .unwrap();
        let _: Value = connection.call("initialize", json!({})).await.unwrap();
        connection
            .notify("notifications/initialized", json!({}))
            .unwrap();
        let _: Value = connection.call("tools/list", json!({})).await.unwrap();
        let _: Value = connection
            .call("tools/call", json!({"name":"echo","arguments":{}}))
            .await
            .unwrap();
        until(|| peer.gets.load(Ordering::SeqCst) >= count).await;
        if matches!(mode, Mode::Retry) {
            tokio::time::sleep(Duration::from_millis(30)).await;
            assert_eq!(peer.gets.load(Ordering::SeqCst), 3);
        }
        let _: Value = connection.call("after", json!({})).await.unwrap();
        connection.close();
        let frames = peer.frames.lock().await;
        let request_ids = frames
            .iter()
            .filter_map(|row| {
                row.2
                    .get("id")
                    .map(|id| (row.2["method"].clone(), id.clone()))
            })
            .take(3)
            .collect::<Vec<_>>();
        assert_eq!(
            request_ids,
            vec![
                (json!("initialize"), json!(0)),
                (json!("tools/list"), json!(1)),
                (json!("tools/call"), json!(2))
            ]
        );
        let gets = frames
            .iter()
            .filter(|row| row.0 == "GET")
            .collect::<Vec<_>>();
        assert!(!gets[0].1.contains_key("last-event-id"));
        for (index, frame) in gets.iter().enumerate().skip(1) {
            let expected = if matches!(mode, Mode::Retry) {
                "cursor-1".to_owned()
            } else {
                format!("cursor-{index}")
            };
            assert_eq!(frame.1["last-event-id"], expected);
        }
        assert_eq!(peer.deletes.load(Ordering::SeqCst), 0);
        server.abort();
    }
}

#[tokio::test]
async fn native_322_close_and_explicit_session_delete_are_independent() {
    let (url, peer, server) = fixture(Mode::Session).await;
    let (connection, session) =
        platform_common::mcp_http::connect_http_with_session(&url, None, &McpHeaders::new(), None)
            .await
            .unwrap();
    let _: Value = connection.call("initialize", json!({})).await.unwrap();
    session.terminate_session().await.unwrap();
    session.terminate_session().await.unwrap();
    assert_eq!(peer.deletes.load(Ordering::SeqCst), 1);
    let _: Value = connection.call("after", json!({})).await.unwrap();
    assert!(!peer
        .frames
        .lock()
        .await
        .last()
        .unwrap()
        .1
        .contains_key("mcp-session-id"));
    connection.close();
    session.closed().await;
    assert_eq!(peer.deletes.load(Ordering::SeqCst), 1);
    server.abort();
    let (url, peer, server) = fixture(Mode::Session).await;
    let (connection, session) =
        platform_common::mcp_http::connect_http_with_session(&url, None, &McpHeaders::new(), None)
            .await
            .unwrap();
    let _: Value = connection.call("initialize", json!({})).await.unwrap();
    connection.close();
    session.closed().await;
    assert!(session.terminate_session().await.is_err());
    assert_eq!(peer.deletes.load(Ordering::SeqCst), 0);
    server.abort();
}

#[tokio::test]
async fn native_322_silent_get_stays_attached_past_ninety_seconds() {
    let (url, peer, server) = fixture(Mode::LongIdle).await;
    let (connection, session) =
        platform_common::mcp_http::connect_http_with_session(&url, None, &McpHeaders::new(), None)
            .await
            .unwrap();
    let _: Value = connection.call("initialize", json!({})).await.unwrap();
    connection
        .notify("notifications/initialized", json!({}))
        .unwrap();
    until(|| peer.gets.load(Ordering::SeqCst) == 1).await;
    tokio::time::sleep(Duration::from_secs(91)).await;
    assert_eq!(
        peer.gets.load(Ordering::SeqCst),
        1,
        "a quiet native stream has no invented 90s idle deadline"
    );
    let _: Value = connection.call("after", json!({})).await.unwrap();
    connection.close();
    session.closed().await;
    assert_eq!(peer.deletes.load(Ordering::SeqCst), 0);
    server.abort();
}

#[tokio::test]
async fn native_322_redirect_methods_credentials_and_127_attempt_limit_match_wire() {
    use axum::{body::Bytes, extract::OriginalUri, http::Method, routing::any};
    #[derive(Clone)]
    struct Redirects {
        destination: String,
        code: StatusCode,
        seen: Arc<Mutex<Vec<(Method, HeaderMap, Bytes)>>>,
        count: Arc<AtomicUsize>,
    }
    async fn redirect(State(state): State<Redirects>) -> Response {
        state.count.fetch_add(1, Ordering::SeqCst);
        Response::builder()
            .status(state.code)
            .header("location", state.destination)
            .body(Body::empty())
            .unwrap()
    }
    async fn landing(
        State(state): State<Redirects>,
        method: Method,
        OriginalUri(_uri): OriginalUri,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        state
            .seen
            .lock()
            .await
            .push((method, headers, body.clone()));
        if body.is_empty() {
            return response("redirected GET", "text/plain");
        }
        let frame: Value = serde_json::from_slice(&body).unwrap();
        response(
            json!({"jsonrpc":"2.0","id":frame["id"],"result":{}}).to_string(),
            "application/json",
        )
    }
    for code in [StatusCode::TEMPORARY_REDIRECT, StatusCode::FOUND] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let landing_url = format!("http://{}/landing", listener.local_addr().unwrap());
        let state = Redirects {
            destination: landing_url,
            code,
            seen: Arc::new(Mutex::new(vec![])),
            count: Arc::new(AtomicUsize::new(0)),
        };
        let app = Router::new()
            .route("/landing", any(landing))
            .with_state(state.clone());
        let target = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let origin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", origin.local_addr().unwrap());
        let app = Router::new()
            .route("/mcp", any(redirect))
            .with_state(state.clone());
        let server = tokio::spawn(async move { axum::serve(origin, app).await.unwrap() });
        let headers = McpHeaders::from_iter([
            ("Authorization".into(), "Bearer fake-loopback-only".into()),
            ("X-Wire-Identity".into(), "gap322".into()),
        ]);
        let connection = platform_common::connect_http(&url, None, &headers, None)
            .await
            .unwrap();
        let answer = connection.call::<_, Value>("probe", json!({})).await;
        assert_eq!(answer.is_ok(), code == StatusCode::TEMPORARY_REDIRECT);
        let seen = state.seen.lock().await;
        assert_eq!(seen.len(), 1);
        assert!(!seen[0].1.contains_key("authorization"));
        assert_eq!(seen[0].1["x-wire-identity"], "gap322");
        assert_eq!(
            seen[0].0,
            if code == StatusCode::FOUND {
                Method::GET
            } else {
                Method::POST
            }
        );
        assert_eq!(seen[0].2.is_empty(), code == StatusCode::FOUND);
        connection.close();
        server.abort();
        target.abort();
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/loop", listener.local_addr().unwrap());
    let state = Redirects {
        destination: url.clone(),
        code: StatusCode::TEMPORARY_REDIRECT,
        seen: Arc::new(Mutex::new(vec![])),
        count: Arc::new(AtomicUsize::new(0)),
    };
    let app = Router::new()
        .route("/loop", any(redirect))
        .with_state(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let connection = platform_common::connect_http(&url, None, &McpHeaders::new(), None)
        .await
        .unwrap();
    assert!(connection
        .call::<_, Value>("probe", json!({}))
        .await
        .is_err());
    assert_eq!(state.count.load(Ordering::SeqCst), 127);
    connection.close();
    server.abort();
}

#[tokio::test]
async fn native_322_legacy_eventsource_reconnects_cursor_and_updates_endpoint() {
    use axum::{extract::OriginalUri, routing::get};
    use futures::StreamExt;
    struct Legacy {
        gets: AtomicUsize,
        close_first: Notify,
        replies: tokio::sync::broadcast::Sender<String>,
        headers: Mutex<Vec<HeaderMap>>,
        posts: Mutex<Vec<String>>,
        request_ids: Mutex<Vec<(Value, Value)>>,
    }
    async fn get_sse(State(state): State<Arc<Legacy>>, headers: HeaderMap) -> Response {
        state.headers.lock().await.push(headers);
        let number = state.gets.fetch_add(1, Ordering::SeqCst) + 1;
        let endpoint = format!(
            "retry: 1\nid: legacy-cursor\nevent: endpoint\ndata: /messages?stream={number}\n\n"
        );
        let first = futures::stream::once(async move {
            Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(endpoint))
        });
        if number == 1 {
            let end = futures::stream::unfold(state, |state| async move {
                state.close_first.notified().await;
                None::<(
                    Result<axum::body::Bytes, std::convert::Infallible>,
                    Arc<Legacy>,
                )>
            });
            response(Body::from_stream(first.chain(end)), "text/event-stream")
        } else {
            let ready = futures::stream::once(async {
                Ok::<_, std::convert::Infallible>(axum::body::Bytes::from_static(
                    b"data: {\"jsonrpc\":\"2.0\",\"method\":\"reattached\"}\n\n",
                ))
            });
            let replies =
                futures::stream::unfold(state.replies.subscribe(), |mut receiver| async move {
                    receiver.recv().await.ok().map(|value| {
                        (
                            Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(value)),
                            receiver,
                        )
                    })
                });
            response(
                Body::from_stream(first.chain(ready).chain(replies)),
                "text/event-stream",
            )
        }
    }
    async fn post_sse(
        State(state): State<Arc<Legacy>>,
        OriginalUri(uri): OriginalUri,
        headers: HeaderMap,
        Json(frame): Json<Value>,
    ) -> StatusCode {
        state.posts.lock().await.push(uri.to_string());
        state
            .request_ids
            .lock()
            .await
            .push((frame["method"].clone(), frame["id"].clone()));
        assert_eq!(headers["accept"], "application/custom");
        let result = if frame["method"] == "initialize" {
            json!({"protocolVersion":"2025-11-25"})
        } else {
            json!({"ok":true})
        };
        state
            .replies
            .send(format!(
                "data: {}\n\n",
                json!({"jsonrpc":"2.0","id":frame["id"],"result":result})
            ))
            .unwrap();
        StatusCode::ACCEPTED
    }
    let (replies, _) = tokio::sync::broadcast::channel(8);
    let state = Arc::new(Legacy {
        gets: AtomicUsize::new(0),
        close_first: Notify::new(),
        replies,
        headers: Mutex::new(vec![]),
        posts: Mutex::new(vec![]),
        request_ids: Mutex::new(vec![]),
    });
    let app = Router::new()
        .route("/sse", get(get_sse))
        .route("/messages", post(post_sse))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/sse", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let headers = McpHeaders::from_iter([("Accept".into(), "application/custom".into())]);
    let connection = platform_common::connect_sse(
        &url,
        None,
        &headers,
        platform_common::SseEndpointMode::EndpointEvent,
    )
    .await
    .unwrap();
    assert!(!connection.has_per_request_cancellation());
    let mut notifications = connection.notifications();
    state.close_first.notify_one();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if notifications.recv().await.unwrap().method == "reattached" {
                break;
            }
        }
    })
    .await
    .unwrap();
    let _: Value = connection.call("initialize", json!({})).await.unwrap();
    let _: Value = connection.call("tools/list", json!({})).await.unwrap();
    let _: Value = connection
        .call("tools/call", json!({"name":"echo","arguments":{}}))
        .await
        .unwrap();
    assert_eq!(
        *state.request_ids.lock().await,
        vec![
            (json!("initialize"), json!(0)),
            (json!("tools/list"), json!(1)),
            (json!("tools/call"), json!(2))
        ]
    );
    let headers = state.headers.lock().await;
    assert_eq!(headers[0]["accept"], "text/event-stream");
    assert_eq!(headers[1]["last-event-id"], "legacy-cursor");
    assert!(state
        .posts
        .lock()
        .await
        .iter()
        .all(|path| path == "/messages?stream=2"));
    connection.close();
    server.abort();
}

fn retain(name: &str, raw: &[u8]) {
    if let Some(root) = std::env::var_os("MCP_REMOTE_GAP_RECEIPTS_322") {
        let root = std::path::PathBuf::from(root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(name), raw).unwrap();
    }
}
async fn read_http(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut raw = vec![];
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte).await.unwrap();
        raw.push(byte[0]);
        if raw.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let headers = String::from_utf8_lossy(&raw);
    let length = headers
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|value| value.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    let start = raw.len();
    raw.resize(start + length, 0);
    stream.read_exact(&mut raw[start..]).await.unwrap();
    raw
}

#[tokio::test]
async fn native_322_http_request_cancellation_aborts_headers_and_body_independently() {
    cancellation_cases(
        &[(false, false), (true, false), (false, true), (true, true)],
        false,
    )
    .await;
}

#[tokio::test]
async fn native_322_initialized_http_cancel_wire_matches_native_session() {
    tokio::time::timeout(
        Duration::from_secs(5),
        cancellation_cases(&[(false, true)], true),
    )
    .await
    .unwrap();
}

async fn cancellation_cases(cases: &[(bool, bool)], initialized: bool) {
    use lingxi_core::host::mcp_result::{
        CancellationToken, JsonrpcMcpResultIo, McpInputRequiredIo, McpInputRequiredOptions,
    };
    for &(body, sdk) in cases {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let started = Arc::new(Notify::new());
        let ended = Arc::new(Notify::new());
        let peer_started = started.clone();
        let peer_ended = ended.clone();
        let cancelled = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let peer_cancelled = cancelled.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let started = peer_started.clone();
                let ended = peer_ended.clone();
                let cancelled = peer_cancelled.clone();
                tokio::spawn(async move {
                    let raw = read_http(&mut stream).await;
                    let header_end =
                        raw.windows(4).position(|part| part == b"\r\n\r\n").unwrap() + 4;
                    if raw.starts_with(b"GET ") {
                        retain("initialized-optional-get.raw", &raw);
                        stream.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                        return;
                    }
                    let json: Value = serde_json::from_slice(&raw[header_end..]).unwrap();
                    let method = json["method"].as_str().unwrap();
                    retain(
                        &format!(
                            "{}cancel-{}-{}-{}-{}.raw",
                            if initialized { "initialized-" } else { "" },
                            if body { "body" } else { "headers" },
                            if sdk { "sdk" } else { "direct" },
                            method.replace('/', "_"),
                            json["id"]
                        ),
                        &raw,
                    );
                    let headers = String::from_utf8_lossy(&raw[..header_end]).to_ascii_lowercase();
                    assert!(headers.contains("accept-encoding: identity\r\n"));
                    assert!(headers.contains("user-agent: lingxi/"));
                    assert!(headers.contains("accept: application/json, text/event-stream\r\n"));
                    if json.get("id").is_none() {
                        if json["method"] == "notifications/cancelled" {
                            cancelled.lock().unwrap().push(json["params"].clone());
                        }
                        stream.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                    } else if json["method"] == if initialized { "tools/call" } else { "hold" } {
                        if initialized {
                            assert_eq!(json["id"], 2);
                            assert!(headers.contains("mcp-protocol-version: 2025-11-25\r\n"));
                            assert!(headers.contains("mcp-session-id: gap-session\r\n"));
                        }
                        if body {
                            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
                        }
                        started.notify_one();
                        let mut byte = [0u8; 1];
                        let _ = stream.read(&mut byte).await;
                        ended.notify_one();
                    } else {
                        let result = match method {
                            "initialize" => {
                                json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"gap","version":"1"}})
                            }
                            "tools/list" => json!({"tools":[]}),
                            _ => json!({"ok":true}),
                        };
                        let answer =
                            json!({"jsonrpc":"2.0","id":json["id"],"result":result}).to_string();
                        let session = if method == "initialize" {
                            "mcp-session-id: gap-session\r\n"
                        } else {
                            ""
                        };
                        let response=format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{session}Content-Length: {}\r\nConnection: close\r\n\r\n{answer}",answer.len());
                        stream.write_all(response.as_bytes()).await.unwrap();
                    }
                });
            }
        });
        let connection = Arc::new(
            platform_common::connect_http(&url, None, &McpHeaders::new(), None)
                .await
                .unwrap(),
        );
        if initialized {
            let _:Value=connection.call("initialize",json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"lingxi","version":"0.12.0"}})).await.unwrap();
            connection
                .notify("notifications/initialized", json!({}))
                .unwrap();
            let _: Value = connection.call("tools/list", json!({})).await.unwrap();
        }
        let cancellation = CancellationToken::new();
        let (call, driver) = if sdk {
            let io = JsonrpcMcpResultIo {
                connection: connection.clone(),
                client_capabilities: Utf16JsonProjection::parse("{}").unwrap(),
            };
            let options = McpInputRequiredOptions {
                cancellation: cancellation.clone(),
                ..Default::default()
            };
            (
                None,
                Some(tokio::spawn(async move {
                    io.request(
                        if initialized { "tools/call" } else { "hold" },
                        Utf16JsonProjection::parse(if initialized {
                            "{\"name\":\"echo\",\"arguments\":{\"value\":\"WIRE_TOOL_INPUT_322\"}}"
                        } else {
                            "{\"private\":\"caller-owned\"}"
                        })
                        .unwrap(),
                        &options,
                    )
                    .await
                })),
            )
        } else {
            (
                Some(
                    connection
                        .start_call_unbounded("hold", json!({"private":"caller-owned"}))
                        .unwrap(),
                ),
                None,
            )
        };
        started.notified().await;
        let _: Value =
            tokio::time::timeout(Duration::from_secs(2), connection.call("ping", json!({})))
                .await
                .unwrap()
                .unwrap();
        if let Some(driver) = driver {
            cancellation.cancel();
            assert!(driver.await.unwrap().is_err());
        } else {
            drop(call);
        }
        tokio::time::timeout(Duration::from_secs(2), ended.notified())
            .await
            .expect("only the cancelled request stream must be aborted");
        let _: Value = connection.call("ping", json!({})).await.unwrap();
        if sdk {
            until(|| !cancelled.lock().unwrap().is_empty()).await;
            assert_eq!(
                *cancelled.lock().unwrap(),
                vec![
                    json!({"requestId":if initialized {2}else{0},"reason":"AbortError: remote-cancel"})
                ]
            );
        } else {
            assert!(
                cancelled.lock().unwrap().is_empty(),
                "direct StartedCall disposal must not invent an SDK notice"
            );
        }
        connection.close();
        server.abort();
    }
}
