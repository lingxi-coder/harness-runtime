//! Passive Monitor sockets: DNS is vetted once and the TCP address is pinned.
use futures_util::{SinkExt, StreamExt};
use platform_api::http::is_public_monitor_address as public_ip;
use platform_api::http::{MonitorWebSocketFrame as Frame, MonitorWebSocketReceiver};
use platform_api::HttpError;
use std::{sync::Arc, time::Duration};
use tokio_tungstenite::tungstenite::{
    client::IntoClientRequest, protocol::WebSocketConfig, Message,
};

const MAX_FRAME_BYTES: usize = 1_048_576;

pub(crate) async fn preflight(raw: &str) -> Result<Vec<std::net::SocketAddr>, HttpError> {
    let url = url::Url::parse(raw).map_err(|e| HttpError::InvalidRequest(e.to_string()))?;
    if !raw.is_ascii()
        || raw.chars().any(|c| c.is_whitespace() || c.is_control())
        || !matches!(url.scheme(), "ws" | "wss")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(HttpError::InvalidRequest(
            "url must be a valid ASCII ws:// or wss:// URL with no userinfo or whitespace".into(),
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| HttpError::InvalidRequest("socket URL needs a host".into()))?
        .trim_matches(['[', ']']);
    let port = url
        .port_or_known_default()
        .ok_or_else(|| HttpError::InvalidRequest("socket URL needs a port".into()))?;
    let addresses: Vec<_> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| HttpError::Connection(e.to_string()))?
        .collect();
    if addresses.is_empty() || addresses.iter().any(|address| !public_ip(address.ip())) {
        return Err(HttpError::InvalidRequest(format!(
            "{host} resolves to a private, link-local, or cloud-metadata range"
        )));
    }
    Ok(addresses)
}

pub(crate) async fn connect(
    raw: String,
    protocols: Vec<String>,
    tls: Arc<rustls::ClientConfig>,
    proxy: Option<Arc<dyn platform_api::http::MonitorWebSocketProxy>>,
) -> Result<MonitorWebSocketReceiver, HttpError> {
    let addresses = preflight(&raw).await?;
    let endpoint =
        url::Url::parse(&raw).map_err(|error| HttpError::InvalidRequest(error.to_string()))?;
    let host = endpoint
        .host_str()
        .unwrap_or("")
        .trim_matches(['[', ']'])
        .to_string();
    let port = endpoint.port_or_known_default().unwrap_or(443);
    let secure = endpoint.scheme() == "wss";
    let mut request = raw
        .into_client_request()
        .map_err(|e| HttpError::InvalidRequest(e.to_string()))?;
    if !protocols.is_empty() {
        let mut seen = std::collections::HashSet::new();
        if protocols.iter().any(|p| {
            p.is_empty()
                || !p
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
                || !seen.insert(p)
        }) {
            return Err(HttpError::InvalidRequest(
                "protocols must be unique RFC 6455 tokens".into(),
            ));
        }
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            protocols
                .join(", ")
                .parse()
                .map_err(|e: http::header::InvalidHeaderValue| {
                    HttpError::InvalidRequest(e.to_string())
                })?,
        );
    }
    let config = WebSocketConfig {
        max_message_size: Some(MAX_FRAME_BYTES),
        max_frame_size: Some(MAX_FRAME_BYTES),
        ..Default::default()
    };
    let budget = Duration::from_secs(30);
    let (socket, _) = tokio::time::timeout(budget, async {
        // Connect to the exact vetted IP; keep original URL for HTTP Host and TLS SNI.
        let tunnel = if let Some(proxy) = proxy {
            proxy
                .connect_proxy(&host, port, secure)
                .await
                .map_err(|error| HttpError::Connection(error.to_string()))?
        } else {
            None
        };
        let tcp: Box<dyn platform_api::http::MonitorSocketIo> = if let Some(tunnel) = tunnel {
            tunnel
        } else {
            Box::new(
                tokio::net::TcpStream::connect(addresses.as_slice())
                    .await
                    .map_err(|e| HttpError::Connection(e.to_string()))?,
            )
        };
        tokio_tungstenite::client_async_tls_with_config(
            request,
            tcp,
            Some(config),
            Some(tokio_tungstenite::Connector::Rustls(tls)),
        )
        .await
        .map_err(|error| match error {
            tokio_tungstenite::tungstenite::Error::Http(response) => HttpError::Status {
                status: response.status().as_u16(),
                body: String::new(),
            },
            other => HttpError::Connection(other.to_string()),
        })
    })
    .await
    .map_err(|_| HttpError::Timeout(budget))??;
    Ok(receive(socket))
}

fn receive<S>(mut socket: tokio_tungstenite::WebSocketStream<S>) -> MonitorWebSocketReceiver
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(async move {
        loop {
            let message = tokio::select! {
                _ = tx.closed() => { let _ = tokio::time::timeout(Duration::from_secs(5), socket.close(None)).await; break; }
                message = socket.next() => message,
            };
            let frame = match message {
                Some(Ok(Message::Text(text))) => Ok(Frame::Text(text)),
                Some(Ok(Message::Binary(bytes))) => Ok(Frame::Binary(bytes.len())),
                Some(Ok(Message::Close(close))) => {
                    let (code, reason) = close.map_or((1005, String::new()), |c| {
                        (u16::from(c.code), c.reason.to_string())
                    });
                    let _ = tx.send(Ok(Frame::Closed(code, reason))).await;
                    break;
                }
                Some(Ok(Message::Ping(bytes))) => {
                    // TaskStop drops the receiver. A peer that stops reading
                    // must not keep this pump alive inside a blocked Pong write.
                    tokio::select! {
                        biased;
                        _ = tx.closed() => break,
                        result = socket.send(Message::Pong(bytes)) => {
                            if result.is_err() { break; }
                        }
                    }
                    continue;
                }
                Some(Ok(Message::Pong(_) | Message::Frame(_))) => continue,
                Some(Err(tokio_tungstenite::tungstenite::Error::Capacity(
                    tokio_tungstenite::tungstenite::error::CapacityError::MessageTooLong {
                        size,
                        ..
                    },
                ))) => {
                    let _ = tx.send(Ok(Frame::Oversized(size))).await;
                    break;
                }
                Some(Err(error)) => Err(HttpError::Connection(error.to_string())),
                None => break,
            };
            let failed = frame.is_err();
            if tx.send(frame).await.is_err() || failed {
                break;
            }
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_network_guard_blocks_private_metadata_and_mapped_addresses() {
        for ip in [
            "127.0.0.1",
            "169.254.169.254",
            "10.0.0.1",
            "100.64.0.1",
            "::1",
            "::ffff:127.0.0.1",
            "fe80::1",
            "fc00::1",
        ] {
            assert!(!public_ip(ip.parse().unwrap()), "{ip}");
        }
        assert!(public_ip("8.8.8.8".parse().unwrap()));
        assert!(public_ip("2606:4700::1111".parse().unwrap()));
    }
    #[tokio::test]
    async fn passive_socket_reports_text_binary_and_close_without_sending_a_request() {
        let (client, server) = tokio::io::duplex(4096);
        let server = tokio::spawn(async move {
            let mut socket = tokio_tungstenite::WebSocketStream::from_raw_socket(
                server,
                tokio_tungstenite::tungstenite::protocol::Role::Server,
                None,
            )
            .await;
            socket.send(Message::Text("ready".into())).await.unwrap();
            socket.send(Message::Binary(vec![0, 1, 2])).await.unwrap();
            socket.close(None).await.unwrap();
        });
        let socket = tokio_tungstenite::WebSocketStream::from_raw_socket(
            client,
            tokio_tungstenite::tungstenite::protocol::Role::Client,
            None,
        )
        .await;
        let mut rx = receive(socket);
        assert_eq!(
            rx.recv().await.unwrap().unwrap(),
            Frame::Text("ready".into())
        );
        assert_eq!(rx.recv().await.unwrap().unwrap(), Frame::Binary(3));
        assert_eq!(
            rx.recv().await.unwrap().unwrap(),
            Frame::Closed(1005, String::new())
        );
        server.await.unwrap();
    }
    #[tokio::test]
    async fn dropping_monitor_receiver_closes_the_live_socket() {
        let (client, server) = tokio::io::duplex(4096);
        let mut server = tokio_tungstenite::WebSocketStream::from_raw_socket(
            server,
            tokio_tungstenite::tungstenite::protocol::Role::Server,
            None,
        )
        .await;
        let client = tokio_tungstenite::WebSocketStream::from_raw_socket(
            client,
            tokio_tungstenite::tungstenite::protocol::Role::Client,
            None,
        )
        .await;
        drop(receive(client));
        let frame = tokio::time::timeout(Duration::from_secs(1), server.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(frame, Message::Close(_)), "{frame:?}");
    }

    #[tokio::test]
    async fn oversized_frames_are_dropped_with_their_byte_count() {
        let (client, server) = tokio::io::duplex(4096);
        let mut server = tokio_tungstenite::WebSocketStream::from_raw_socket(
            server,
            tokio_tungstenite::tungstenite::protocol::Role::Server,
            None,
        )
        .await;
        let client = tokio_tungstenite::WebSocketStream::from_raw_socket(
            client,
            tokio_tungstenite::tungstenite::protocol::Role::Client,
            Some(WebSocketConfig {
                max_message_size: Some(16),
                max_frame_size: Some(16),
                ..Default::default()
            }),
        )
        .await;
        let mut rx = receive(client);
        server.send(Message::Text("x".repeat(32))).await.unwrap();
        assert_eq!(rx.recv().await.unwrap().unwrap(), Frame::Oversized(32));
    }
    #[tokio::test]
    async fn dropping_monitor_receiver_cancels_a_backpressured_pong() {
        use std::pin::Pin;
        use std::task::{Context, Poll};
        use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

        struct BackpressuredSocket {
            ping: Option<[u8; 3]>,
            writing: Arc<tokio::sync::Notify>,
            dropped: Option<tokio::sync::oneshot::Sender<()>>,
        }
        impl AsyncRead for BackpressuredSocket {
            fn poll_read(
                mut self: Pin<&mut Self>,
                _: &mut Context<'_>,
                buf: &mut ReadBuf<'_>,
            ) -> Poll<std::io::Result<()>> {
                if let Some(ping) = self.ping.take() {
                    buf.put_slice(&ping);
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Pending
                }
            }
        }
        impl AsyncWrite for BackpressuredSocket {
            fn poll_write(
                self: Pin<&mut Self>,
                _: &mut Context<'_>,
                _: &[u8],
            ) -> Poll<std::io::Result<usize>> {
                self.writing.notify_one();
                Poll::Pending
            }
            fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
                Poll::Pending
            }
            fn poll_shutdown(
                self: Pin<&mut Self>,
                _: &mut Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Pending
            }
        }
        impl Drop for BackpressuredSocket {
            fn drop(&mut self) {
                if let Some(dropped) = self.dropped.take() {
                    let _ = dropped.send(());
                }
            }
        }
        let writing = Arc::new(tokio::sync::Notify::new());
        let (dropped, released) = tokio::sync::oneshot::channel();
        let socket = tokio_tungstenite::WebSocketStream::from_raw_socket(
            BackpressuredSocket {
                // One valid, unmasked server Ping frame, with payload "x".
                ping: Some([0x89, 0x01, b'x']),
                writing: writing.clone(),
                dropped: Some(dropped),
            },
            tokio_tungstenite::tungstenite::protocol::Role::Client,
            None,
        )
        .await;
        let receiver = receive(socket);
        tokio::time::timeout(Duration::from_secs(1), writing.notified())
            .await
            .expect("real pump must attempt its Pong before cancellation");
        drop(receiver);
        tokio::time::timeout(Duration::from_secs(1), released)
            .await
            .expect("TaskStop must release a socket even while its Pong write is pending")
            .expect("underlying socket was dropped");
    }

    #[tokio::test]
    async fn endpoint_preflight_rejects_private_dns_before_connecting() {
        for url in [
            "ws://127.0.0.1:9",
            "ws://localhost:9",
            "wss://user:pass@example.com",
        ] {
            assert!(preflight(url).await.is_err(), "{url}");
        }
    }
}
