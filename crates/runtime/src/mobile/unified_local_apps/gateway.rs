//! Per-installation loopback gateway. A URL contains an unguessable run epoch;
//! every accepted connection is cancelled when that epoch is stopped.
use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
};
use tokio_util::sync::CancellationToken;

const MAX_HEAD: usize = 32 * 1024;

#[derive(Clone)]
pub(super) struct Route {
    pub run_id: String,
    pub root: PathBuf,
    pub entry: String,
    pub entry_service: Option<String>,
    pub services: BTreeMap<String, u16>,
    pub port: u16,
    pub capabilities: Vec<String>,
    pub cancel: CancellationToken,
}

pub(super) fn spawn(
    listener: impl Into<Arc<TcpListener>>,
    route: Route,
) -> tokio::task::JoinHandle<()> {
    let listener = listener.into();
    tokio::spawn(async move {
        let route = Arc::new(route);
        let permits = Arc::new(Semaphore::new(32));
        loop {
            let socket = tokio::select! {
                _ = route.cancel.cancelled() => break,
                accepted = listener.accept() => match accepted { Ok((socket, peer)) if peer.ip().is_loopback() => socket, Ok(_) => continue, Err(_) => break },
            };
            let Ok(permit) = permits.clone().try_acquire_owned() else {
                continue;
            };
            let route = route.clone();
            tokio::spawn(async move {
                let _permit = permit;
                tokio::select! {
                    _ = route.cancel.cancelled() => {},
                    _ = serve(socket, &route) => {},
                }
            });
        }
    })
}

async fn error(socket: &mut TcpStream, code: u16, message: &str) -> std::io::Result<()> {
    let body = message.as_bytes();
    socket.write_all(format!("HTTP/1.1 {code} Error\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n", body.len()).as_bytes()).await?;
    socket.write_all(body).await
}

async fn serve(mut socket: TcpStream, route: &Route) -> std::io::Result<()> {
    let mut bytes = Vec::with_capacity(4096);
    let head_end = loop {
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break index + 4;
        }
        if bytes.len() >= MAX_HEAD {
            return error(&mut socket, 431, "request headers too large").await;
        }
        let mut buf = [0_u8; 4096];
        let count = match tokio::time::timeout(Duration::from_secs(10), socket.read(&mut buf)).await
        {
            Ok(result) => result?,
            Err(_) => return error(&mut socket, 408, "request timeout").await,
        };
        if count == 0 {
            return Ok(());
        }
        bytes.extend_from_slice(&buf[..count]);
    };
    if head_end > MAX_HEAD {
        return error(&mut socket, 431, "request headers too large").await;
    }
    let mut headers = [httparse::EMPTY_HEADER; 96];
    let mut request = httparse::Request::new(&mut headers);
    if !matches!(
        request.parse(&bytes[..head_end]),
        Ok(httparse::Status::Complete(_))
    ) {
        return error(&mut socket, 400, "invalid HTTP request").await;
    }
    let method = request.method.unwrap_or("");
    let target = request.path.unwrap_or("");
    let expected_host = format!("127.0.0.1:{}", route.port);
    let expected_origin = format!("http://{expected_host}");
    let hosts: Vec<_> = request
        .headers
        .iter()
        .filter(|h| h.name.eq_ignore_ascii_case("host"))
        .collect();
    if hosts.len() != 1 || hosts[0].value != expected_host.as_bytes() {
        return error(&mut socket, 403, "invalid app host").await;
    }
    if request
        .headers
        .iter()
        .any(|h| h.name.eq_ignore_ascii_case("origin") && h.value != expected_origin.as_bytes())
    {
        return error(
            &mut socket,
            403,
            "cross-origin app requests are not allowed",
        )
        .await;
    }
    let prefix = format!("/__run/{}/", route.run_id);
    let Some(relative) = target.strip_prefix(&prefix) else {
        // Ordinary frameworks use root-absolute URLs. Redirect those into the
        // exact referring run, rather than granting the current run by origin.
        // Same-origin Referer cannot be forged by page JavaScript. A stale
        // document consequently cannot enter a replacement run.
        let epoch_referrer = format!("{expected_origin}{prefix}");
        let from_current = request.headers.iter().any(|h| {
            h.name.eq_ignore_ascii_case("referer")
                && std::str::from_utf8(h.value)
                    .is_ok_and(|value| value.starts_with(&epoch_referrer))
        });
        if from_current
            && target.starts_with('/')
            && !target.starts_with("//")
            && !target.starts_with("/__run/")
        {
            let static_base = if route.entry_service.is_none() {
                route
                    .entry
                    .rsplit_once('/')
                    .map(|(parent, _)| format!("{parent}/"))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let relative = target.trim_start_matches('/');
            let target = if relative.is_empty() || relative.starts_with('?') {
                format!("{}{relative}", route.entry)
            } else {
                format!("{static_base}{relative}")
            };
            socket.write_all(format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: {prefix}{target}\r\nContent-Length: 0\r\nConnection: close\r\nCache-Control: no-store\r\nReferrer-Policy: same-origin\r\n\r\n").as_bytes()).await?;
            return Ok(());
        }
        return error(
            &mut socket,
            410,
            "this application run is no longer available",
        )
        .await;
    };
    // Reject request-smuggling ambiguity before opening any upstream socket.
    let lengths: Vec<_> = request
        .headers
        .iter()
        .filter(|h| h.name.eq_ignore_ascii_case("content-length"))
        .collect();
    let transfers: Vec<_> = request
        .headers
        .iter()
        .filter(|h| h.name.eq_ignore_ascii_case("transfer-encoding"))
        .collect();
    if lengths.len() > 1 || transfers.len() > 1 || (!lengths.is_empty() && !transfers.is_empty()) {
        return error(&mut socket, 400, "ambiguous request body framing").await;
    }
    if lengths.iter().any(|h| {
        std::str::from_utf8(h.value)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .is_none()
    }) {
        return error(&mut socket, 400, "invalid content length").await;
    }
    if transfers
        .iter()
        .any(|h| !h.value.eq_ignore_ascii_case(b"chunked"))
    {
        return error(&mut socket, 400, "unsupported transfer encoding").await;
    }
    let named_service = relative.starts_with("__service/");
    let (service, upstream_path) = if let Some(service_route) = relative.strip_prefix("__service/")
    {
        let Some((name, path)) = service_route.split_once('/') else {
            return error(&mut socket, 404, "unknown service").await;
        };
        (Some(name), format!("/{path}"))
    } else {
        (route.entry_service.as_deref(), format!("/{relative}"))
    };
    if let Some(name) = service {
        let Some(port) = route.services.get(name) else {
            return error(&mut socket, 404, "unknown service").await;
        };
        let mut upstream = match tokio::time::timeout(
            Duration::from_secs(5),
            TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, *port)),
        )
        .await
        {
            Ok(Ok(upstream)) => upstream,
            _ => return error(&mut socket, 502, "application service is unavailable").await,
        };
        let upgrade = request.headers.iter().any(|h| {
            h.name.eq_ignore_ascii_case("upgrade") && h.value.eq_ignore_ascii_case(b"websocket")
        });
        let mut head = format!("{method} {upstream_path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Forwarded-Prefix: {prefix}\r\n").into_bytes();
        for header in request.headers.iter() {
            if [
                "host",
                "connection",
                "proxy-connection",
                "proxy-authorization",
                "x-forwarded-prefix",
            ]
            .iter()
            .any(|name| header.name.eq_ignore_ascii_case(name))
            {
                continue;
            }
            head.extend_from_slice(header.name.as_bytes());
            head.extend_from_slice(b": ");
            head.extend_from_slice(header.value);
            head.extend_from_slice(b"\r\n");
        }
        head.extend_from_slice(if upgrade {
            b"Connection: Upgrade\r\n\r\n"
        } else {
            b"Connection: close\r\n\r\n"
        });
        upstream.write_all(&head).await?;
        upstream.write_all(&bytes[head_end..]).await?;
        let response_prefix = if named_service {
            format!("{prefix}__service/{name}/")
        } else {
            prefix.clone()
        };
        let (mut client_read, mut client_write) = socket.split();
        let (mut upstream_read, mut upstream_write) = upstream.split();
        // Only bounded HTTP headers are inspected. Bodies and upgraded binary
        // streams remain opaque, concurrently flowing in both directions.
        let security = security_headers(route);
        tokio::try_join!(
            async {
                tokio::io::copy(&mut client_read, &mut upstream_write).await?;
                upstream_write.shutdown().await
            },
            forward_response(
                &mut upstream_read,
                &mut client_write,
                &response_prefix,
                *port,
                &security,
            ),
        )?;
        return Ok(());
    }
    if method != "GET" && method != "HEAD" {
        return error(&mut socket, 405, "static applications accept GET and HEAD").await;
    }
    let path = relative.split('?').next().unwrap_or("");
    let path = if path.is_empty() {
        route.entry.as_str()
    } else {
        path
    };
    let Some(file) = confined_file(&route.root, path) else {
        return error(&mut socket, 404, "asset not found").await;
    };
    let mut file_handle = match tokio::fs::File::open(&file).await {
        Ok(file) => file,
        Err(_) => return error(&mut socket, 404, "asset not found").await,
    };
    let metadata = file_handle.metadata().await?;
    if !metadata.is_file() {
        return error(&mut socket, 404, "asset not found").await;
    }
    let content_type = match file.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "wasm" => "application/wasm",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    };
    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n{}\r\n", metadata.len(), security_headers(route)).as_bytes()).await?;
    if method == "GET" {
        tokio::io::copy(&mut file_handle, &mut socket).await?;
    }
    Ok(())
}

fn security_headers(route: &Route) -> String {
    let network = route
        .capabilities
        .iter()
        .any(|capability| capability == "network");
    let remote_sources = if network { " https: wss:" } else { "" };
    let remote_scripts = if network { " https:" } else { "" };
    let camera = if route.capabilities.iter().any(|c| c == "camera") {
        "(self)"
    } else {
        "()"
    };
    let microphone = if route.capabilities.iter().any(|c| c == "microphone") {
        "(self)"
    } else {
        "()"
    };
    format!("Content-Security-Policy: sandbox allow-scripts allow-forms allow-modals allow-downloads allow-same-origin; default-src 'self' data: blob:{remote_scripts}; script-src 'self' 'unsafe-inline' 'unsafe-eval' blob:{remote_scripts}; style-src 'self' 'unsafe-inline'{remote_scripts}; connect-src 'self' ws://127.0.0.1:{}{remote_sources}; worker-src 'none'; object-src 'none'; base-uri 'none'; frame-src 'none'; frame-ancestors 'none'\r\nPermissions-Policy: camera={camera}, microphone={microphone}, geolocation=()\r\nReferrer-Policy: same-origin\r\nX-Frame-Options: DENY\r\n", route.port)
}

async fn forward_response<R: tokio::io::AsyncRead + Unpin, W: tokio::io::AsyncWrite + Unpin>(
    upstream: &mut R,
    client: &mut W,
    prefix: &str,
    port: u16,
    security: &str,
) -> std::io::Result<()> {
    let mut bytes = Vec::new();
    loop {
        let end = loop {
            if let Some(index) = bytes.windows(4).position(|p| p == b"\r\n\r\n") {
                break index + 4;
            }
            if bytes.len() > MAX_HEAD {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "upstream headers too large",
                ));
            }
            let mut chunk = [0_u8; 4096];
            let count = tokio::time::timeout(Duration::from_secs(30), upstream.read(&mut chunk))
                .await
                .map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::TimedOut, "upstream response timeout")
                })??;
            if count == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "upstream closed before headers",
                ));
            }
            bytes.extend_from_slice(&chunk[..count]);
        };
        if end > MAX_HEAD {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "upstream headers too large",
            ));
        }
        let mut headers = [httparse::EMPTY_HEADER; 96];
        let mut response = httparse::Response::new(&mut headers);
        if !matches!(
            response.parse(&bytes[..end]),
            Ok(httparse::Status::Complete(_))
        ) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid upstream HTTP response",
            ));
        }
        let code = response.code.unwrap_or(502);
        let mut head =
            format!("HTTP/1.1 {code} {}\r\n", response.reason.unwrap_or("")).into_bytes();
        for header in response.headers.iter() {
            head.extend_from_slice(header.name.as_bytes());
            head.extend_from_slice(b": ");
            if header.name.eq_ignore_ascii_case("location") {
                if let Ok(location) = std::str::from_utf8(header.value) {
                    head.extend_from_slice(rewrite_location(location, prefix, port).as_bytes());
                } else {
                    head.extend_from_slice(header.value);
                }
            } else {
                head.extend_from_slice(header.value);
            }
            head.extend_from_slice(b"\r\n");
        }
        if code >= 200 {
            head.extend_from_slice(security.as_bytes());
        }
        head.extend_from_slice(b"\r\n");
        client.write_all(&head).await?;
        bytes.drain(..end);
        if (100..200).contains(&code) && code != 101 {
            continue;
        }
        client.write_all(&bytes).await?;
        tokio::io::copy(upstream, client).await?;
        return client.shutdown().await;
    }
}
fn rewrite_location(location: &str, prefix: &str, port: u16) -> String {
    let local = location
        .strip_prefix(&format!("http://127.0.0.1:{port}/"))
        .or_else(|| location.strip_prefix(&format!("http://localhost:{port}/")));
    if let Some(path) = local {
        return format!("{prefix}{path}");
    }
    if location.starts_with('/') && !location.starts_with("//") {
        return format!("{prefix}{}", location.trim_start_matches('/'));
    }
    location.to_owned()
}

fn confined_file(root: &Path, raw: &str) -> Option<PathBuf> {
    let mut decoded = Vec::new();
    let bytes = raw.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor] == b'%' {
            let hex = std::str::from_utf8(bytes.get(cursor + 1..cursor + 3)?).ok()?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            cursor += 3;
        } else {
            decoded.push(bytes[cursor]);
            cursor += 1;
        }
    }
    let value = String::from_utf8(decoded).ok()?;
    if value.contains(['\\', '\0']) {
        return None;
    }
    let path = Path::new(&value);
    if path.is_absolute()
        || path
            .components()
            .any(|p| !matches!(p, Component::Normal(_)))
    {
        return None;
    }
    let root = root.canonicalize().ok()?;
    let candidate = root.join(path).canonicalize().ok()?;
    candidate.starts_with(&root).then_some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn exchange(port: u16, bytes: Vec<u8>) -> Vec<u8> {
        let mut socket = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        socket.write_all(&bytes).await.unwrap();
        let mut output = Vec::new();
        tokio::time::timeout(Duration::from_secs(3), socket.read_to_end(&mut output))
            .await
            .unwrap()
            .unwrap();
        output
    }
    #[tokio::test]
    async fn static_epoch_host_and_path_are_checked() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("index.html"), "fixture").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cancel = CancellationToken::new();
        let task = spawn(
            listener,
            Route {
                run_id: "current".into(),
                root: temp.path().into(),
                entry: "index.html".into(),
                entry_service: None,
                services: BTreeMap::new(),
                port,
                capabilities: vec![],
                cancel: cancel.clone(),
            },
        );
        for (path, host, expected) in [
            ("/__run/current/", format!("127.0.0.1:{port}"), "200"),
            ("/__run/old/", format!("127.0.0.1:{port}"), "410"),
            (
                "/__run/current/%2e%2e/secret",
                format!("127.0.0.1:{port}"),
                "404",
            ),
            ("/__run/current/", "attacker.test".into(), "403"),
        ] {
            let response = exchange(
                port,
                format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n\r\n").into_bytes(),
            )
            .await;
            assert!(String::from_utf8_lossy(&response).starts_with(&format!("HTTP/1.1 {expected}")));
        }
        cancel.cancel();
        task.await.unwrap();
    }
    #[tokio::test]
    async fn post_binary_bytes_and_sse_are_forwarded_without_buffering_response() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = upstream.accept().await.unwrap();
            let mut input = Vec::new();
            let mut buf = [0; 1024];
            loop {
                let n = socket.read(&mut buf).await.unwrap();
                input.extend_from_slice(&buf[..n]);
                if input.ends_with(&[0, 255, 1, 2]) {
                    break;
                }
            }
            assert!(input.starts_with(b"POST /upload HTTP/1.1"));
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: ready\n\n").await.unwrap();
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cancel = CancellationToken::new();
        let task = spawn(
            listener,
            Route {
                run_id: "epoch".into(),
                root: PathBuf::new(),
                entry: String::new(),
                entry_service: Some("api".into()),
                services: BTreeMap::from([("api".into(), upstream_port)]),
                port,
                capabilities: vec![],
                cancel: cancel.clone(),
            },
        );
        let mut request=format!("POST /__run/epoch/upload HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 4\r\n\r\n").into_bytes();
        request.extend_from_slice(&[0, 255, 1, 2]);
        let response = exchange(port, request).await;
        assert!(response.ends_with(b"data: ready\n\n"));
        server.await.unwrap();
        cancel.cancel();
        task.await.unwrap();
    }
}

#[cfg(test)]
mod stream_tests {
    use super::*;
    async fn header(socket: &mut TcpStream) -> Vec<u8> {
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let byte = socket.read_u8().await.unwrap();
            head.push(byte);
            assert!(head.len() < MAX_HEAD);
        }
        head
    }
    #[tokio::test]
    async fn websocket_binary_frames_flow_both_ways_and_stop_closes_stream() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = upstream.accept().await.unwrap();
            let head = header(&mut socket).await;
            assert!(head.starts_with(b"GET /socket HTTP/1.1"));
            socket.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: test\r\n\r\n").await.unwrap();
            let mut masked = [0_u8; 8];
            socket.read_exact(&mut masked).await.unwrap();
            assert_eq!(masked, [0x82, 0x82, 1, 2, 3, 4, 1, 0xfd]);
            socket.write_all(&[0x82, 2, 0, 0xff]).await.unwrap();
            let _ = reached_tx.send(());
            let mut byte = [0; 1];
            let eof = tokio::time::timeout(Duration::from_secs(3), socket.read(&mut byte))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(eof, 0);
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cancel = CancellationToken::new();
        let task = spawn(
            listener,
            Route {
                run_id: "ws".into(),
                root: PathBuf::new(),
                entry: String::new(),
                entry_service: None,
                services: BTreeMap::from([("socket".into(), upstream_port)]),
                port,
                capabilities: vec![],
                cancel: cancel.clone(),
            },
        );
        let mut client = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        client.write_all(format!("GET /__run/ws/__service/socket/socket HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n").as_bytes()).await.unwrap();
        assert!(header(&mut client).await.starts_with(b"HTTP/1.1 101"));
        client
            .write_all(&[0x82, 0x82, 1, 2, 3, 4, 1, 0xfd])
            .await
            .unwrap();
        let mut frame = [0; 4];
        client.read_exact(&mut frame).await.unwrap();
        assert_eq!(frame, [0x82, 2, 0, 0xff]);
        reached_rx.await.unwrap();
        cancel.cancel();
        task.await.unwrap();
        let mut byte = [0; 1];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), client.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        server.await.unwrap();
    }
    #[tokio::test]
    async fn sse_chunk_arrives_before_upstream_finishes_and_redirect_stays_in_epoch() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = upstream.accept().await.unwrap();
            header(&mut socket).await;
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: first\n\n").await.unwrap();
            finish_rx.await.unwrap();
            socket.write_all(b"data: last\n\n").await.unwrap();
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cancel = CancellationToken::new();
        let task = spawn(
            listener,
            Route {
                run_id: "sse".into(),
                root: PathBuf::new(),
                entry: String::new(),
                entry_service: Some("api".into()),
                services: BTreeMap::from([("api".into(), upstream_port)]),
                port,
                capabilities: vec![],
                cancel: cancel.clone(),
            },
        );
        let mut client = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        client
            .write_all(
                format!("GET /__run/sse/events HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        header(&mut client).await;
        let mut first = [0; 13];
        tokio::time::timeout(Duration::from_secs(2), client.read_exact(&mut first))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&first, b"data: first\n\n");
        finish_tx.send(()).unwrap();
        let mut rest = Vec::new();
        client.read_to_end(&mut rest).await.unwrap();
        assert_eq!(rest, b"data: last\n\n");
        server.await.unwrap();
        cancel.cancel();
        task.await.unwrap();
        assert_eq!(
            rewrite_location("/login?next=home", "/__run/x/__service/api/", 32100),
            "/__run/x/__service/api/login?next=home"
        );
        assert_eq!(
            rewrite_location("http://127.0.0.1:32100/login", "/__run/x/", 32100),
            "/__run/x/login"
        );
        assert_eq!(
            rewrite_location("https://external.example/login", "/__run/x/", 32100),
            "https://external.example/login"
        );
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;
    #[tokio::test]
    async fn rejects_cross_origin_and_ambiguous_body_and_serves_host_csp() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("index.html"), "safe").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cancel = CancellationToken::new();
        let task = spawn(
            listener,
            Route {
                run_id: "scope".into(),
                root: root.path().into(),
                entry: "index.html".into(),
                entry_service: None,
                services: BTreeMap::new(),
                port,
                capabilities: vec![],
                cancel: cancel.clone(),
            },
        );
        for (headers, code) in [
            ("Origin: https://evil.example\r\n", 403),
            ("Content-Length: 2\r\nTransfer-Encoding: chunked\r\n", 400),
            ("Content-Length: 1\r\nContent-Length: 1\r\n", 400),
            ("Transfer-Encoding: gzip\r\n", 400),
            ("", 200),
        ] {
            let mut socket = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
                .await
                .unwrap();
            socket
                .write_all(
                    format!(
                        "GET /__run/scope/ HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{headers}\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let mut response = Vec::new();
            socket.read_to_end(&mut response).await.unwrap();
            let response = String::from_utf8(response).unwrap();
            assert!(response.starts_with(&format!("HTTP/1.1 {code}")));
            if code == 200 {
                assert!(response.contains("Content-Security-Policy:"));
                assert!(response.contains("worker-src 'none'"));
                assert!(response.contains("frame-ancestors 'none'"));
                assert!(response.contains("Permissions-Policy: camera=(), microphone=()"));
                assert!(!response.contains(" https:"));
            }
        }
        cancel.cancel();
        task.await.unwrap();
    }
}

#[cfg(test)]
mod absolute_path_tests {
    use super::*;
    #[tokio::test]
    async fn root_absolute_urls_redirect_only_from_current_epoch_and_keep_post_semantics() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("dist")).unwrap();
        std::fs::write(root.path().join("dist/index.html"), "fixture").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cancel = CancellationToken::new();
        let task = spawn(
            listener,
            Route {
                run_id: "current".into(),
                root: root.path().into(),
                entry: "dist/index.html".into(),
                entry_service: None,
                services: BTreeMap::new(),
                port,
                capabilities: vec![],
                cancel: cancel.clone(),
            },
        );
        for (referrer, code) in [
            (
                format!("http://127.0.0.1:{port}/__run/current/dist/index.html"),
                307,
            ),
            (
                format!("http://127.0.0.1:{port}/__run/old/dist/index.html"),
                410,
            ),
            (String::new(), 410),
        ] {
            let mut socket = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
                .await
                .unwrap();
            socket.write_all(format!("POST /assets/app.js?q=1 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nReferer: {referrer}\r\nContent-Length: 0\r\n\r\n").as_bytes()).await.unwrap();
            let mut response = Vec::new();
            socket.read_to_end(&mut response).await.unwrap();
            let response = String::from_utf8(response).unwrap();
            assert!(response.starts_with(&format!("HTTP/1.1 {code}")));
            if code == 307 {
                assert!(response.contains("Location: /__run/current/dist/assets/app.js?q=1"));
            }
        }
        cancel.cancel();
        task.await.unwrap();
    }
}
