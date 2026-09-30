//! `WindowsHttp::stream_sse` smoke test against a local axum server.
//!
//! Host-portable: hyper / axum / reqwest all run identically on macOS, Linux,
//! and Windows, so this same suite exercises the adapter regardless of CI host.

use axum::body::Body;
use axum::http::{header, Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::Router;
use futures_util::StreamExt;
use lingxi_core::host::HttpTransport;
use lingxi_core::types::{HttpMethod, HttpRequest};
use platform_windows::WindowsHttp;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpListener;

const SSE_BODY: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"model\":\"x\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"reasoning\"}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

async fn sse_handler() -> impl IntoResponse {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from(SSE_BODY))
        .expect("response")
}

async fn spawn_sse_server() -> SocketAddr {
    let app = Router::new().route("/v1/messages", post(sse_handler));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        axum::serve(listener, app.into_make_service())
            .await
            .expect("serve");
    });
    addr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream_sse_parses_message_start_and_deltas() {
    let addr = spawn_sse_server().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let http = WindowsHttp::new();
    let req = HttpRequest {
        method: HttpMethod::Post,
        url: format!("http://{addr}/v1/messages"),
        headers: vec![("accept".into(), "text/event-stream".into())],
        body: Some("{\"stream\":true}".into()),
        body_bytes: None,
        timeout: Some(Duration::from_secs(5)),
    };

    let mut stream = http.stream_sse(req).await.expect("stream open");
    let mut got = Vec::new();
    while let Some(item) = stream.next().await {
        let event = item.expect("event");
        got.push(event.event_type.clone().unwrap_or_default());
    }
    assert_eq!(
        got,
        vec![
            "message_start".to_string(),
            "content_block_delta".to_string(),
            "content_block_delta".to_string(),
            "message_stop".to_string(),
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream_sse_surfaces_non_2xx_at_open_time() {
    async fn fail_handler() -> impl IntoResponse {
        (StatusCode::TOO_MANY_REQUESTS, "rate limited")
    }
    let app = Router::new().route("/v1/messages", post(fail_handler));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        axum::serve(listener, app.into_make_service())
            .await
            .expect("serve");
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let http = WindowsHttp::new();
    let req = HttpRequest {
        method: HttpMethod::Post,
        url: format!("http://{addr}/v1/messages"),
        headers: vec![],
        body: None,
        body_bytes: None,
        timeout: Some(Duration::from_secs(5)),
    };
    let result = http.stream_sse(req).await;
    let Err(err) = result else {
        panic!("stream should fail for 429");
    };
    match err {
        lingxi_core::host::HttpError::Status { status, body } => {
            assert_eq!(status, 429);
            assert!(body.contains("rate limited"));
        }
        other => panic!("expected Status, got {other:?}"),
    }
}
