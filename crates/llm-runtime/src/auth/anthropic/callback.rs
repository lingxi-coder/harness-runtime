//! Loopback HTTP listener for the OAuth redirect.
//!
//! See spec §30.3 / A5. Binds a TCP listener on `127.0.0.1:{port}`, accepts
//! connections until a `GET /callback?code=...&state=...` arrives (replying
//! `404` to any other path, e.g. a favicon probe), URL-decodes and validates
//! the `state` against the locally-generated CSRF token, writes a small HTML
//! success page to the browser, and returns the parsed params.
//!
//! Passing `port == 0` binds an OS-assigned ephemeral port; the caller reads
//! the chosen port back via [`CallbackListener::port`] and bakes it into the
//! `redirect_uri` before opening the browser (claude-code's `listen(0)`
//! pattern).

use std::net::SocketAddr;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Failures during the loopback callback handshake.
#[derive(Debug, Clone, Error)]
pub enum CallbackError {
    /// `bind` / `accept` failed at the socket layer.
    #[error("bind failed: {0}")]
    Bind(String),
    /// Request was malformed or missing required parameters.
    #[error("invalid callback request: {0}")]
    InvalidRequest(String),
    /// `state` returned by the `IdP` didn't match the local CSRF token.
    #[error("state mismatch")]
    StateMismatch,
    /// The provider redirected with `error=` instead of a code — most often
    /// the user pressed Deny. Reporting this as "missing code" hid the real
    /// reason. Shape mirrors `mcp::oauth::callback::CallbackError::Provider`.
    #[error("authorization denied: {0}")]
    Provider(String),
}

/// Parameters parsed out of a `GET /callback?code=...&state=...` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallbackParams {
    /// Authorization code to exchange for tokens.
    pub code: String,
    /// CSRF state echoed back by the `IdP`.
    pub state: String,
}

/// A bound loopback listener whose port is known before any browser hits it.
///
/// Construct with [`CallbackListener::bind`] (which accepts `port == 0` for an
/// OS-assigned port), read [`CallbackListener::port`] into the `redirect_uri`,
/// then drive [`CallbackListener::accept`] to wait for the redirect.
pub struct CallbackListener {
    listener: TcpListener,
    port: u16,
}

/// Bind a loopback listener with `SO_REUSEADDR` set. See the note in
/// [`CallbackListener::bind`].
fn bind_reusable(addr: SocketAddr) -> std::io::Result<TcpListener> {
    let socket = tokio::net::TcpSocket::new_v4()?;
    socket.set_reuseaddr(true)?;
    socket.bind(addr)?;
    socket.listen(1024)
}

impl CallbackListener {
    /// Bind `127.0.0.1:{port}`. `port == 0` selects an OS-assigned ephemeral
    /// port readable via [`Self::port`].
    ///
    /// # Errors
    /// [`CallbackError::Bind`] if the socket cannot be bound.
    pub async fn bind(port: u16) -> Result<Self, CallbackError> {
        let addr: SocketAddr = ([127, 0, 0, 1], port).into();
        // SO_REUSEADDR: this server closes the connection first, so its accepted
        // socket lingers in TIME_WAIT holding the fixed callback port. Without
        // it, a user who cancels an OAuth login and retries inside that window
        // (60s on macOS) gets AddrInUse. Reusing a TIME_WAIT address is all this
        // permits — two live listeners on one port would need SO_REUSEPORT, so a
        // real conflict is still reported. Harmless when `port == 0`.
        let listener = bind_reusable(addr).map_err(|e| CallbackError::Bind(e.to_string()))?;
        let port = listener
            .local_addr()
            .map_err(|e| CallbackError::Bind(e.to_string()))?
            .port();
        Ok(Self { listener, port })
    }

    /// The actual bound port (resolved even when `bind(0)` was used).
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Accept connections until a `GET /callback` request arrives, validate the
    /// `state`, reply with a success page, and return the params. Connections
    /// to any other path receive a `404` and are skipped.
    ///
    /// # Errors
    /// [`CallbackError::StateMismatch`] on a CSRF mismatch; other variants on a
    /// malformed request or socket failure.
    pub async fn accept(self, expected_state: &str) -> Result<CallbackParams, CallbackError> {
        loop {
            let (stream, _) = self
                .listener
                .accept()
                .await
                .map_err(|e| CallbackError::Bind(e.to_string()))?;
            match handle_connection(stream, expected_state).await {
                Ok(Some(params)) => return Ok(params),
                // Non-/callback path (e.g. favicon) — already 404'd; keep waiting.
                Ok(None) => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

/// Read one request off `stream`. Returns `Ok(Some(params))` for a valid
/// `/callback`, `Ok(None)` for any other path (after replying 404), or an error.
async fn handle_connection(
    mut stream: TcpStream,
    expected_state: &str,
) -> Result<Option<CallbackParams>, CallbackError> {
    let Some(req) = read_request_head(&mut stream).await else {
        // A connection that carried no readable request head is not a failed
        // login. Safari opens speculative connections to the redirect host and
        // drops them; aborting `accept()` on one of those killed the flow
        // before the real redirect ever arrived.
        return Ok(None);
    };

    let Some(path) = req
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
    else {
        let _ = write_response(&mut stream, "400 Bad Request", "Malformed request.").await;
        return Ok(None);
    };
    let (route, query) = path.split_once('?').unwrap_or((path, ""));

    // Only the OAuth redirect path is interesting; everything else (favicon,
    // probes) gets a 404 and is skipped without aborting the flow.
    if route != "/callback" {
        let _ = write_response(&mut stream, "404 Not Found", "Not found.").await;
        return Ok(None);
    }

    let mut code = None;
    let mut state = None;
    let mut error = None;
    let mut error_description = None;
    for kv in query.split('&') {
        if let Some(v) = kv.strip_prefix("code=") {
            code = Some(url_decode(v));
        } else if let Some(v) = kv.strip_prefix("state=") {
            state = Some(url_decode(v));
        } else if let Some(v) = kv.strip_prefix("error=") {
            error = Some(url_decode(v));
        } else if let Some(v) = kv.strip_prefix("error_description=") {
            error_description = Some(url_decode(v));
        }
    }

    // Order matters, and mirrors `mcp::oauth::callback`: CSRF first, then the
    // provider's own error, then the code. Checking `code` first — as this did
    // — reported "missing code" for a user who pressed Deny, and masked a state
    // mismatch behind the same message. A missing `state` is a mismatch.
    let state = state.unwrap_or_default();
    if state != expected_state {
        let _ = write_response(&mut stream, "400 Bad Request", "Invalid state parameter.").await;
        return Err(CallbackError::StateMismatch);
    }
    if let Some(error) = error {
        let detail = format!("{error}: {}", error_description.unwrap_or_default());
        let _ = write_response(&mut stream, "200 OK", "Authorization was denied.").await;
        return Err(CallbackError::Provider(detail));
    }
    let code = code.ok_or_else(|| CallbackError::InvalidRequest("missing code".into()))?;

    let _ = write_response(
        &mut stream,
        "200 OK",
        "Login complete. You can close this window.",
    )
    .await;

    Ok(Some(CallbackParams { code, state }))
}

/// Read one HTTP request head (through the blank line) off `stream`.
///
/// Returns `None` when the peer sent nothing, the head never terminated within
/// `MAX_REQUEST_HEAD`, or nothing arrived within `READ_TIMEOUT` — each of which
/// means "ignore this connection", never "fail the login".
///
/// The previous single 4 KiB `read` also truncated any request split across TCP
/// segments, which a long `code` plus browser headers can trigger.
async fn read_request_head(stream: &mut TcpStream) -> Option<String> {
    const MAX_REQUEST_HEAD: usize = 8 * 1024;
    const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    let read = async {
        let mut head: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                break;
            }
            head.extend_from_slice(&chunk[..n]);
            if head.windows(4).any(|w| w == b"\r\n\r\n") || head.len() >= MAX_REQUEST_HEAD {
                break;
            }
        }
        (!head.is_empty()).then(|| String::from_utf8_lossy(&head).into_owned())
    };
    tokio::time::timeout(READ_TIMEOUT, read)
        .await
        .ok()
        .flatten()
}

/// Best-effort `percent`-decode. Base64url callback values contain no `%`, but
/// some providers escape them — round-trip them faithfully. On a malformed
/// escape we keep the raw substring rather than dropping data.
fn url_decode(raw: &str) -> String {
    match urlencoding::decode(raw) {
        Ok(decoded) => decoded.into_owned(),
        Err(_) => raw.to_string(),
    }
}

async fn write_response(
    stream: &mut TcpStream,
    status: &str,
    body: &str,
) -> Result<(), std::io::Error> {
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    );
    stream.write_all(resp.as_bytes()).await
}

/// Bind on `127.0.0.1:port`, wait for a single GET to `/callback?code=...&state=...`,
/// validate the state token, write a small success page, and return the params.
///
/// Thin wrapper over [`CallbackListener`] preserved for callers that pick the
/// port themselves (the port must already be bound into the `redirect_uri`).
///
/// # Errors
/// See [`CallbackError`].
pub async fn await_callback(
    port: u16,
    expected_state: &str,
) -> Result<CallbackParams, CallbackError> {
    let listener = CallbackListener::bind(port).await?;
    listener.accept(expected_state).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpStream;
    use tokio::time::{sleep, Duration};

    async fn try_send_get(port: u16, path_and_query: &str) -> std::io::Result<String> {
        let mut client = TcpStream::connect(("127.0.0.1", port)).await?;
        let req = format!("GET {path_and_query} HTTP/1.1\r\nHost: localhost\r\n\r\n");
        client.write_all(req.as_bytes()).await?;
        let mut resp = Vec::new();
        // The server closes the connection after writing; read to EOF.
        let _ = client.read_to_end(&mut resp).await;
        Ok(String::from_utf8_lossy(&resp).into_owned())
    }

    async fn send_get(port: u16, path_and_query: &str) -> String {
        try_send_get(port, path_and_query).await.expect("send GET")
    }

    #[tokio::test]
    async fn returns_params_on_valid_callback() {
        let listener = CallbackListener::bind(0).await.expect("bind");
        let port = listener.port();
        assert_ne!(port, 0, "ephemeral port resolved");

        let server = tokio::spawn(async move { listener.accept("S").await });
        let resp = send_get(port, "/callback?code=abc&state=S").await;

        let params = server.await.expect("join").expect("accept ok");
        assert_eq!(
            params,
            CallbackParams {
                code: "abc".into(),
                state: "S".into()
            }
        );
        assert!(resp.contains("200 OK"));
        assert!(resp.contains("Login complete"));
    }

    /// Safari opens speculative connections to the redirect host and drops
    /// them without sending a byte. That used to surface as
    /// `InvalidRequest("empty request")` out of `accept()` and killed the
    /// login before the real redirect arrived.
    #[tokio::test]
    async fn a_zero_byte_connection_does_not_abort_the_flow() {
        let listener = CallbackListener::bind(0).await.expect("bind");
        let port = listener.port();
        let server = tokio::spawn(async move { listener.accept("S").await });

        // Connect and close without writing anything.
        {
            let probe = TcpStream::connect(("127.0.0.1", port))
                .await
                .expect("probe");
            drop(probe);
        }
        sleep(Duration::from_millis(20)).await;

        // The real redirect still completes.
        let _ = send_get(port, "/callback?code=abc&state=S").await;
        let params = server
            .await
            .expect("join")
            .expect("accept survived the probe");
        assert_eq!(params.code, "abc");
    }

    /// A user pressing Deny redirects with `error=`, no `code`. Reporting
    /// "missing code" for that hid the actual reason.
    #[tokio::test]
    async fn a_provider_error_redirect_is_surfaced_as_provider() {
        let listener = CallbackListener::bind(0).await.expect("bind");
        let port = listener.port();
        let server = tokio::spawn(async move { listener.accept("S").await });
        let _ = send_get(
            port,
            "/callback?error=access_denied&error_description=User%20denied&state=S",
        )
        .await;
        match server.await.expect("join") {
            Err(CallbackError::Provider(detail)) => {
                assert!(detail.contains("access_denied"), "detail: {detail}");
                assert!(detail.contains("User denied"), "detail: {detail}");
            }
            other => panic!("expected Provider, got {other:?}"),
        }
    }

    /// State is validated before `code`, so a CSRF mismatch is never masked as
    /// a missing parameter. A missing `state` counts as a mismatch.
    #[tokio::test]
    async fn state_is_checked_before_code() {
        let listener = CallbackListener::bind(0).await.expect("bind");
        let port = listener.port();
        let server = tokio::spawn(async move { listener.accept("EXPECTED").await });
        let _ = send_get(port, "/callback?state=WRONG").await;
        assert!(matches!(
            server.await.expect("join"),
            Err(CallbackError::StateMismatch)
        ));
    }

    /// A single 4 KiB `read` truncated any request split across TCP segments.
    #[tokio::test]
    async fn a_request_split_across_two_writes_is_parsed() {
        let listener = CallbackListener::bind(0).await.expect("bind");
        let port = listener.port();
        let server = tokio::spawn(async move { listener.accept("S").await });

        let mut client = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        client
            .write_all(b"GET /callback?code=split&state=S HTTP/1.1\r\n")
            .await
            .expect("write line");
        sleep(Duration::from_millis(30)).await;
        client
            .write_all(b"Host: localhost\r\nUser-Agent: probe\r\n\r\n")
            .await
            .expect("write headers");
        let mut resp = Vec::new();
        let _ = client.read_to_end(&mut resp).await;

        let params = server.await.expect("join").expect("accept ok");
        assert_eq!(params.code, "split");
    }

    #[tokio::test]
    async fn state_mismatch_is_rejected() {
        let listener = CallbackListener::bind(0).await.expect("bind");
        let port = listener.port();
        let server = tokio::spawn(async move { listener.accept("EXPECTED").await });
        let _ = send_get(port, "/callback?code=abc&state=WRONG").await;
        let result = server.await.expect("join");
        assert!(matches!(result, Err(CallbackError::StateMismatch)));
    }

    #[tokio::test]
    async fn url_encoded_values_are_decoded() {
        let listener = CallbackListener::bind(0).await.expect("bind");
        let port = listener.port();
        // code contains a percent-escaped slash; state is plain.
        let server = tokio::spawn(async move { listener.accept("ST").await });
        let _ = send_get(port, "/callback?code=a%2Fb&state=ST").await;
        let params = server.await.expect("join").expect("ok");
        assert_eq!(params.code, "a/b");
    }

    #[tokio::test]
    async fn non_callback_path_is_404_then_callback_succeeds() {
        let listener = CallbackListener::bind(0).await.expect("bind");
        let port = listener.port();
        let server = tokio::spawn(async move { listener.accept("S").await });

        // A favicon probe should be 404'd and skipped.
        let favicon_resp = send_get(port, "/favicon.ico").await;
        assert!(favicon_resp.contains("404"), "favicon resp: {favicon_resp}");

        // The real callback then arrives and resolves the accept().
        let _ = send_get(port, "/callback?code=zzz&state=S").await;
        let params = server.await.expect("join").expect("ok");
        assert_eq!(params.code, "zzz");
    }

    #[tokio::test]
    async fn await_callback_wrapper_binds_explicit_port() {
        let base = 41000 + (std::process::id() % 10_000) as u16;
        for offset in 0..64u16 {
            let port = base.saturating_add(offset);
            let server = tokio::spawn(async move { await_callback(port, "S").await });
            for _ in 0..50 {
                if server.is_finished() {
                    break;
                }
                if try_send_get(port, "/callback?code=c&state=S").await.is_ok() {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }

            match server.await.expect("join") {
                Ok(params) => {
                    assert_eq!(params.code, "c");
                    return;
                }
                Err(CallbackError::Bind(_)) => continue,
                Err(other) => panic!("unexpected callback error: {other}"),
            }
        }
        panic!("could not bind any explicit callback test port");
    }
}
