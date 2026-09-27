//! [`HttpTransport`] contract test suite.
//!
//! The suite asserts a small number of invariants every transport must
//! honour. We bind a real in-process hyper server on `127.0.0.1:0` and run
//! both the production `reqwest`-based transport and the mock transport
//! against it where possible.
//!
//! Invariants:
//!
//! * `request(GET /ok)` returns status 200 and a non-empty body.
//! * `request(GET <closed-port>)` returns an [`HttpError`] (not a panic).
//! * `stream_sse(GET /sse)` yields at least one event and the stream then
//!   terminates rather than hanging forever.

use futures::StreamExt;
use platform_api::HttpTransport;
use protocol::{HttpMethod, HttpRequest};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;

/// Run the standard [`HttpTransport`] contract against an impl and a base
/// URL for an echo server that exposes `GET /ok` and `GET /sse`.
///
/// # Panics
///
/// Panics on the first invariant violation.
pub async fn http_transport_contract_tests<H: HttpTransport>(http: &H, base_url: &str) {
    test_request_returns_status_and_body(http, base_url).await;
    test_request_failure_yields_error(http).await;
    test_stream_sse_terminates(http, base_url).await;
}

async fn test_request_returns_status_and_body<H: HttpTransport>(http: &H, base_url: &str) {
    let req = HttpRequest {
        method: HttpMethod::Get,
        url: format!("{base_url}/ok"),
        headers: Vec::new(),
        body: None,
        body_bytes: None,
        timeout: Some(Duration::from_secs(5)),
    };
    let resp = http.request(req).await.expect("request must succeed");
    assert_eq!(resp.status, 200, "expected 200, got {}", resp.status);
    assert!(
        !resp.body.is_empty(),
        "response body must not be empty, got {:?}",
        resp.body
    );
}

async fn test_request_failure_yields_error<H: HttpTransport>(http: &H) {
    let req = HttpRequest {
        method: HttpMethod::Get,
        url: "http://127.0.0.1:1/never-listens".into(),
        headers: Vec::new(),
        body: None,
        body_bytes: None,
        timeout: Some(Duration::from_secs(2)),
    };
    let r = http.request(req).await;
    assert!(
        r.is_err(),
        "request to a closed port must return an HttpError, got Ok({r:?})"
    );
}

async fn test_stream_sse_terminates<H: HttpTransport>(http: &H, base_url: &str) {
    let req = HttpRequest {
        method: HttpMethod::Get,
        url: format!("{base_url}/sse"),
        headers: vec![("Accept".into(), "text/event-stream".into())],
        body: None,
        body_bytes: None,
        timeout: Some(Duration::from_secs(5)),
    };
    let mut stream = http.stream_sse(req).await.expect("stream_sse must succeed");
    let mut events = 0u32;
    while let Some(_evt) = stream.next().await {
        events += 1;
        if events > 100 {
            break; // safety cap — the echo server only sends 3
        }
    }
    assert!(
        events > 0,
        "SSE stream must yield at least one event before closing"
    );
}

// ---- in-process echo server -------------------------------------------------

/// Handle to a running echo server. Drop or call [`Self::shutdown`] to free
/// the bound port and stop the accept loop.
pub struct EchoServer {
    base_url: String,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl EchoServer {
    /// Base URL of the form `http://127.0.0.1:<port>`.
    #[must_use]
    pub fn base_url(&self) -> String {
        self.base_url.clone()
    }

    /// Trigger graceful shutdown. Idempotent.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        // Give the accept loop a tick to exit cleanly.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

impl Drop for EchoServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

/// Spawn a tiny in-process hyper server bound to `127.0.0.1:0`.
///
/// Routes:
/// * `GET /ok` — `200 "ok"` (plain text).
/// * `GET /sse` — `text/event-stream` with three `data:` frames, each
///   terminated by `\n\n`, then closes the connection.
///
/// Any other route returns `404`.
///
/// # Panics
///
/// Panics if the local socket cannot be bound — that is a hard test
/// infrastructure failure.
pub async fn spawn_echo_server() -> EchoServer {
    use http_body_util::Full;
    use hyper::body::Bytes;
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper::{Request, Response, StatusCode};
    use hyper_util::rt::TokioIo;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind 127.0.0.1:0");
    let addr = listener.local_addr().expect("local_addr");
    let base_url = format!("http://{addr}");

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accept = listener.accept() => {
                    let Ok((stream, _peer)) = accept else { break };
                    let io = TokioIo::new(stream);
                    tokio::spawn(async move {
                        let svc = service_fn(|req: Request<hyper::body::Incoming>| async move {
                            let path = req.uri().path().to_string();
                            let resp: Response<Full<Bytes>> = match path.as_str() {
                                "/ok" => Response::builder()
                                    .status(StatusCode::OK)
                                    .header("content-type", "text/plain")
                                    .body(Full::new(Bytes::from_static(b"ok")))
                                    .unwrap(),
                                "/sse" => {
                                    let body = "data: a\n\ndata: b\n\ndata: c\n\n";
                                    Response::builder()
                                        .status(StatusCode::OK)
                                        .header("content-type", "text/event-stream")
                                        .header("cache-control", "no-cache")
                                        .body(Full::new(Bytes::from_static(body.as_bytes())))
                                        .unwrap()
                                }
                                _ => Response::builder()
                                    .status(StatusCode::NOT_FOUND)
                                    .body(Full::new(Bytes::from_static(b"not found")))
                                    .unwrap(),
                            };
                            Ok::<_, Infallible>(resp)
                        });
                        let _ = http1::Builder::new().serve_connection(io, svc).await;
                    });
                }
            }
        }
    });

    EchoServer {
        base_url,
        shutdown: Some(shutdown_tx),
    }
}
