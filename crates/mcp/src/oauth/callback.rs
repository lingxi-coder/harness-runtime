//! Loopback HTTP listener for the OAuth redirect (MCP remote auth).
//!
//! Binds a TCP listener on `127.0.0.1:{port}`, accepts connections until a
//! `GET /callback?code=...&state=...` arrives (replying `404` to any other
//! path, e.g. a favicon probe), URL-decodes and validates the `state` against
//! the locally-generated CSRF token, writes a small success page to the
//! browser, and returns the parsed params.
//!
//! Passing `port == 0` binds an OS-assigned ephemeral port; the caller reads
//! the chosen port back via [`CallbackListener::port`] and bakes it into the
//! `redirect_uri` before opening the browser (claude-code's `listen(0)`
//! pattern). Ported from `anthropic-oauth::callback`.

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
    /// The `IdP` redirected with an `error` query parameter (the user denied,
    /// the request was invalid, etc.). Mirrors claude-code's `G(Error(...))`
    /// path, which serves a styled error page AND aborts the flow.
    #[error("{0}")]
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

impl CallbackListener {
    /// Bind `127.0.0.1:{port}`. `port == 0` selects an OS-assigned ephemeral
    /// port readable via [`Self::port`].
    ///
    /// # Errors
    /// [`CallbackError::Bind`] if the socket cannot be bound.
    pub async fn bind(port: u16) -> Result<Self, CallbackError> {
        let addr: SocketAddr = ([127, 0, 0, 1], port).into();
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| CallbackError::Bind(e.to_string()))?;
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
    /// `redirect_uri` is the loopback URL the caller baked into the authorize
    /// request; it is echoed into the `404` body (claude-code's
    /// "the registered redirect_uri must be {T}" copy).
    ///
    /// # Errors
    /// [`CallbackError::StateMismatch`] on a CSRF mismatch; other variants on a
    /// malformed request or socket failure.
    pub async fn accept(
        self,
        expected_state: &str,
        redirect_uri: &str,
    ) -> Result<CallbackParams, CallbackError> {
        loop {
            let (stream, _) = self
                .listener
                .accept()
                .await
                .map_err(|e| CallbackError::Bind(e.to_string()))?;
            match handle_connection(stream, expected_state, redirect_uri).await {
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
///
/// Branch ordering mirrors claude-code's `/callback` handler (binary offset
/// 208249900): state-mismatch (400) → `error` param (200) → `code` (200);
/// any non-`/callback` path → 404. On state-mismatch and on an `error` param,
/// the styled page is served first AND then the flow is aborted with an error.
async fn handle_connection(
    mut stream: TcpStream,
    expected_state: &str,
    redirect_uri: &str,
) -> Result<Option<CallbackParams>, CallbackError> {
    let mut buf = vec![0u8; 4096];
    let n = stream
        .read(&mut buf)
        .await
        .map_err(|e| CallbackError::InvalidRequest(e.to_string()))?;
    let req = String::from_utf8_lossy(&buf[..n]);

    let line = req
        .lines()
        .next()
        .ok_or_else(|| CallbackError::InvalidRequest("empty request".into()))?;
    let path = line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| CallbackError::InvalidRequest("no path".into()))?;
    let (route, query) = path.split_once('?').unwrap_or((path, ""));

    // Only the OAuth redirect path is interesting; everything else (favicon,
    // probes) gets a 404 and is skipped without aborting the flow.
    if route != "/callback" {
        let body = render_page(
            false,
            "Not found",
            &format!(
                "This is the LingXi MCP OAuth callback listener. It only handles /callback. \
If your OAuth provider redirected here, the registered redirect_uri must be {redirect_uri}."
            ),
            None,
        );
        let _ = write_response(&mut stream, "404 Not Found", &body).await;
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

    // (1) state-check first (claude `state !== expectedState`): serve a 400 page
    //     then abort. claude treats a missing `state` as a mismatch too.
    let state = state.unwrap_or_default();
    if state != expected_state {
        let body = render_page(
            false,
            "Authentication failed",
            "Invalid state parameter. Close this tab and try again from LingXi.",
            None,
        );
        // Serve the page BEFORE returning so the browser shows it.
        let _ = write_response(&mut stream, "400 Bad Request", &body).await;
        return Err(CallbackError::StateMismatch);
    }

    // (2) provider `error` param: serve a 200 page with the error detail, abort.
    if let Some(err) = error {
        let detail = format!("{}: {}", err, error_description.unwrap_or_default());
        let body = render_page(
            false,
            "Authentication failed",
            "Close this tab and try again from LingXi.",
            Some(&detail),
        );
        let _ = write_response(&mut stream, "200 OK", &body).await;
        return Err(CallbackError::Provider(detail));
    }

    // (3) success: a `code` is present → serve the success page and return.
    let code = code.ok_or_else(|| CallbackError::InvalidRequest("missing code".into()))?;
    let body = render_page(
        true,
        "Authentication successful",
        "You can close this tab and return to LingXi.",
        None,
    );
    let _ = write_response(&mut stream, "200 OK", &body).await;

    Ok(Some(CallbackParams { code, state }))
}

/// HTML-escape per claude-code's `DGr`/`xId` (binary offsets 204622512 /
/// 204624534): a single left-to-right pass mapping `& < > " '` to their
/// entities. Equivalent to the `/[&<>"']/g` regex pass (`&` is in the map, so
/// no double-escaping). All other characters pass through verbatim.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Inline CSS served in the callback page `<style>` (claude-code's `kId`,
/// binary offset 204623131). Verbatim including the 12 embedded `\n` rule
/// separators; NO trailing newline (ends with the `@media` close braces).
/// Only the LingXi rebrand carve-out applies elsewhere — the CSS itself
/// (hex colors, fonts) is byte-exact to claude.
const CALLBACK_CSS: &str = "*,*::before,*::after{box-sizing:border-box}\nhtml,body{margin:0;padding:0}\nbody{min-height:100vh;background:#FAF9F5;color:#141413;font:15px/1.5 ui-sans-serif,-apple-system,BlinkMacSystemFont,\"Segoe UI\",Helvetica,Arial,sans-serif;-webkit-font-smoothing:antialiased;display:flex;align-items:center;justify-content:center;padding:48px 24px}\nmain{width:100%;max-width:560px}\n.status{display:inline-flex;align-items:center;gap:8px;padding:4px 10px 4px 8px;border-radius:999px;background:rgba(85,138,66,.10);color:#345C28;font-size:12.5px;font-weight:500;letter-spacing:-.005em;margin-bottom:20px}\n.status::before{content:\"\";width:6px;height:6px;border-radius:50%;background:#558A42;box-shadow:0 0 0 3px rgba(85,138,66,.18)}\n.status.err{background:rgba(166,50,68,.08);color:#671D28}\n.status.err::before{background:#A63244;box-shadow:0 0 0 3px rgba(166,50,68,.15)}\nh1{font-family:ui-serif,Charter,\"Iowan Old Style\",Georgia,serif;font-weight:400;font-size:32px;line-height:1.15;letter-spacing:-.02em;margin:0 0 10px;text-wrap:balance}\n.sub{margin:0;color:#4D4C48;font-size:15px;line-height:1.55;max-width:52ch}\n.detail{margin-top:20px;background:#FFF;border:.5px solid rgba(31,30,29,.15);border-left:3px solid #A63244;border-radius:10px;padding:14px 16px;font-size:14px;line-height:1.5;color:#3D3D3A;word-break:break-word}\n@media (max-width:520px){h1{font-size:26px}body{padding:32px 18px}}";

/// Render the callback HTML page (claude-code's `Jle`, binary offset 204622572).
///
/// One-line template (the only literal `\n`s live inside [`CALLBACK_CSS`]):
/// `heading`/`message`/`detail` are HTML-escaped via [`esc`]; the success page
/// (`ok == true`) carries a `window.close()` auto-close script and the
/// `Connected` status pill, the error page the `Error` pill. Brand strings are
/// the LingXi rebrand carve-out; everything else (DOCTYPE, status words, escape
/// entities, CSS) is byte-exact to claude.
fn render_page(ok: bool, heading: &str, message: &str, detail: Option<&str>) -> String {
    let status = if ok {
        "<span class=\"status\">Connected</span>"
    } else {
        "<span class=\"status err\">Error</span>"
    };
    let detail_div = match detail {
        Some(d) => format!("<div class=\"detail\">{}</div>", esc(d)),
        None => String::new(),
    };
    let close_script = if ok {
        "<script>setTimeout(function(){try{window.close()}catch(e){}},1500)</script>"
    } else {
        ""
    };
    format!(
        "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>LingXi</title><style>{css}</style></head><body><main>{status}<h1>{heading}</h1><p class=\"sub\">{message}</p>{detail_div}</main>{close_script}</body></html>",
        css = CALLBACK_CSS,
        status = status,
        heading = esc(heading),
        message = esc(message),
        detail_div = detail_div,
        close_script = close_script,
    )
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
    // claude-code serves the page via Node `res.writeHead(_, {"Content-Type":
    // "text/html"})` + `res.end(body)` (bare `text/html`, no charset; Node
    // auto-fills Content-Length). The port frames raw HTTP/1.1 itself; byte
    // parity is asserted on the BODY, which is what the browser renders.
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    );
    stream.write_all(resp.as_bytes()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpStream;

    async fn send_get(port: u16, path_and_query: &str) -> String {
        let mut client = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        let req = format!("GET {path_and_query} HTTP/1.1\r\nHost: localhost\r\n\r\n");
        client.write_all(req.as_bytes()).await.expect("write");
        let mut resp = Vec::new();
        let _ = client.read_to_end(&mut resp).await;
        String::from_utf8_lossy(&resp).into_owned()
    }

    /// Extract just the HTTP body (after the `\r\n\r\n` header terminator).
    fn body_of(resp: &str) -> &str {
        resp.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("")
    }

    const RU: &str = "http://localhost:5000/callback";

    #[tokio::test]
    async fn accept_is_interruptible_by_a_timeout() {
        // The connect path (registry #18) bounds this wait so a never-completed
        // browser OAuth cannot hold the per-server lifecycle lock forever.
        // `accept()` must therefore be a well-behaved cancellable future: with no
        // callback delivered, a timeout fires (and drops the accept) instead of
        // hanging.
        let listener = CallbackListener::bind(0).await.expect("bind");
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            listener.accept("S", RU),
        )
        .await;
        assert!(
            result.is_err(),
            "accept must be interruptible by the surrounding timeout"
        );
    }

    #[tokio::test]
    async fn returns_params_on_valid_callback() {
        let listener = CallbackListener::bind(0).await.expect("bind");
        let port = listener.port();
        assert_ne!(port, 0, "ephemeral port resolved");

        let server = tokio::spawn(async move { listener.accept("S", RU).await });
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
        assert!(resp.contains("Content-Type: text/html\r\n"));
        // Success page: success pill, heading/message, auto-close script.
        let body = body_of(&resp);
        assert!(body.contains("<span class=\"status\">Connected</span>"));
        assert!(body.contains("<h1>Authentication successful</h1>"));
        assert!(body.contains("You can close this tab and return to LingXi."));
        assert!(body.contains("window.close()"));
    }

    #[tokio::test]
    async fn state_mismatch_is_rejected() {
        let listener = CallbackListener::bind(0).await.expect("bind");
        let port = listener.port();
        let server = tokio::spawn(async move { listener.accept("EXPECTED", RU).await });
        let resp = send_get(port, "/callback?code=abc&state=WRONG").await;
        let result = server.await.expect("join");
        assert!(matches!(result, Err(CallbackError::StateMismatch)));
        // claude serves a 400 page even though the flow aborts.
        assert!(resp.contains("400 Bad Request"));
        assert!(body_of(&resp)
            .contains("Invalid state parameter. Close this tab and try again from LingXi."));
    }

    #[tokio::test]
    async fn provider_error_is_served_and_aborts() {
        let listener = CallbackListener::bind(0).await.expect("bind");
        let port = listener.port();
        let server = tokio::spawn(async move { listener.accept("S", RU).await });
        let resp = send_get(
            port,
            "/callback?error=access_denied&error_description=user%20said%20no&state=S",
        )
        .await;
        let result = server.await.expect("join");
        assert!(matches!(result, Err(CallbackError::Provider(_))));
        assert!(resp.contains("200 OK"));
        let body = body_of(&resp);
        assert!(body.contains("<h1>Authentication failed</h1>"));
        assert!(body.contains("Close this tab and try again from LingXi."));
        assert!(body.contains("<div class=\"detail\">access_denied: user said no</div>"));
    }

    #[tokio::test]
    async fn url_encoded_values_are_decoded() {
        let listener = CallbackListener::bind(0).await.expect("bind");
        let port = listener.port();
        let server = tokio::spawn(async move { listener.accept("ST", RU).await });
        let _ = send_get(port, "/callback?code=a%2Fb&state=ST").await;
        let params = server.await.expect("join").expect("ok");
        assert_eq!(params.code, "a/b");
    }

    #[tokio::test]
    async fn non_callback_path_is_404_then_callback_succeeds() {
        let listener = CallbackListener::bind(0).await.expect("bind");
        let port = listener.port();
        let server = tokio::spawn(async move { listener.accept("S", RU).await });

        let favicon_resp = send_get(port, "/favicon.ico").await;
        assert!(favicon_resp.contains("404"), "favicon resp: {favicon_resp}");
        let body = body_of(&favicon_resp);
        assert!(body.contains("<h1>Not found</h1>"));
        assert!(body.contains(
            "This is the LingXi MCP OAuth callback listener. It only handles /callback. \
If your OAuth provider redirected here, the registered redirect_uri must be \
http://localhost:5000/callback."
        ));

        let _ = send_get(port, "/callback?code=zzz&state=S").await;
        let params = server.await.expect("join").expect("ok");
        assert_eq!(params.code, "zzz");
    }

    // -- Byte-exact page parity (claude-code `Jle`/`kId`/`DGr`, 2.1.195). ------

    /// The full success-page body, byte-for-byte, matches claude-code's `Jle`
    /// output (with the LingXi rebrand carve-out for the 3 brand strings +
    /// title). Verifies the one-line template, the 12-newline inline CSS, and
    /// the success-only auto-close script.
    #[test]
    fn success_page_is_byte_exact() {
        let body = render_page(
            true,
            "Authentication successful",
            "You can close this tab and return to LingXi.",
            None,
        );
        let expected = concat!(
            "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">",
            "<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">",
            "<title>LingXi</title><style>",
            "*,*::before,*::after{box-sizing:border-box}\n",
            "html,body{margin:0;padding:0}\n",
            "body{min-height:100vh;background:#FAF9F5;color:#141413;font:15px/1.5 ui-sans-serif,-apple-system,BlinkMacSystemFont,\"Segoe UI\",Helvetica,Arial,sans-serif;-webkit-font-smoothing:antialiased;display:flex;align-items:center;justify-content:center;padding:48px 24px}\n",
            "main{width:100%;max-width:560px}\n",
            ".status{display:inline-flex;align-items:center;gap:8px;padding:4px 10px 4px 8px;border-radius:999px;background:rgba(85,138,66,.10);color:#345C28;font-size:12.5px;font-weight:500;letter-spacing:-.005em;margin-bottom:20px}\n",
            ".status::before{content:\"\";width:6px;height:6px;border-radius:50%;background:#558A42;box-shadow:0 0 0 3px rgba(85,138,66,.18)}\n",
            ".status.err{background:rgba(166,50,68,.08);color:#671D28}\n",
            ".status.err::before{background:#A63244;box-shadow:0 0 0 3px rgba(166,50,68,.15)}\n",
            "h1{font-family:ui-serif,Charter,\"Iowan Old Style\",Georgia,serif;font-weight:400;font-size:32px;line-height:1.15;letter-spacing:-.02em;margin:0 0 10px;text-wrap:balance}\n",
            ".sub{margin:0;color:#4D4C48;font-size:15px;line-height:1.55;max-width:52ch}\n",
            ".detail{margin-top:20px;background:#FFF;border:.5px solid rgba(31,30,29,.15);border-left:3px solid #A63244;border-radius:10px;padding:14px 16px;font-size:14px;line-height:1.5;color:#3D3D3A;word-break:break-word}\n",
            "@media (max-width:520px){h1{font-size:26px}body{padding:32px 18px}}",
            "</style></head><body><main>",
            "<span class=\"status\">Connected</span>",
            "<h1>Authentication successful</h1>",
            "<p class=\"sub\">You can close this tab and return to LingXi.</p>",
            "</main>",
            "<script>setTimeout(function(){try{window.close()}catch(e){}},1500)</script>",
            "</body></html>",
        );
        assert_eq!(body, expected);
    }

    /// The error page (`ok == false`) uses the `err` status pill, NO auto-close
    /// script, and renders the escaped `detail` div.
    #[test]
    fn error_page_pill_no_script_and_escapes_detail() {
        let body = render_page(
            false,
            "Authentication failed",
            "Close this tab and try again from LingXi.",
            Some("bad <tag> & \"quote\" 'apos'"),
        );
        assert!(body.contains("<span class=\"status err\">Error</span>"));
        assert!(
            !body.contains("window.close()"),
            "error page has no auto-close"
        );
        assert!(body.contains(
            "<div class=\"detail\">bad &lt;tag&gt; &amp; &quot;quote&quot; &#39;apos&#39;</div>"
        ));
    }

    /// `esc` is a single left-to-right pass; `&` does not double-escape later
    /// entities (matches claude's `/[&<>"']/g` regex).
    #[test]
    fn esc_single_pass_no_double_escape() {
        assert_eq!(esc("&<>\"'"), "&amp;&lt;&gt;&quot;&#39;");
        assert_eq!(esc("a&b"), "a&amp;b");
        // An ampersand followed by `lt;` must NOT become `&amp;lt;`-then-escaped
        // again; it is escaped exactly once.
        assert_eq!(esc("&lt;"), "&amp;lt;");
        assert_eq!(esc("plain text"), "plain text");
    }
}
