//! The host watchdog, not a hidden SDK default, owns model read deadlines.
use futures::{poll, StreamExt};
use lingxi_llm_client::{HttpRequest, Transport};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn provider_http_keeps_reading_past_the_sdk_default_idle_limit() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (finish, waiting) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0; 4096];
        socket.read(&mut buf).await.unwrap();
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\na")
            .await
            .unwrap();
        waiting.await.unwrap();
        socket.write_all(b"b").await.unwrap();
    });
    let mut response = http_client::provider_transport()
        .unwrap()
        .send(HttpRequest {
            method: "GET".into(),
            url,
            headers: vec![],
            body: Default::default(),
            timeout: None,
        })
        .await
        .unwrap();
    assert_eq!(response.body.next().await.unwrap().unwrap(), "a");
    tokio::time::pause();
    let next = response.body.next();
    tokio::pin!(next);
    assert!(poll!(&mut next).is_pending());
    tokio::time::advance(Duration::from_secs(301)).await;
    assert!(
        poll!(&mut next).is_pending(),
        "transport preempted the host watchdog"
    );
    tokio::time::resume();
    finish.send(()).unwrap();
    assert_eq!(next.await.unwrap().unwrap(), "b");
    server.await.unwrap();
}
