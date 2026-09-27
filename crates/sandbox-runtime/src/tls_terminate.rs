//! In-process TLS termination for HTTPS traffic through the forward proxy.
//!
//! Faithful 1:1 port of `tls-terminate-proxy.js`. When a [`MitmCa`] is
//! configured, the forward proxy hands `CONNECT` requests here instead of
//! opening an opaque byte tunnel. We terminate the client's TLS with a per-host
//! leaf cert (see [`crate::mitm_leaf`]), parse the decrypted stream as HTTP/1.1,
//! and re-issue each request upstream over a *real* TLS connection. The optional
//! `filter_request` callback runs on each parsed request before it is forwarded.
//!
//! SECURITY-CRITICAL: this is the actual TLS interception path. The leaf served
//! to the client is the minted one (P6a, signed by the configured MITM CA); the
//! upstream re-issue verifies the *origin's* real certificate against the system
//! trust store (plus an optional `upstream_ca`) — verification is **never**
//! disabled.
//!
//! ## Divergences from the TS reference (flagged)
//!
//! - **No unix-socket loopback.** The TS stands up a short-lived `https.Server`
//!   on a unix socket and pipes the client socket through it (a workaround for
//!   Bun's missing `emit('connection', socket)`). Rust has no such constraint:
//!   we feed the (head-prepended) client stream straight into a
//!   [`tokio_rustls::TlsAcceptor`] and serve HTTP/1.1 with hyper directly. Same
//!   observable behavior, no temp socket / `unlink` bookkeeping.
//! - **No global-agent quirk.** The TS sets `agent: false` to dodge Node/Bun
//!   connection-pool + cached-`ca` quirks. We open a fresh `tokio-rustls` client
//!   connection per request and never pool, which is the faithful intent.
//! - **WebSocket / upgrade over TLS is out of scope** (the TS refuses it). A
//!   decrypted `Upgrade` request is forwarded like any other request; the
//!   upstream's `101` (if any) is relayed as a normal response without splicing
//!   the raw bytes. This matches the TS "refused / not supported" stance.

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio_rustls::{TlsAcceptor, TlsConnector};

use crate::mitm_ca::MitmCa;
use crate::mitm_leaf::MitmCertResolver;
use crate::parent_proxy::strip_hop_by_hop;
use crate::request_filter::{decide_and_respond, FilterOutcome, FilterRequestFn};

/// Where a terminated CONNECT is re-issued: the originally-requested host:port
/// plus an optional extra CA to trust for the upstream leg (`target.upstreamCA`).
#[derive(Clone)]
pub struct TlsTarget {
    /// The originally-requested origin hostname (or IP literal).
    pub hostname: String,
    /// The originally-requested origin port.
    pub port: u16,
    /// Extra CA certificate(s) (DER) to trust for the upstream TLS handshake, in
    /// addition to the system roots. `None` ⇒ system roots only. Mirrors the TS
    /// `target.upstreamCA`.
    pub upstream_ca: Option<Arc<Vec<rustls::pki_types::CertificateDer<'static>>>>,
}

impl std::fmt::Debug for TlsTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsTarget")
            .field("hostname", &self.hostname)
            .field("port", &self.port)
            .field("upstream_ca", &self.upstream_ca.as_ref().map(|c| c.len()))
            .finish()
    }
}

/// The boxed response body produced by the terminator's HTTP/1.1 service.
type ProxyBody = BoxBody<Bytes, hyper::Error>;

/// True if `buf` starts with a TLS Handshake record header.
///
/// Three bytes: content type `0x16` (Handshake) + `legacy_record_version`
/// `0x03,0x00–0x03`. RFC 8446 §5.1 froze the record-layer version (TLS 1.3+
/// negotiate via the `supported_versions` extension; the wire header stays
/// ≤`0x0303`), so this holds for current and future TLS. Faithful to the TS
/// `looksLikeClientHello`.
///
/// Routing heuristic, not a security check: a non-TLS stream that happens to
/// start `16 03 0x` is handed to the TLS server, which then rejects it properly.
#[must_use]
pub fn looks_like_client_hello(buf: &[u8]) -> bool {
    buf.len() >= 3 && buf[0] == 0x16 && buf[1] == 0x03 && buf[2] <= 0x03
}

/// Wait for the client's first post-`CONNECT` bytes and report whether they look
/// like a TLS `ClientHello`. The caller must already have written the
/// `200 Connection Established` line — clients don't send until they see it.
///
/// `already` carries any bytes the caller already consumed (e.g. an early
/// payload delivered with the CONNECT). If it is already ≥3 bytes we decide
/// immediately; otherwise we read from `stream` until we have ≥3 bytes (or hit
/// EOF). The consumed bytes are returned as `.1` so the caller can replay them
/// into whichever downstream (the TLS acceptor or the opaque tunnel) it picks.
///
/// Faithful to the TS `peekForClientHello`.
pub async fn peek_client_hello<S>(stream: &mut S, already: Vec<u8>) -> (bool, Vec<u8>)
where
    S: AsyncRead + Unpin,
{
    let mut buf = already;
    if buf.len() >= 3 {
        return (looks_like_client_hello(&buf), buf);
    }
    let mut chunk = [0_u8; 1024];
    loop {
        // EOF (`Ok(0)`) or a read error both end the peek; we decide on whatever
        // bytes we have so far (≥3 ⇒ a verdict, <3 ⇒ not-TLS).
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        if buf.len() >= 3 {
            break;
        }
    }
    (looks_like_client_hello(&buf), buf)
}

/// A reader/writer that replays `head` (the sniffed `ClientHello` bytes) before
/// the live `inner` stream, so the TLS acceptor sees the complete handshake.
///
/// Reads drain `head` first, then delegate to `inner`. Writes/flush/shutdown go
/// straight to `inner` (the server only writes *after* the handshake, by which
/// point `head` is fully drained).
struct PrependReader<S> {
    head: Bytes,
    pos: usize,
    inner: S,
}

impl<S> PrependReader<S> {
    fn new(head: Vec<u8>, inner: S) -> Self {
        Self {
            head: Bytes::from(head),
            pos: 0,
            inner,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for PrependReader<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.pos < self.head.len() {
            let remaining = &self.head[self.pos..];
            let n = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..n]);
            self.pos += n;
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PrependReader<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Terminate the client's TLS on `client_stream`, parse the decrypted HTTP/1.1
/// stream, and forward each request to `target` over a fresh upstream TLS
/// connection.
///
/// Preconditions: the caller has already validated `target` against the domain
/// allowlist and written `200 Connection Established`; `head` carries whatever
/// the `ClientHello` sniff consumed (replayed into the TLS acceptor).
///
/// - The server-side config uses a [`MitmCertResolver`] (per-host leaf minted
///   from the SNI, default = `target.hostname`) and advertises `http/1.1` ALPN.
/// - Each decrypted request runs `filter_request` (when set) via
///   [`decide_and_respond`] against the absolute URL `https://<host><path>`,
///   where `<host>` is the request `Host` header, or `hostname[:port]` when the
///   port is not 443. A deny yields the byte-exact 403; an allow re-issues
///   upstream.
/// - The upstream re-issue strips hop-by-hop headers, **drops the `Host`
///   header** (the client derives it from `{host, port}` — correct SAN
///   verification), and dials a fresh `tokio-rustls` client to `hostname:port`
///   with the system roots (+ `upstream_ca` if set) and SNI = `hostname` (skipped
///   for IP literals). No connection pool. Upstream failure → `502 Bad Gateway`.
///
/// # Errors
///
/// Returns an error if the TLS handshake with the client fails or the decrypted
/// HTTP/1.1 connection ends abnormally. (Per-request upstream failures are
/// folded into a relayed `502`, not returned here.)
pub async fn terminate_and_forward<S>(
    ca: Arc<MitmCa>,
    filter_request: Option<FilterRequestFn>,
    client_stream: S,
    head: Vec<u8>,
    target: TlsTarget,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // Server TLS config: SNI cert resolver (default host = the CONNECT target),
    // http/1.1 ALPN only (we do not terminate HTTP/2 — clients negotiate down).
    let resolver = Arc::new(MitmCertResolver::new(
        Arc::clone(&ca),
        target.hostname.clone(),
    ));
    let mut server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    server_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(server_config));

    let prepended = PrependReader::new(head, client_stream);
    let tls_stream = acceptor.accept(prepended).await.map_err(|err| {
        tracing::error!(
            target: "tls_terminate",
            "[tls-terminate] client TLS error for {}: {err}",
            target.hostname
        );
        err
    })?;

    let target = Arc::new(target);
    let filter_request = filter_request.map(Arc::new);

    let io = TokioIo::new(tls_stream);
    let service = service_fn(move |req: Request<Incoming>| {
        let target = Arc::clone(&target);
        let filter_request = filter_request.clone();
        async move {
            Ok::<_, hyper::Error>(handle_request(req, &target, filter_request.as_deref()).await)
        }
    });

    if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
        tracing::debug!(target: "tls_terminate", "[tls-terminate] decrypted connection ended: {err}");
    }
    Ok(())
}

/// Handle one decrypted HTTP/1.1 request: run the filter, then re-issue upstream.
async fn handle_request(
    req: Request<Incoming>,
    target: &TlsTarget,
    filter_request: Option<&FilterRequestFn>,
) -> Response<ProxyBody> {
    // The decrypted request is origin-form: the path-and-query is the URI.
    let path = req
        .uri()
        .path_and_query()
        .map_or("/", http::uri::PathAndQuery::as_str)
        .to_string();
    let method = req.method().clone();

    // Absolute URL for the filter: Host header, else hostname[:port] (port≠443).
    let host = req
        .headers()
        .get(http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map_or_else(
            || {
                if target.port == 443 {
                    target.hostname.clone()
                } else {
                    format!("{}:{}", target.hostname, target.port)
                }
            },
            ToString::to_string,
        );
    let abs_url = format!("https://{host}{path}");

    // Snapshot the forward headers (hop-by-hop stripped, Host dropped) BEFORE
    // consuming the request into the filter (which needs the headers + body).
    let in_headers: Vec<(String, String)> = req
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let mut fwd_headers = strip_hop_by_hop(&in_headers);
    fwd_headers.retain(|(k, _)| !k.eq_ignore_ascii_case("host"));

    let (parts, body) = req.into_parts();

    // Run the per-request filter. On deny → byte-exact 403; on allow → the
    // (buffered) body to forward upstream.
    let body_to_forward: Full<Bytes> = if let Some(filter) = filter_request {
        match decide_and_respond(filter, &abs_url, &method, &parts.headers, body).await {
            FilterOutcome::Allow(b) => b,
            FilterOutcome::Deny(resp) => {
                let (p, b) = resp.into_parts();
                return Response::from_parts(p, b.map_err(|never| match never {}).boxed());
            }
        }
    } else {
        match body.collect().await {
            Ok(c) => Full::new(c.to_bytes()),
            Err(_) => return bad_gateway(),
        }
    };

    forward_upstream(target, &method, &path, fwd_headers, body_to_forward).await
}

/// Re-issue a single decrypted request to the origin over a fresh real-TLS
/// connection, relaying the response (hop-by-hop stripped). Upstream error → 502.
async fn forward_upstream(
    target: &TlsTarget,
    method: &http::Method,
    path: &str,
    fwd_headers: Vec<(String, String)>,
    body: Full<Bytes>,
) -> Response<ProxyBody> {
    let Ok(connector) = upstream_connector(target.upstream_ca.as_deref().map(Vec::as_slice)) else {
        return bad_gateway();
    };

    // Fresh TCP + TLS to the origin (no pool — a proxy's outbound leg must not
    // share a connection pool keyed on the proxy process).
    let Ok(tcp) = tokio::net::TcpStream::connect((target.hostname.as_str(), target.port)).await
    else {
        tracing::error!(
            target: "tls_terminate",
            "[tls-terminate] upstream {}:{} TCP connect failed",
            target.hostname, target.port
        );
        return bad_gateway();
    };

    // SNI = the host the client intended; skip for IP literals (SNI cannot carry
    // an IP). For an IP literal we still must hand rustls *some* ServerName, so
    // we pass the IP — rustls verifies the cert against the IP SAN.
    let Ok(server_name) = server_name_for(&target.hostname) else {
        return bad_gateway();
    };

    let Ok(tls) = connector.connect(server_name, tcp).await else {
        tracing::error!(
            target: "tls_terminate",
            "[tls-terminate] upstream {}:{} TLS handshake failed",
            target.hostname, target.port
        );
        return bad_gateway();
    };

    let io = TokioIo::new(tls);
    let Ok((mut sender, conn)) = hyper::client::conn::http1::handshake(io).await else {
        return bad_gateway();
    };
    tokio::spawn(async move {
        if let Err(err) = conn.await {
            tracing::debug!(target: "tls_terminate", "[tls-terminate] upstream conn ended: {err}");
        }
    });

    let mut builder = Request::builder().method(method.clone()).uri(path);
    for (k, v) in fwd_headers {
        builder = builder.header(k, v);
    }
    let Ok(upstream_req) = builder.body(body) else {
        return bad_gateway();
    };

    let Ok(resp) = sender.send_request(upstream_req).await else {
        tracing::error!(
            target: "tls_terminate",
            "[tls-terminate] upstream {}:{} request failed",
            target.hostname, target.port
        );
        return bad_gateway();
    };

    // Relay: status + hop-by-hop-stripped headers + streamed body.
    let (parts, resp_body) = resp.into_parts();
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
    out.body(resp_body.boxed())
        .unwrap_or_else(|_| bad_gateway())
}

/// Build the SNI [`ServerName`] for an upstream handshake. An IP literal is
/// passed as an `IpAddress` server name (rustls verifies against the IP SAN); a
/// DNS host is passed as a `DnsName`.
fn server_name_for(hostname: &str) -> Result<ServerName<'static>, ()> {
    if let Ok(ip) = hostname.parse::<std::net::IpAddr>() {
        return Ok(ServerName::IpAddress(ip.into()));
    }
    ServerName::try_from(hostname.to_string()).map_err(|err| {
        tracing::error!(target: "tls_terminate", "[tls-terminate] invalid upstream SNI {hostname:?}: {err}");
    })
}

/// Build a `tokio-rustls` client connector trusting the system roots plus any
/// `upstream_ca` certs. Verification is **never** disabled — the origin's real
/// certificate is checked against this trust store. ALPN advertises `http/1.1`.
///
/// # Errors
///
/// Returns an error if no usable roots could be assembled (so the caller fails
/// the request rather than proceeding with an empty trust store).
fn upstream_connector(
    upstream_ca: Option<&[rustls::pki_types::CertificateDer<'static>]>,
) -> Result<TlsConnector, ()> {
    let mut roots = rustls::RootCertStore::empty();

    // System roots (rustls-native-certs: the OS trust store + NODE_EXTRA_CA_CERTS
    // is not consulted here — the upstream_ca channel is the explicit extra-trust
    // path, mirroring the TS `ca:` option).
    let native = rustls_native_certs::load_native_certs();
    if !native.errors.is_empty() {
        tracing::warn!(
            target: "tls_terminate",
            "[tls-terminate] some native roots failed to load: {:?}", native.errors
        );
    }
    for cert in native.certs {
        // Ignore individual malformed roots; a partial system store is fine.
        let _ = roots.add(cert);
    }

    if let Some(extra) = upstream_ca {
        for cert in extra {
            roots.add(cert.clone()).map_err(|err| {
                tracing::error!(target: "tls_terminate", "[tls-terminate] bad upstream_ca cert: {err}");
            })?;
        }
    }

    if roots.is_empty() {
        tracing::error!(target: "tls_terminate", "[tls-terminate] empty upstream trust store");
        return Err(());
    }

    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsConnector::from(Arc::new(config)))
}

/// `502 Bad Gateway` with the byte-exact body the TS reference sends.
fn bad_gateway() -> Response<ProxyBody> {
    Response::builder()
        .status(StatusCode::BAD_GATEWAY)
        .header(http::header::CONTENT_TYPE, "text/plain")
        .body(
            Full::new(Bytes::from_static(b"Bad Gateway"))
                .map_err(|never| match never {})
                .boxed(),
        )
        .expect("static 502 response is always valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_hello_predicate() {
        // Real `ClientHello` record header prefix.
        assert!(looks_like_client_hello(&[0x16, 0x03, 0x01, 0x00, 0x05]));
        assert!(looks_like_client_hello(&[0x16, 0x03, 0x03]));
        assert!(looks_like_client_hello(&[0x16, 0x03, 0x00]));
        // legacy_record_version byte > 0x03 → not TLS.
        assert!(!looks_like_client_hello(&[0x16, 0x03, 0x04]));
        // Plain HTTP.
        assert!(!looks_like_client_hello(b"GET "));
        // Too short.
        assert!(!looks_like_client_hello(&[0x16, 0x03]));
        assert!(!looks_like_client_hello(&[]));
    }

    #[tokio::test]
    async fn peek_decides_immediately_when_head_long_enough() {
        let mut empty = tokio::io::empty();
        let (is_tls, head) = peek_client_hello(&mut empty, vec![0x16, 0x03, 0x01, 0xAA]).await;
        assert!(is_tls);
        assert_eq!(head, vec![0x16, 0x03, 0x01, 0xAA]);
    }

    #[tokio::test]
    async fn peek_reads_more_bytes_when_head_short() {
        // head has 1 byte; the stream supplies the rest of a `ClientHello` prefix.
        let stream = std::io::Cursor::new(vec![0x03, 0x01, 0x00]);
        let mut stream = tokio::io::BufReader::new(stream);
        let (is_tls, head) = peek_client_hello(&mut stream, vec![0x16]).await;
        assert!(is_tls);
        assert_eq!(&head[..3], &[0x16, 0x03, 0x01]);
    }

    #[tokio::test]
    async fn peek_non_tls_first_bytes() {
        let stream = std::io::Cursor::new(b"ET / HTTP/1.1".to_vec());
        let mut stream = tokio::io::BufReader::new(stream);
        let (is_tls, head) = peek_client_hello(&mut stream, b"G".to_vec()).await;
        assert!(!is_tls);
        assert_eq!(&head[..1], b"G");
    }

    // ─────────── integration: the security proof (real in-process TLS) ───────────

    use crate::mitm_ca::{create_mitm_ca, dispose_mitm_ca, MitmCaOptions};
    use crate::request_filter::{Decision, FilterRequest};
    use http_body_util::Empty;
    use hyper::body::Incoming;
    use rustls::pki_types::CertificateDer;
    use std::future::Future;
    use std::pin::Pin;
    use tokio::net::{TcpListener, TcpStream};

    /// A stand-in HTTPS origin's `ServerConfig` for `host`, plus its CA DER (for
    /// `upstream_ca`).
    ///
    /// Builds a proper 2-level chain: a self-signed CA (the trust anchor returned
    /// for `upstream_ca`) signing a leaf end-entity cert carrying `SAN=host`. A
    /// single self-signed cert can't be both anchor and end-entity (webpki rejects
    /// `CaUsedAsEndEntity`), so the server presents the leaf (+ chain) and the
    /// client trusts the CA.
    fn self_signed_origin(host: &str) -> (rustls::ServerConfig, CertificateDer<'static>) {
        // Origin CA.
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "test-origin-ca");
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();
        let ca_der = CertificateDer::from(ca_cert.der().to_vec());

        // Leaf signed by the origin CA, SAN = host.
        let leaf_key = rcgen::KeyPair::generate().unwrap();
        let leaf_params = rcgen::CertificateParams::new(vec![host.to_string()]).unwrap();
        let leaf_cert = leaf_params.signed_by(&leaf_key, &ca_cert, &ca_key).unwrap();
        let leaf_der = CertificateDer::from(leaf_cert.der().to_vec());
        let leaf_key_der =
            rustls::pki_types::PrivateKeyDer::try_from(leaf_key.serialize_der()).unwrap();

        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![leaf_der, ca_der.clone()], leaf_key_der)
            .unwrap();
        (config, ca_der)
    }

    /// Stand up a real HTTPS origin (tokio-rustls) on loopback returning 200
    /// "ok". Returns (host, port, its self-signed cert DER for trust).
    async fn https_origin(host: &str) -> (String, u16, CertificateDer<'static>) {
        let (mut config, cert_der) = self_signed_origin(host);
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
        (host.to_string(), port, cert_der)
    }

    /// A tokio-rustls client trusting only `ca_der`, connecting to `stream` with
    /// SNI=`host`, sending `GET path` and returning (`status_line`, `body`, `leaf_sans`).
    async fn drive_mitm_client(
        stream: TcpStream,
        ca_der: CertificateDer<'static>,
        host: &str,
        path: &str,
    ) -> (u16, String, Vec<String>) {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ca_der).unwrap();
        let mut config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let connector = TlsConnector::from(Arc::new(config));
        let server_name = ServerName::try_from(host.to_string()).unwrap();
        let tls = connector.connect(server_name, stream).await.unwrap();

        // Capture the leaf SANs the client was served (peer certs).
        let leaf_sans = {
            let (_, conn) = tls.get_ref();
            let mut sans = Vec::new();
            if let Some(certs) = conn.peer_certificates() {
                if let Some(leaf) = certs.first() {
                    use x509_parser::prelude::FromDer as _;
                    let parsed = x509_parser::certificate::X509Certificate::from_der(leaf.as_ref())
                        .unwrap()
                        .1;
                    if let Ok(Some(san)) = parsed.subject_alternative_name() {
                        for gn in &san.value.general_names {
                            match gn {
                                x509_parser::extensions::GeneralName::DNSName(n) => {
                                    sans.push((*n).to_string());
                                }
                                x509_parser::extensions::GeneralName::IPAddress(octets)
                                    if octets.len() == 4 =>
                                {
                                    sans.push(format!(
                                        "{}.{}.{}.{}",
                                        octets[0], octets[1], octets[2], octets[3]
                                    ));
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
            sans
        };

        let io = TokioIo::new(tls);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let req = Request::builder()
            .method(http::Method::GET)
            .uri(path)
            .header(http::header::HOST, host)
            .body(Empty::<Bytes>::new())
            .unwrap();
        let resp = sender.send_request(req).await.unwrap();
        let status = resp.status().as_u16();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            String::from_utf8_lossy(&body).into_owned(),
            leaf_sans,
        )
    }

    /// Extract the first certificate (the CA) from a PEM string as DER.
    fn first_cert_der(pem: &str) -> CertificateDer<'static> {
        let mut rd = std::io::BufReader::new(pem.as_bytes());
        let der = rustls_pemfile::certs(&mut rd).next().unwrap().unwrap();
        CertificateDer::from(der.to_vec())
    }

    fn deny_path_filter(deny: &'static str) -> FilterRequestFn {
        Arc::new(move |req: FilterRequest| {
            let denied = req.url.contains(deny);
            Box::pin(async move {
                if denied {
                    Decision::Deny {
                        reason: Some("blocked".to_string()),
                    }
                } else {
                    Decision::Allow
                }
            }) as Pin<Box<dyn Future<Output = Decision> + Send>>
        })
    }

    /// THE security proof: a CA-trusting client's HTTPS request is terminated +
    /// forwarded to the stand-in origin and the "ok" body round-trips; the leaf
    /// the client sees has a SAN matching the host.
    #[tokio::test]
    async fn mitm_round_trips_origin_response() {
        // Host = the loopback IP so the upstream TCP connect (to target.hostname)
        // resolves in-process; cert SANs are IP:127.0.0.1 end-to-end (origin leaf,
        // minted MITM leaf, and the client's SNI/verification).
        let host = "127.0.0.1";
        let (ohost, oport, origin_cert) = https_origin(host).await;
        let ca = Arc::new(create_mitm_ca(MitmCaOptions::default()).unwrap());
        let ca_der = first_cert_der(&ca.cert_pem);

        // Wire the client ⇄ terminator over an in-memory TCP pair on loopback.
        let pair = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let pair_port = pair.local_addr().unwrap().port();
        let target = TlsTarget {
            hostname: ohost.clone(),
            port: oport,
            upstream_ca: Some(Arc::new(vec![origin_cert])),
        };
        let ca2 = Arc::clone(&ca);
        tokio::spawn(async move {
            let (server_side, _) = pair.accept().await.unwrap();
            terminate_and_forward(ca2, None, server_side, Vec::new(), target)
                .await
                .unwrap();
        });
        let client_side = TcpStream::connect(("127.0.0.1", pair_port)).await.unwrap();

        let (status, body, sans) = drive_mitm_client(client_side, ca_der, host, "/hello").await;
        assert_eq!(status, 200);
        assert_eq!(body, "ok", "MITM must round-trip the origin response");
        assert!(
            sans.contains(&host.to_string()),
            "leaf SAN must match host: {sans:?}"
        );
        dispose_mitm_ca(&ca);
    }

    /// A filter_request that denies the path → the CA-trusting client gets the
    /// byte-exact 403 (`X-Proxy-Error: blocked-by-sandbox-runtime`).
    #[tokio::test]
    async fn mitm_filter_denies_with_byte_exact_403() {
        let host = "127.0.0.1";
        let (ohost, oport, origin_cert) = https_origin(host).await;
        let ca = Arc::new(create_mitm_ca(MitmCaOptions::default()).unwrap());
        let ca_der = first_cert_der(&ca.cert_pem);

        let pair = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let pair_port = pair.local_addr().unwrap().port();
        let target = TlsTarget {
            hostname: ohost.clone(),
            port: oport,
            upstream_ca: Some(Arc::new(vec![origin_cert])),
        };
        let ca2 = Arc::clone(&ca);
        tokio::spawn(async move {
            let (server_side, _) = pair.accept().await.unwrap();
            let filter = Some(deny_path_filter("/blocked"));
            terminate_and_forward(ca2, filter, server_side, Vec::new(), target)
                .await
                .unwrap();
        });
        let client_side = TcpStream::connect(("127.0.0.1", pair_port)).await.unwrap();

        // Use a raw read so we can assert the byte-exact header + body.
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ca_der).unwrap();
        let mut config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let connector = TlsConnector::from(Arc::new(config));
        let server_name = ServerName::try_from(host.to_string()).unwrap();
        let tls = connector.connect(server_name, client_side).await.unwrap();
        let io = TokioIo::new(tls);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let req = Request::builder()
            .method(http::Method::GET)
            .uri("/blocked")
            .header(http::header::HOST, host)
            .body(Empty::<Bytes>::new())
            .unwrap();
        let resp = sender.send_request(req).await.unwrap();
        assert_eq!(resp.status().as_u16(), 403);
        assert_eq!(
            resp.headers().get("X-Proxy-Error").unwrap(),
            "blocked-by-sandbox-runtime"
        );
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body, Bytes::from_static(b"blocked\n"));
        dispose_mitm_ca(&ca);
    }
}
