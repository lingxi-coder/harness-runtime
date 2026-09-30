//! The unified hyper 1.x forward proxy (`http-proxy.js`): ONE server handling
//! BOTH plain-HTTP absolute-form forwarding (http-proxy.js:147-258) and
//! `CONNECT` tunnelling (http-proxy.js:12-145), with parent-proxy routing and
//! the per-request [`request_filter`] body hook.
//!
//! SECURITY-CRITICAL: this enforces the network allowlist for both HTTP and
//! HTTPS. Both denial paths emit the byte-exact 403 (`X-Proxy-Error:
//! blocked-by-allowlist`, body `Connection blocked by network allowlist`).
//!
//! Architecture: Node uses one `http.Server` listening for both `connect` and
//! `request` events. The faithful Rust port is one `hyper`
//! `http1::Builder::serve_connection(..).with_upgrades()` over a service that
//! branches on `Method::CONNECT`. CONNECT replies `200 Connection Established`,
//! then `hyper::upgrade::on` yields the raw client stream which is spliced to
//! the upstream with `tokio::io::copy_bidirectional`. Plain HTTP reconstructs
//! the absolute URI from parsed components (closing URL-parser differential
//! bypasses) and forwards via a per-request `hyper::client::conn::http1`
//! handshake to the origin (or to the parent proxy with an absolute-path
//! request line + `Proxy-Authorization`).

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};

use crate::config::{
    current_network_config, shared_network_config, NetworkConfig, SharedNetworkConfig,
};
use crate::dial::{dial_direct, parse_connect_target, CONNECT_TIMEOUT};
use crate::matcher::{filter_network_request_with_ask_decision, AskFn};
use crate::mitm_ca::MitmCa;
use crate::parent_proxy::{
    connect_via_parent_proxy, proxy_auth_header, select_parent_proxy_url,
    should_bypass_parent_proxy, strip_hop_by_hop, ResolvedParentProxy,
};
use crate::request_filter::{decide_and_respond, FilterOutcome, FilterRequestFn};
use crate::tls_terminate::{peek_client_hello, terminate_and_forward, TlsTarget};
use crate::violation_store::{SandboxViolationStore, Violation};

/// Options for the forward proxy server.
///
/// The in-process TLS-MITM termination lands in P6b: when [`Self::mitm_ca`] is
/// set, a `CONNECT` whose first bytes look like a TLS `ClientHello` is
/// terminated and re-issued (see [`crate::tls_terminate`]) instead of being
/// opaque-tunnelled. The remaining `getMitmSocketPath` (external MITM
/// unix-socket) seam stays marked `// P6:` for the plain-HTTP path.
#[derive(Clone)]
pub struct ProxyOptions {
    /// Live, swappable allow/deny network config (the allowlist enforced for
    /// HTTP + CONNECT). Read **per request** so a
    /// [`crate::manager::SandboxManager::update_config`] allow/deny swap takes
    /// effect on already-running connections with no rebind (the TS reads the
    /// shared module `config` per request). Build a non-live one with
    /// [`ProxyOptions::with_static_config`].
    pub config: SharedNetworkConfig,
    /// Optional resolved parent/upstream proxy (chaining). `None` ⇒ direct.
    pub parent_proxy: Option<Arc<ResolvedParentProxy>>,
    /// Optional per-request filter callback (the `filterRequest` body hook).
    pub filter_request: Option<FilterRequestFn>,
    /// Optional MITM CA. When set, a `CONNECT` whose first post-`200` bytes are a
    /// TLS `ClientHello` is TLS-terminated (per-host minted leaf) and each decrypted
    /// request is re-issued upstream over real TLS; non-TLS CONNECT bytes still
    /// opaque-tunnel. `None` ⇒ the P3b opaque-tunnel path, unchanged.
    pub mitm_ca: Option<Arc<MitmCa>>,
    /// Extra CA certificate(s) (DER) to trust for the terminator's upstream TLS
    /// leg (the TS `target.upstreamCA`), in addition to the system roots. Only
    /// consulted when [`Self::mitm_ca`] is set.
    pub tls_terminate_upstream_ca: Option<Arc<Vec<rustls::pki_types::CertificateDer<'static>>>>,
    /// Optional interactive ask-callback (the TS `sandboxAskCallback`). Consulted
    /// by [`filter_network_request_with_ask`] ONLY for hosts that no
    /// allow/deny rule decides. `None` ⇒ unmatched hosts are denied (the P3b
    /// behaviour). Added for P8b so the [`crate::manager::SandboxManager`] can
    /// thread its ask-callback into the live filter; existing callers set it to
    /// `None`.
    pub ask: Option<AskFn>,
    /// Optional per-session token that validates per-command proxy credentials.
    pub proxy_auth_token: Option<String>,
    /// Optional violation store for `<sandbox_violations>` attribution.
    pub violation_store: Option<Arc<SandboxViolationStore>>,
}

impl ProxyOptions {
    /// Build a [`ProxyOptions`] whose `config` is a fresh non-live
    /// [`SharedNetworkConfig`] wrapping `config` (no external writer). For direct
    /// callers + tests that don't need the manager's live-swap; the per-request
    /// read path is identical, just nobody ever writes the handle.
    #[must_use]
    pub fn with_static_config(
        config: NetworkConfig,
        parent_proxy: Option<Arc<ResolvedParentProxy>>,
        filter_request: Option<FilterRequestFn>,
        mitm_ca: Option<Arc<MitmCa>>,
        tls_terminate_upstream_ca: Option<Arc<Vec<rustls::pki_types::CertificateDer<'static>>>>,
        ask: Option<AskFn>,
    ) -> Self {
        Self {
            config: shared_network_config(config),
            parent_proxy,
            filter_request,
            mitm_ca,
            tls_terminate_upstream_ca,
            ask,
            proxy_auth_token: None,
            violation_store: None,
        }
    }
}

impl std::fmt::Debug for ProxyOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyOptions")
            .field("config", &self.config)
            .field("parent_proxy", &self.parent_proxy)
            .field("filter_request", &self.filter_request.is_some())
            .field("mitm_ca", &self.mitm_ca.is_some())
            .field(
                "tls_terminate_upstream_ca",
                &self.tls_terminate_upstream_ca.as_ref().map(|c| c.len()),
            )
            .field("ask", &self.ask.is_some())
            .field("proxy_auth_token", &self.proxy_auth_token.is_some())
            .field("violation_store", &self.violation_store.is_some())
            .finish()
    }
}

/// The response body type produced by the proxy service: a boxed body so we can
/// uniformly return relayed upstream bodies, buffered deny bodies, and empty
/// bodies.
type ProxyBody = BoxBody<Bytes, hyper::Error>;

/// Run the forward-proxy accept loop on `listener`. Each accepted connection is
/// served on its own task with `with_upgrades()` so `CONNECT` can hand off the
/// raw stream. Errors are logged-and-dropped (a single bad connection never
/// brings the loop down), matching the Node server's per-socket error handling.
pub async fn serve(listener: TcpListener, options: Arc<ProxyOptions>) {
    loop {
        let Ok((client, _peer)) = listener.accept().await else {
            continue;
        };
        let options = Arc::clone(&options);
        tokio::spawn(async move {
            let io = TokioIo::new(client);
            let service = service_fn(move |req: Request<Incoming>| {
                let options = Arc::clone(&options);
                async move { Ok::<_, hyper::Error>(handle(req, options).await) }
            });
            if let Err(e) = http1::Builder::new()
                .serve_connection(io, service)
                .with_upgrades()
                .await
            {
                tracing::debug!(error = %e, "proxy connection ended");
            }
        });
    }
}

/// Dispatch a single request to the CONNECT or plain-HTTP handler.
async fn handle(req: Request<Incoming>, options: Arc<ProxyOptions>) -> Response<ProxyBody> {
    if req.method() == Method::CONNECT {
        handle_connect(req, &options).await
    } else {
        handle_plain(req, &options).await
    }
}

/// Build the byte-exact 403 allowlist-denial response shared by both handlers
/// (`X-Proxy-Error: blocked-by-allowlist`, body `Connection blocked by network
/// allowlist`, `Content-Type: text/plain`).
fn blocked_by_allowlist() -> Response<ProxyBody> {
    Response::builder()
        .status(StatusCode::FORBIDDEN)
        .header(http::header::CONTENT_TYPE, "text/plain")
        .header("X-Proxy-Error", "blocked-by-allowlist")
        .body(full_body("Connection blocked by network allowlist"))
        .expect("static 403 allowlist response is always valid")
}

/// A simple status-only response with an empty body.
fn status_only(status: StatusCode) -> Response<ProxyBody> {
    Response::builder()
        .status(status)
        .body(empty_body())
        .expect("status-only response is always valid")
}

fn empty_body() -> ProxyBody {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

fn full_body(s: &str) -> ProxyBody {
    Full::new(Bytes::from(s.to_owned()))
        .map_err(|never| match never {})
        .boxed()
}

fn format_host_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn decode_authenticated_command(
    headers: &http::HeaderMap,
    proxy_auth_token: Option<&str>,
) -> Option<String> {
    let expected = proxy_auth_token?;
    let header = headers
        .get(http::header::PROXY_AUTHORIZATION)?
        .to_str()
        .ok()?;
    let basic = header.strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(basic)
        .ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (username, password) = decoded.split_once(':')?;
    if password != expected {
        return None;
    }
    username.strip_prefix("srt.").map(ToString::to_string)
}

fn record_network_violation(
    options: &ProxyOptions,
    encoded_command: Option<String>,
    host: &str,
    port: u16,
    reason: &str,
) {
    let Some(store) = options.violation_store.as_ref() else {
        return;
    };
    let Some(encoded_command) = encoded_command else {
        return;
    };
    let line = format!(
        "deny network-outbound {} ({reason})",
        format_host_port(host, port)
    );
    store.add_violation(Violation {
        encoded_command,
        line: Some(line),
        host: Some(host.to_string()),
        port: Some(port),
        operation: Some("connect".into()),
        ..Default::default()
    });
}

// ─────────────────────────────── CONNECT ───────────────────────────────

/// `server.on('connect', ...)` (http-proxy.js:12-145). Filter, dial (parent or
/// direct), reply `200 Connection Established`, then upgrade the client stream
/// and splice it to the upstream. Malformed target → 400; denied → byte-exact
/// 403; dial failure → 502.
async fn handle_connect(
    req: Request<Incoming>,
    options: &Arc<ProxyOptions>,
) -> Response<ProxyBody> {
    // The CONNECT request-target is the authority (e.g. `host:443`), carried in
    // the URI. hyper exposes it via `uri().authority()` / the path-and-query.
    let target = req
        .uri()
        .authority()
        .map(ToString::to_string)
        .or_else(|| Some(req.uri().to_string()))
        .and_then(|t| parse_connect_target(&t));

    let Some((hostname, port)) = target else {
        tracing::debug!(uri = %req.uri(), "invalid CONNECT request");
        return status_only(StatusCode::BAD_REQUEST);
    };

    // Read the CURRENT live config (clone the inner Arc), then DROP the guard
    // before the await — the std RwLock is never held across a suspension point.
    let net = current_network_config(&options.config);
    let encoded_command =
        decode_authenticated_command(req.headers(), options.proxy_auth_token.as_deref());
    let decision =
        filter_network_request_with_ask_decision(port, &hostname, &net, options.ask.as_ref()).await;
    if let Some(reason) = decision.denied_reason() {
        record_network_violation(options, encoded_command, &hostname, port, reason);
        tracing::debug!(%hostname, port, "CONNECT blocked by allowlist");
        return blocked_by_allowlist();
    }

    // P6b: mitmCA routing — when a MITM CA is configured, the spawned tunnel
    // task sniffs the upgraded stream for a TLS ClientHello and TLS-terminates it
    // (per-request upstream re-issue); non-TLS bytes fall through to the opaque
    // tunnel below. (`getMitmSocketPath` external-socket seam stays for plain
    // HTTP.) `mitm_ca = None` ⇒ the opaque path is taken unconditionally.

    // Decide route: parent-proxy CONNECT (when set and not bypassed) > direct.
    let parent_url = options
        .parent_proxy
        .as_ref()
        .filter(|p| !should_bypass_parent_proxy(p, &hostname))
        .and_then(|p| select_parent_proxy_url(p, true).cloned());

    let options = Arc::clone(options);
    // Upgrade happens after we return the 200; spawn the tunnel task that awaits
    // the upgraded client stream and dials the upstream.
    tokio::spawn(async move {
        if let Err(e) = run_connect_tunnel(req, hostname, port, parent_url, options).await {
            tracing::debug!(error = %e, "CONNECT tunnel ended");
        }
    });

    // Reply 200 Connection Established (hyper completes the upgrade once this
    // response is written and the body is empty).
    Response::builder()
        .status(StatusCode::OK)
        .body(empty_body())
        .expect("200 Connection Established is always valid")
}

/// Await the upgraded client stream, then either TLS-terminate it (when a MITM
/// CA is configured AND the first bytes are a TLS `ClientHello`) or dial the
/// upstream (parent or direct) and opaque-splice them.
///
/// When `mitm_ca` is `None` we never sniff: the upgraded stream is spliced
/// directly (the P3b behavior, unchanged). Early client bytes are delivered as
/// part of the upgraded stream; for the opaque path no separate `head` replay is
/// needed, but the TLS-sniff consumes a few bytes which we replay into either
/// the TLS acceptor (terminate) or a prepended copy (opaque fall-through).
async fn run_connect_tunnel(
    req: Request<Incoming>,
    hostname: String,
    port: u16,
    parent_url: Option<url::Url>,
    options: Arc<ProxyOptions>,
) -> std::io::Result<()> {
    let upgraded = hyper::upgrade::on(req)
        .await
        .map_err(std::io::Error::other)?;
    let mut client = TokioIo::new(upgraded);

    // P6b: TLS-MITM termination. Sniff the first bytes for a ClientHello; on TLS,
    // terminate in-process (per-request origin re-issue, NO upstream dial here);
    // non-TLS bytes fall through to the opaque tunnel with the sniffed head
    // prepended (the SSH-over-CONNECT case).
    if let Some(ca) = options.mitm_ca.clone() {
        let (is_tls, head) = peek_client_hello(&mut client, Vec::new()).await;
        if is_tls {
            let target = TlsTarget {
                hostname,
                port,
                upstream_ca: options.tls_terminate_upstream_ca.clone(),
            };
            return terminate_and_forward(ca, options.filter_request.clone(), client, head, target)
                .await;
        }
        // Non-TLS: opaque-tunnel, replaying the sniffed head first.
        return opaque_tunnel(client, hostname, port, parent_url, head).await;
    }

    opaque_tunnel(client, hostname, port, parent_url, Vec::new()).await
}

/// Dial the upstream (parent-proxy CONNECT or direct) and opaque-splice the raw
/// `client` stream to it, writing `head` (any bytes consumed by the TLS sniff)
/// to the upstream first so nothing is lost. This is the P3b byte-tunnel path,
/// reused for both the non-MITM CONNECT and the MITM non-TLS fall-through.
async fn opaque_tunnel<C>(
    mut client: C,
    hostname: String,
    port: u16,
    parent_url: Option<url::Url>,
    head: Vec<u8>,
) -> std::io::Result<()>
where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    if let Some(url) = parent_url {
        let mut upstream = match tokio::time::timeout(
            CONNECT_TIMEOUT,
            connect_via_parent_proxy(&url, &hostname, port),
        )
        .await
        {
            Ok(Ok(u)) => u,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "parent-proxy CONNECT dial timed out",
                ))
            }
        };
        if !head.is_empty() {
            tokio::io::AsyncWriteExt::write_all(&mut upstream, &head).await?;
        }
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    } else {
        let mut upstream = dial_direct(&hostname, port).await?;
        if !head.is_empty() {
            tokio::io::AsyncWriteExt::write_all(&mut upstream, &head).await?;
        }
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    }
    Ok(())
}

// ─────────────────────────────── plain HTTP ───────────────────────────────

/// `server.on('request', ...)` (http-proxy.js:147-258). Absolute-form request
/// URI; parse host+port; filter; reconstruct the absolute URI from parsed
/// components; strip hop-by-hop + set Host; run the request-filter; route to
/// parent-proxy or direct; relay the response (hop-by-hop stripped). Upstream
/// error → 502.
async fn handle_plain(req: Request<Incoming>, options: &Arc<ProxyOptions>) -> Response<ProxyBody> {
    let uri = req.uri().clone();
    let scheme = uri.scheme_str().unwrap_or("http").to_string();
    let is_https = scheme == "https";

    // The absolute-form URI must carry an authority (host). Without one we
    // cannot allowlist-check, so reject as malformed.
    let Some(authority) = uri.authority().cloned() else {
        return status_only(StatusCode::BAD_REQUEST);
    };
    let hostname = crate::host::strip_brackets(authority.host());
    let port = authority
        .port_u16()
        .unwrap_or(if is_https { 443 } else { 80 });

    // Read the CURRENT live config (clone the inner Arc), then DROP the guard
    // before the await — the std RwLock is never held across a suspension point.
    let net = current_network_config(&options.config);
    let encoded_command =
        decode_authenticated_command(req.headers(), options.proxy_auth_token.as_deref());
    let decision =
        filter_network_request_with_ask_decision(port, &hostname, &net, options.ask.as_ref()).await;
    if let Some(reason) = decision.denied_reason() {
        record_network_violation(options, encoded_command, &hostname, port, reason);
        tracing::debug!(%hostname, port, "HTTP request blocked by allowlist");
        return blocked_by_allowlist();
    }

    // Reconstruct the absolute URI from PARSED components (close URL-parser
    // differential bypasses): scheme + authority + path-and-query.
    let path_and_query = uri
        .path_and_query()
        .map_or("/", http::uri::PathAndQuery::as_str)
        .to_string();
    let host_header = authority.as_str().to_string();
    let abs_url = format!("{scheme}://{host_header}{path_and_query}");

    let method = req.method().clone();

    // strip_hop_by_hop on the request headers + set Host to the parsed authority.
    let in_headers: Vec<(String, String)> = req
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let mut fwd_headers = strip_hop_by_hop(&in_headers);
    fwd_headers.retain(|(k, _)| !k.eq_ignore_ascii_case("host"));
    fwd_headers.push(("host".to_string(), host_header.clone()));

    // P6: getMitmSocketPath routing seam here — when MITM is wired, route plain
    // HTTP through the MITM unix socket before the parent/direct branch below.

    // Run the per-request filter (applies to plain HTTP too). On deny → 403;
    // on allow → the (buffered) body to forward.
    let (parts, body) = req.into_parts();
    let body_to_forward: Full<Bytes> = if let Some(filter) = options.filter_request.as_ref() {
        match decide_and_respond(filter, &abs_url, &method, &parts.headers, body).await {
            FilterOutcome::Allow(b) => b,
            FilterOutcome::Deny(resp) => {
                let (p, b) = resp.into_parts();
                return Response::from_parts(p, b.map_err(|never| match never {}).boxed());
            }
        }
    } else {
        // No filter: collect the body to forward verbatim. (A faithful
        // streaming forward without buffering is possible but the parent/direct
        // request builders below take a unified `Full<Bytes>` body; buffering
        // here keeps the forward path uniform. Loopback-only exposure makes the
        // buffer acceptable.)
        match body.collect().await {
            Ok(c) => Full::new(c.to_bytes()),
            Err(_) => return status_only(StatusCode::BAD_GATEWAY),
        }
    };

    // Route: parent-proxy (set + not bypassed) > direct.
    let parent_url = options
        .parent_proxy
        .as_ref()
        .filter(|p| !should_bypass_parent_proxy(p, &hostname))
        .and_then(|p| select_parent_proxy_url(p, is_https).cloned());

    if let Some(url) = parent_url {
        forward_via_parent(&url, &method, &abs_url, fwd_headers, body_to_forward).await
    } else {
        forward_direct(
            &hostname,
            port,
            &method,
            &path_and_query,
            fwd_headers,
            body_to_forward,
        )
        .await
    }
}

/// Forward to the origin directly via a per-request `http1` handshake. The
/// request line uses origin-form (`path?query`); the response is relayed with
/// hop-by-hop headers stripped. Upstream error → 502.
async fn forward_direct(
    hostname: &str,
    port: u16,
    method: &Method,
    path_and_query: &str,
    fwd_headers: Vec<(String, String)>,
    body: Full<Bytes>,
) -> Response<ProxyBody> {
    let Ok(stream) = dial_direct(hostname, port).await else {
        return bad_gateway();
    };
    send_upstream(stream, method, path_and_query, fwd_headers, body).await
}

/// Forward to the parent proxy: the request line uses the ABSOLUTE URI (proxy
/// request-target) and includes `Proxy-Authorization` when the parent URL
/// carries credentials. Response relayed with hop-by-hop stripped; error → 502.
async fn forward_via_parent(
    parent_url: &url::Url,
    method: &Method,
    abs_url: &str,
    mut fwd_headers: Vec<(String, String)>,
    body: Full<Bytes>,
) -> Response<ProxyBody> {
    let parent_host = crate::host::strip_brackets(parent_url.host_str().unwrap_or(""));
    let parent_port = parent_url
        .port()
        .unwrap_or(if parent_url.scheme() == "https" {
            443
        } else {
            80
        });
    if let Some(auth) = proxy_auth_header(parent_url) {
        fwd_headers.push(("proxy-authorization".to_string(), auth));
    }
    // NOTE: an `https://` parent proxy needs a TLS handshake to the parent for
    // plain-HTTP forwarding. The base sandbox config only emits `http://`
    // parent proxies for the plain-HTTP path in practice; an https-parent
    // plain-HTTP forward would need a tokio-rustls client here. Documented
    // compromise: we dial the parent over TCP (http parent). For https-parent
    // CONNECT chaining (the common case) see `connect_via_parent_proxy`.
    let Ok(stream) = dial_direct(&parent_host, parent_port).await else {
        return bad_gateway();
    };
    // The parent receives the absolute URI as the request target.
    send_upstream(stream, method, abs_url, fwd_headers, body).await
}

/// Perform the `http1` client handshake over `stream`, send the request with
/// `request_target` as the request line, and relay the response with hop-by-hop
/// headers stripped.
async fn send_upstream(
    stream: TcpStream,
    method: &Method,
    request_target: &str,
    fwd_headers: Vec<(String, String)>,
    body: Full<Bytes>,
) -> Response<ProxyBody> {
    let io = TokioIo::new(stream);
    let Ok((mut sender, conn)) = hyper::client::conn::http1::handshake(io).await else {
        return bad_gateway();
    };
    tokio::spawn(async move {
        if let Err(e) = conn.with_upgrades().await {
            tracing::debug!(error = %e, "upstream connection ended");
        }
    });

    let mut builder = Request::builder()
        .method(method.clone())
        .uri(request_target);
    for (k, v) in fwd_headers {
        builder = builder.header(k, v);
    }
    let Ok(upstream_req) = builder.body(body) else {
        return bad_gateway();
    };

    let Ok(Ok(resp)) =
        tokio::time::timeout(Duration::from_secs(30), sender.send_request(upstream_req)).await
    else {
        return bad_gateway();
    };

    // Relay: status + hop-by-hop-stripped response headers + streamed body.
    let (parts, body) = resp.into_parts();
    let resp_headers: Vec<(String, String)> = parts
        .headers
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let stripped = strip_hop_by_hop(&resp_headers);

    let mut out = Response::builder().status(parts.status);
    for (k, v) in stripped {
        out = out.header(k, v);
    }
    out.body(body.boxed()).unwrap_or_else(|_| bad_gateway())
}

/// `502 Bad Gateway` with the byte-exact body the TS reference sends
/// (`Bad Gateway`, `Content-Type: text/plain`).
fn bad_gateway() -> Response<ProxyBody> {
    Response::builder()
        .status(StatusCode::BAD_GATEWAY)
        .header(http::header::CONTENT_TYPE, "text/plain")
        .body(full_body("Bad Gateway"))
        .expect("static 502 response is always valid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::sandbox_proxy_username;
    use crate::violation_store::SandboxViolationStore;
    use rustls::pki_types::pem::PemObject;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    fn opts(cfg: NetworkConfig) -> Arc<ProxyOptions> {
        Arc::new(ProxyOptions::with_static_config(
            cfg, None, None, None, None, None,
        ))
    }

    async fn start_proxy(options: Arc<ProxyOptions>) -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move { serve(l, options).await });
        port
    }

    /// A TCP echo upstream (for CONNECT tunnel tests).
    async fn echo_upstream() -> (String, u16) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut s, _)) = l.accept().await {
                let mut buf = [0u8; 64];
                loop {
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    if s.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        });
        (addr.ip().to_string(), addr.port())
    }

    /// A tiny hyper origin returning 200 "ok" and recording the headers it saw
    /// (for the hop-by-hop test). Returns (host, port, captured-headers handle).
    async fn hyper_origin() -> (String, u16, Arc<tokio::sync::Mutex<Vec<(String, String)>>>) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let captured = Arc::new(tokio::sync::Mutex::new(Vec::<(String, String)>::new()));
        let cap2 = Arc::clone(&captured);
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = l.accept().await else {
                    break;
                };
                let cap = Arc::clone(&cap2);
                tokio::spawn(async move {
                    let io = TokioIo::new(sock);
                    let svc = service_fn(move |req: Request<Incoming>| {
                        let cap = Arc::clone(&cap);
                        async move {
                            let mut g = cap.lock().await;
                            for (k, v) in req.headers() {
                                g.push((
                                    k.as_str().to_string(),
                                    v.to_str().unwrap_or("").to_string(),
                                ));
                            }
                            drop(g);
                            Ok::<_, hyper::Error>(Response::new(Full::new(Bytes::from_static(
                                b"ok",
                            ))))
                        }
                    });
                    let _ = http1::Builder::new().serve_connection(io, svc).await;
                });
            }
        });
        (addr.ip().to_string(), addr.port(), captured)
    }

    /// Send a raw CONNECT and return the proxy's status line + the live socket.
    async fn connect_via_proxy(proxy_port: u16, target: &str) -> (String, TcpStream) {
        let mut s = TcpStream::connect(("127.0.0.1", proxy_port)).await.unwrap();
        s.write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut buf = vec![0u8; 128];
        let n = s.read(&mut buf).await.unwrap();
        let head = String::from_utf8_lossy(&buf[..n]).into_owned();
        let line = head.lines().next().unwrap_or("").to_string();
        (line, s)
    }

    async fn connect_via_proxy_with_auth(
        proxy_port: u16,
        target: &str,
        command: &str,
        token: &str,
    ) -> String {
        let basic = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{token}", sandbox_proxy_username(command)));
        let mut s = TcpStream::connect(("127.0.0.1", proxy_port)).await.unwrap();
        s.write_all(
            format!(
                "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nProxy-Authorization: Basic {basic}\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        let mut buf = vec![0u8; 128];
        let n = s.read(&mut buf).await.unwrap();
        String::from_utf8_lossy(&buf[..n])
            .lines()
            .next()
            .unwrap_or("")
            .to_string()
    }

    #[tokio::test]
    async fn connect_allow_tunnels_and_deny_403() {
        let (uhost, uport) = echo_upstream().await;
        let cfg = NetworkConfig {
            allowed_domains: vec![uhost.clone()],
            denied_domains: vec![],
            ..Default::default()
        };
        let pport = start_proxy(opts(cfg)).await;

        let (line, mut tun) = connect_via_proxy(pport, &format!("{uhost}:{uport}")).await;
        assert!(line.contains("200"), "expected 200, got {line}");
        tun.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        tun.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");

        let (line, _) = connect_via_proxy(pport, "1.2.3.4:443").await;
        assert!(line.contains("403"), "expected 403, got {line}");
    }

    /// `strictAllowlist` (2.1.219) at the enforcement layer: an unmatched
    /// CONNECT is 403'd deterministically even though an ask callback is
    /// wired — the filter's strict gate (`if(!r||xl.network.strictAllowlist)`,
    /// 2.1.220 @229871903) runs BEFORE the callback, so it never fires.
    #[tokio::test]
    async fn connect_strict_allowlist_denies_despite_ask() {
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ran_in = Arc::clone(&ran);
        let ask: AskFn = Arc::new(move |_host: &str, _port: u16| {
            let ran = Arc::clone(&ran_in);
            Box::pin(async move {
                ran.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(true)
            })
                as std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, _>> + Send>>
        });
        let cfg = NetworkConfig {
            allowed_domains: vec!["allowed.example".into()],
            denied_domains: vec![],
            strict_allowlist: Some(true),
            ..Default::default()
        };
        let pport = start_proxy(Arc::new(ProxyOptions::with_static_config(
            cfg,
            None,
            None,
            None,
            None,
            Some(ask),
        )))
        .await;
        let (line, _s) = connect_via_proxy(pport, "unmatched.example:443").await;
        assert!(line.contains("403"), "expected 403, got {line}");
        assert!(
            !ran.load(std::sync::atomic::Ordering::SeqCst),
            "strict mode must not consult the ask callback"
        );
    }

    #[tokio::test]
    async fn connect_malformed_target_400() {
        let pport = start_proxy(opts(NetworkConfig::default())).await;
        // hyper rejects a CONNECT with no authority before our handler in some
        // cases; send a syntactically-valid-but-portless authority to exercise
        // our parse_connect_target → 400 path.
        let mut s = TcpStream::connect(("127.0.0.1", pport)).await.unwrap();
        s.write_all(b"CONNECT noport HTTP/1.1\r\nHost: noport\r\n\r\n")
            .await
            .unwrap();
        let mut buf = vec![0u8; 128];
        let n = s.read(&mut buf).await.unwrap();
        let line = String::from_utf8_lossy(&buf[..n])
            .lines()
            .next()
            .unwrap_or("")
            .to_string();
        assert!(line.contains("400"), "expected 400, got {line}");
    }

    #[tokio::test]
    async fn connect_early_payload_preserved() {
        let (uhost, uport) = echo_upstream().await;
        let cfg = NetworkConfig {
            allowed_domains: vec![uhost.clone()],
            denied_domains: vec![],
            ..Default::default()
        };
        let pport = start_proxy(opts(cfg)).await;

        let mut s = TcpStream::connect(("127.0.0.1", pport)).await.unwrap();
        // CONNECT head AND the early payload in one write (same segment).
        s.write_all(
            format!("CONNECT {uhost}:{uport} HTTP/1.1\r\nHost: {uhost}\r\n\r\nEARLY").as_bytes(),
        )
        .await
        .unwrap();

        let mut buf = vec![0u8; 256];
        let mut got = Vec::new();
        loop {
            let n = s.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
            if got.windows(5).any(|w| w == b"EARLY") {
                break;
            }
        }
        let text = String::from_utf8_lossy(&got);
        assert!(text.contains("200"), "expected 200 status, got {text:?}");
        assert!(
            text.contains("EARLY"),
            "early payload lost; echo was {text:?}"
        );
    }

    /// Send an absolute-form plain-HTTP request through the proxy, return the
    /// raw response bytes.
    async fn http_request_via_proxy(
        proxy_port: u16,
        abs_url: &str,
        host_authority: &str,
    ) -> Vec<u8> {
        let mut s = TcpStream::connect(("127.0.0.1", proxy_port)).await.unwrap();
        s.write_all(
            format!(
                "GET {abs_url} HTTP/1.1\r\nHost: {host_authority}\r\nConnection: close, X-Drop\r\nX-Drop: secret\r\nX-Keep: keep\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        let mut buf = Vec::new();
        loop {
            let mut chunk = [0u8; 512];
            let n = s.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        buf
    }

    #[tokio::test]
    async fn plain_http_allow_forwards_to_origin() {
        let (ohost, oport, _cap) = hyper_origin().await;
        let cfg = NetworkConfig {
            allowed_domains: vec![ohost.clone()],
            denied_domains: vec![],
            ..Default::default()
        };
        let pport = start_proxy(opts(cfg)).await;
        let authority = format!("{ohost}:{oport}");
        let abs = format!("http://{authority}/");
        let resp = http_request_via_proxy(pport, &abs, &authority).await;
        let text = String::from_utf8_lossy(&resp);
        assert!(text.contains("200"), "expected 200, got {text:?}");
        assert!(
            text.ends_with("ok") || text.contains("\r\nok"),
            "expected body ok, got {text:?}"
        );
    }

    #[tokio::test]
    async fn plain_http_deny_403_byte_exact() {
        // empty allowlist ⇒ deny-all
        let pport = start_proxy(opts(NetworkConfig::default())).await;
        let resp =
            http_request_via_proxy(pport, "http://denied.example:80/", "denied.example:80").await;
        let text = String::from_utf8_lossy(&resp);
        assert!(text.contains("403"), "expected 403, got {text:?}");
        assert!(
            text.contains("X-Proxy-Error: blocked-by-allowlist")
                || text
                    .to_ascii_lowercase()
                    .contains("x-proxy-error: blocked-by-allowlist"),
            "missing X-Proxy-Error header: {text:?}"
        );
        assert!(
            text.ends_with("Connection blocked by network allowlist"),
            "wrong 403 body: {text:?}"
        );
    }

    #[tokio::test]
    async fn plain_http_strips_hop_by_hop_on_forward() {
        let (ohost, oport, cap) = hyper_origin().await;
        let cfg = NetworkConfig {
            allowed_domains: vec![ohost.clone()],
            denied_domains: vec![],
            ..Default::default()
        };
        let pport = start_proxy(opts(cfg)).await;
        let authority = format!("{ohost}:{oport}");
        let abs = format!("http://{authority}/");
        let _ = http_request_via_proxy(pport, &abs, &authority).await;

        // Give the origin a moment to record headers.
        let mut seen: Vec<(String, String)> = Vec::new();
        for _ in 0..50 {
            seen = cap.lock().await.clone();
            if !seen.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        let names: Vec<String> = seen.iter().map(|(k, _)| k.to_ascii_lowercase()).collect();
        assert!(
            !names.contains(&"connection".to_string()),
            "Connection header leaked: {names:?}"
        );
        // X-Drop was named in the Connection token list → must be stripped.
        assert!(
            !names.contains(&"x-drop".to_string()),
            "Connection-listed X-Drop leaked: {names:?}"
        );
        // X-Keep survives; Host is set to the parsed authority.
        assert!(
            names.contains(&"x-keep".to_string()),
            "X-Keep lost: {names:?}"
        );
        let host = seen.iter().find(|(k, _)| k.eq_ignore_ascii_case("host"));
        assert_eq!(host.map(|(_, v)| v.as_str()), Some(authority.as_str()));
    }

    // ─────────── P6b: CONNECT TLS-MITM wiring (real in-process TLS) ───────────

    use crate::mitm_ca::{create_mitm_ca, dispose_mitm_ca, MitmCaOptions};
    use crate::request_filter::{Decision, FilterRequest};
    use rustls::pki_types::{CertificateDer, ServerName};
    use std::future::Future;
    use std::pin::Pin;
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    /// Stand up a real HTTPS origin on loopback (host=127.0.0.1) returning 200
    /// "ok", with a proper CA→leaf chain. Returns (port, the origin CA DER for
    /// `upstream_ca` trust).
    async fn https_origin_127() -> (u16, CertificateDer<'static>) {
        let host = "127.0.0.1";
        // Origin CA.
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "test-origin-ca");
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();
        let ca_der = CertificateDer::from(ca_cert.der().to_vec());
        // Leaf (SAN=IP:127.0.0.1) signed by the origin CA.
        let leaf_key = rcgen::KeyPair::generate().unwrap();
        let leaf_params = rcgen::CertificateParams::new(vec![host.to_string()]).unwrap();
        let leaf_cert = leaf_params.signed_by(&leaf_key, &ca_cert, &ca_key).unwrap();
        let leaf_der = CertificateDer::from(leaf_cert.der().to_vec());
        let leaf_key_der =
            rustls::pki_types::PrivateKeyDer::try_from(leaf_key.serialize_der()).unwrap();

        let mut config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![leaf_der, ca_der.clone()], leaf_key_der)
            .unwrap();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let acceptor = TlsAcceptor::from(Arc::new(config));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    break;
                };
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let io = TokioIo::new(tls);
                    let svc = service_fn(|_req: Request<Incoming>| async {
                        Ok::<_, hyper::Error>(Response::new(Full::new(Bytes::from_static(b"ok"))))
                    });
                    let _ = http1::Builder::new().serve_connection(io, svc).await;
                });
            }
        });
        (port, ca_der)
    }

    /// First cert (the CA) of a PEM string, as DER.
    fn first_cert_der(pem: &str) -> CertificateDer<'static> {
        let mut rd = std::io::BufReader::new(pem.as_bytes());
        let der = rustls::pki_types::CertificateDer::pem_reader_iter(&mut rd)
            .next()
            .unwrap()
            .unwrap();
        CertificateDer::from(der.to_vec())
    }

    /// Open a CONNECT tunnel through the proxy to `127.0.0.1:port`, then drive a
    /// CA-trusting TLS client over it and `GET path`. Returns (status, body).
    async fn connect_tls_get(
        proxy_port: u16,
        origin_port: u16,
        ca_der: CertificateDer<'static>,
        path: &str,
    ) -> (u16, String) {
        let mut s = TcpStream::connect(("127.0.0.1", proxy_port)).await.unwrap();
        let target = format!("127.0.0.1:{origin_port}");
        s.write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        // Read the 200 status line (up to the blank line).
        let mut buf = [0u8; 256];
        let n = s.read(&mut buf).await.unwrap();
        let head = String::from_utf8_lossy(&buf[..n]).into_owned();
        assert!(head.contains("200"), "CONNECT did not return 200: {head:?}");

        let mut roots = rustls::RootCertStore::empty();
        roots.add(ca_der).unwrap();
        let mut config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let connector = TlsConnector::from(Arc::new(config));
        let server_name = ServerName::try_from("127.0.0.1").unwrap();
        let tls = connector.connect(server_name, s).await.unwrap();

        let io = TokioIo::new(tls);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let req = Request::builder()
            .method(Method::GET)
            .uri(path)
            .header(http::header::HOST, "127.0.0.1")
            .body(empty_body())
            .unwrap();
        let resp = sender.send_request(req).await.unwrap();
        let status = resp.status().as_u16();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    fn mitm_opts(
        ca: Arc<MitmCa>,
        upstream_ca: CertificateDer<'static>,
        filter: Option<FilterRequestFn>,
    ) -> Arc<ProxyOptions> {
        Arc::new(ProxyOptions::with_static_config(
            NetworkConfig {
                allowed_domains: vec!["127.0.0.1".to_string()],
                denied_domains: vec![],
                ..Default::default()
            },
            None,
            filter,
            Some(ca),
            Some(Arc::new(vec![upstream_ca])),
            None,
        ))
    }

    /// MITM CONNECT: a CA-trusting client does CONNECT + TLS + GET through the
    /// proxy to a stand-in origin → 200 "ok" (the decrypted request is re-issued
    /// upstream and the response round-trips).
    #[tokio::test]
    async fn connect_mitm_terminates_and_round_trips() {
        let (oport, origin_ca) = https_origin_127().await;
        let ca = Arc::new(create_mitm_ca(MitmCaOptions::default()).unwrap());
        let ca_der = first_cert_der(&ca.cert_pem);
        let pport = start_proxy(mitm_opts(Arc::clone(&ca), origin_ca, None)).await;

        let (status, body) = connect_tls_get(pport, oport, ca_der, "/hello").await;
        assert_eq!(status, 200);
        assert_eq!(body, "ok");
        dispose_mitm_ca(&ca);
    }

    /// MITM CONNECT with a denying filter → the CA-trusting client gets a 403.
    #[tokio::test]
    async fn connect_mitm_filter_denies_403() {
        let (oport, origin_ca) = https_origin_127().await;
        let ca = Arc::new(create_mitm_ca(MitmCaOptions::default()).unwrap());
        let ca_der = first_cert_der(&ca.cert_pem);
        let filter: FilterRequestFn = Arc::new(|req: FilterRequest| {
            let denied = req.url.contains("/blocked");
            Box::pin(async move {
                if denied {
                    Decision::Deny {
                        reason: Some("blocked".to_string()),
                    }
                } else {
                    Decision::Allow
                }
            }) as Pin<Box<dyn Future<Output = Decision> + Send>>
        });
        let pport = start_proxy(mitm_opts(Arc::clone(&ca), origin_ca, Some(filter))).await;

        let (status, _) = connect_tls_get(pport, oport, ca_der, "/blocked").await;
        assert_eq!(status, 403);
        dispose_mitm_ca(&ca);
    }

    /// LIVE update_config proof on a RUNNING proxy: with `allowedDomains=[]`
    /// (deny-all), a CONNECT to the echo upstream is denied (403); after writing
    /// a new config (`allowedDomains=[host]`) into the SHARED handle the proxy is
    /// reading, a NEW CONNECT to the same host is allowed (tunnels) — no rebind,
    /// no restart. This is the Rust equivalent of the TS per-request config read.
    #[tokio::test]
    async fn live_config_swap_changes_decision_on_running_proxy() {
        use crate::config::shared_network_config;

        let (uhost, uport) = echo_upstream().await;

        // Start deny-all (empty allowlist) on a shared, swappable handle.
        let shared = shared_network_config(NetworkConfig::default());
        let options = Arc::new(ProxyOptions {
            config: Arc::clone(&shared),
            parent_proxy: None,
            filter_request: None,
            mitm_ca: None,
            tls_terminate_upstream_ca: None,
            ask: None,
            proxy_auth_token: None,
            violation_store: None,
        });
        let pport = start_proxy(options).await;

        // Deny-all → 403.
        let (line, _) = connect_via_proxy(pport, &format!("{uhost}:{uport}")).await;
        assert!(
            line.contains("403"),
            "expected 403 before update, got {line}"
        );

        // LIVE swap: allow the upstream host. The running proxy reads this on the
        // next request with no rebind.
        *shared.write().unwrap() = Arc::new(NetworkConfig {
            allowed_domains: vec![uhost.clone()],
            denied_domains: vec![],
            ..Default::default()
        });

        // A NEW request is now allowed and tunnels end-to-end.
        let (line, mut tun) = connect_via_proxy(pport, &format!("{uhost}:{uport}")).await;
        assert!(
            line.contains("200"),
            "expected 200 after update, got {line}"
        );
        tun.write_all(b"live").await.unwrap();
        let mut buf = [0u8; 4];
        tun.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"live", "live-allowed tunnel should echo");
    }

    /// `mitm_ca = None` leaves the opaque tunnel intact: a CONNECT to a TCP echo
    /// upstream still byte-tunnels (the P3b path is unchanged).
    #[tokio::test]
    async fn connect_without_mitm_still_opaque_tunnels() {
        let (uhost, uport) = echo_upstream().await;
        let cfg = NetworkConfig {
            allowed_domains: vec![uhost.clone()],
            denied_domains: vec![],
            ..Default::default()
        };
        let pport = start_proxy(opts(cfg)).await;
        let (line, mut tun) = connect_via_proxy(pport, &format!("{uhost}:{uport}")).await;
        assert!(line.contains("200"), "expected 200, got {line}");
        tun.write_all(b"pong").await.unwrap();
        let mut buf = [0u8; 4];
        tun.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"pong");
    }

    #[tokio::test]
    async fn denied_connect_with_proxy_auth_records_violation_line() {
        let store = Arc::new(SandboxViolationStore::new());
        let token = "tok_123";
        let options = Arc::new(ProxyOptions {
            config: shared_network_config(NetworkConfig::default()),
            parent_proxy: None,
            filter_request: None,
            mitm_ca: None,
            tls_terminate_upstream_ca: None,
            ask: None,
            proxy_auth_token: Some(token.to_string()),
            violation_store: Some(Arc::clone(&store)),
        });
        let pport = start_proxy(options).await;
        let line = connect_via_proxy_with_auth(pport, "denied.example:443", "echo hi", token).await;
        assert!(line.contains("403"), "expected 403, got {line}");
        assert_eq!(
            store.get_violation_lines_for_command("echo hi"),
            vec!["deny network-outbound denied.example:443 (host is not on the allow list)"]
        );
    }
}
