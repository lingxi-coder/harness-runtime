use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

use futures_util::{stream, StreamExt};
use http_client::provider_transport;
use lingxi_llm_client::protocol::LlmError;
use lingxi_llm_client::HttpRequest;
use lingxi_llm_client::{HttpStreamRequest, Transport};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

async fn read_headers(socket: &mut TcpStream) -> String {
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        assert!(headers.len() < 16 * 1024);
        headers.push(socket.read_u8().await.unwrap());
    }
    String::from_utf8(headers).unwrap()
}

#[tokio::test]
async fn streaming_upload_and_error_download_are_incremental_binary_streams() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (upload_release, upload_ready) = oneshot::channel();
    let (response_release, response_ready) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let headers = read_headers(&mut socket).await.to_ascii_lowercase();
        assert!(headers.starts_with("post /files http/1.1\r\n"));
        assert!(headers.contains("content-length: 4\r\n"));
        assert!(headers.contains("content-type: application/octet-stream\r\n"));
        assert!(!headers.contains("transfer-encoding"));
        let mut chunk = [0; 2];
        socket.read_exact(&mut chunk).await.unwrap();
        assert_eq!(chunk, [0, 255]);
        // The producer cannot finish until the server receives the first
        // chunk. A transport that collects uploads before sending deadlocks.
        upload_release.send(()).unwrap();
        socket.read_exact(&mut chunk).await.unwrap();
        assert_eq!(chunk, [128, 1]);
        socket
            .write_all(b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 4\r\nRetry-After: 3\r\nConnection: close\r\n\r\n\x00\xff")
            .await
            .unwrap();
        response_ready.await.unwrap();
        socket.write_all(&[128, 1]).await.unwrap();
    });
    let body = stream::once(async { Ok(vec![0, 255]) }).chain(stream::once(async move {
        upload_ready.await.unwrap();
        Ok(vec![128, 1])
    }));
    let transport = provider_transport().unwrap();
    let mut response = tokio::time::timeout(
        Duration::from_secs(5),
        transport.send_stream(HttpStreamRequest {
            method: "POST".into(),
            url: format!("http://{address}/files"),
            headers: vec![
                ("content-type".into(), "application/octet-stream".into()),
                ("Content-Length".into(), "4".into()),
            ],
            body: body.map(|chunk| chunk.map(Into::into)).boxed(),
            content_length: 4,
            timeout: Some(Duration::from_secs(5)),
        }),
    )
    .await
    .expect("streaming upload must make progress before its source finishes")
    .unwrap();
    assert_eq!(response.status, 429);
    assert!(response
        .headers
        .contains(&("retry-after".into(), "3".into())));
    assert_eq!(
        response.body.next().await.unwrap().unwrap().as_ref(),
        [0, 255]
    );
    response_release.send(()).unwrap();
    assert_eq!(
        response.body.next().await.unwrap().unwrap().as_ref(),
        [128, 1]
    );
    assert!(response.body.next().await.is_none());
    server.await.unwrap();
}

#[tokio::test]
async fn streaming_upload_rejects_conflicting_framing_headers_without_polling() {
    for headers in [
        vec![("Content-Length".into(), "2".into())],
        vec![("transfer-encoding".into(), "chunked".into())],
        vec![
            ("Content-Length".into(), "1".into()),
            ("content-length".into(), "1".into()),
        ],
    ] {
        let polls = Arc::new(AtomicUsize::new(0));
        let body_polls = polls.clone();
        let body = stream::once(async move {
            body_polls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![255])
        });
        let result = provider_transport()
            .unwrap()
            .send_stream(HttpStreamRequest {
                method: "POST".into(),
                url: "http://127.0.0.1:9/files".into(),
                headers,
                body: body.map(|chunk| chunk.map(Into::into)).boxed(),
                content_length: 1,
                timeout: None,
            })
            .await;
        assert!(matches!(result, Err(LlmError::InvalidRequest { .. })));
        assert_eq!(polls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn streaming_upload_errors_do_not_expose_url_credentials() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    for transport in [provider_transport().unwrap(), provider_transport().unwrap()] {
        let error = transport
            .send_stream(HttpStreamRequest {
                method: "POST".into(),
                url: format!(
                    "http://{address}/upload?Signature=private-signature&upload_id=private-session"
                ),
                headers: vec![],
                content_length: 1,
                body: stream::once(async { Ok(vec![255].into()) }).boxed(),
                timeout: Some(Duration::from_secs(1)),
            })
            .await
            .err()
            .expect("closed listener must reject the connection");
        assert!(matches!(error, LlmError::Transport { .. }));
        let message = error.to_string();
        assert!(!message.contains("private-signature"), "{message}");
        assert!(!message.contains("private-session"), "{message}");
        assert!(!message.contains(&address.to_string()), "{message}");
    }
}

#[tokio::test]
async fn streaming_upload_surfaces_redirect_without_replaying_the_body() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_headers(&mut socket).await;
        assert_eq!(socket.read_u8().await.unwrap(), 255);
        socket
            .write_all(b"HTTP/1.1 307 Temporary Redirect\r\nLocation: /other\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
    });
    let response = provider_transport()
        .unwrap()
        .send_stream(HttpStreamRequest {
            method: "POST".into(),
            url: format!("http://{address}/files"),
            headers: vec![],
            body: stream::once(async { Ok(vec![255].into()) }).boxed(),
            content_length: 1,
            timeout: Some(Duration::from_secs(5)),
        })
        .await
        .unwrap();
    assert_eq!(response.status, 307);
    assert!(response
        .headers
        .contains(&("location".into(), "/other".into())));
    server.await.unwrap();
}

#[tokio::test]
async fn sdk_raw_download_exposes_error_headers_before_the_body_finishes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (release, ready) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_headers(&mut socket).await;
        socket
            .write_all(b"HTTP/1.1 503 Unavailable\r\nContent-Length: 2\r\nRetry-After: 5\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        ready.await.unwrap();
        socket.write_all(&[0, 255]).await.unwrap();
    });
    let transport = provider_transport().unwrap();
    let mut response = tokio::time::timeout(
        Duration::from_secs(5),
        transport.send(HttpRequest {
            method: "GET".into(),
            url: format!("http://{address}/files/content"),
            headers: vec![],
            body: Default::default(),
            timeout: None,
        }),
    )
    .await
    .expect("error response headers must not wait for its body")
    .unwrap();
    assert_eq!(response.status, 503);
    assert!(response
        .headers
        .contains(&("retry-after".into(), "5".into())));
    release.send(()).unwrap();
    assert_eq!(
        response.body.next().await.unwrap().unwrap().as_ref(),
        [0, 255]
    );
    assert!(response.body.next().await.is_none());
    server.await.unwrap();
}
