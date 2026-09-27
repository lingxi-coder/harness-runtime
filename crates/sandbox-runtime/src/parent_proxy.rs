//! Parent/upstream proxy resolution + `NO_PROXY` bypass (pure port of
//! `parent-proxy.js:45-212`). Reads env via an injected map so the resolver
//! stays pure/testable. CIDR matching via `ipnet`.

use std::collections::HashMap;
use std::hash::BuildHasher;
use std::net::Ipv6Addr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use base64::Engine as _;
use ipnet::IpNet;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use url::Url;

/// Parsed `NO_PROXY` ruleset (`parseNoProxy`, parent-proxy.js:86-148).
#[derive(Debug, Clone, Default)]
pub struct NoProxy {
    /// `NO_PROXY=*` — bypass everything.
    pub all: bool,
    /// Hostname suffixes (normalized: lowercased, `*.`/`:port` stripped).
    pub suffixes: Vec<String>,
    /// CIDR subnets + exact IP literals (stored as /32 or /128).
    pub cidr: Vec<IpNet>,
}

/// Resolved parent-proxy config (`resolveParentProxy` result).
#[derive(Debug, Clone)]
pub struct ResolvedParentProxy {
    /// Proxy URL for plain HTTP destinations (`None` ⇒ direct).
    pub http_url: Option<Url>,
    /// Proxy URL for HTTPS destinations (falls back to `HTTP_PROXY`).
    pub https_url: Option<Url>,
    /// `NO_PROXY` bypass ruleset.
    pub no_proxy: NoProxy,
}

/// Explicit config overrides (mirrors the TS `cfg` arg / `ParentProxyConfigSchema`,
/// `sandbox-config.js:73-89`). `None` fields fall back to env. Serializes
/// camelCase (`noProxy`) so it can be a field of the umbrella config.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParentProxyConfig {
    /// Override for `HTTP_PROXY`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<String>,
    /// Override for `HTTPS_PROXY`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub https: Option<String>,
    /// Override for `NO_PROXY`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_proxy: Option<String>,
}

fn env_first<'a, S: BuildHasher>(
    env: &'a HashMap<String, String, S>,
    keys: &[&str],
) -> Option<&'a str> {
    keys.iter().find_map(|k| env.get(*k).map(String::as_str))
}

/// Parse a proxy URL, accepting schemeless `host:port` (curl-style), rejecting
/// any non-http/https scheme or empty host. (`resolveParentProxy`'s `parse`.)
fn parse_proxy_url(u: &str) -> Option<Url> {
    let has_scheme = u.split_once("://").is_some_and(|(s, _)| {
        let mut cs = s.chars();
        cs.next().is_some_and(|c| c.is_ascii_alphabetic())
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
    });
    let with_scheme = if has_scheme {
        u.to_string()
    } else {
        format!("http://{u}")
    };
    let parsed = Url::parse(&with_scheme).ok()?;
    if (parsed.scheme() != "http" && parsed.scheme() != "https") || parsed.host_str().is_none() {
        return None;
    }
    Some(parsed)
}

/// `resolveParentProxy` (parent-proxy.js:45-84). `None` if neither HTTP nor
/// HTTPS proxy is configured (after parsing).
#[must_use]
#[allow(clippy::similar_names)] // `http`/`https`/`http_url`/`https_url` mirror the env keys.
pub fn resolve_parent_proxy<S: BuildHasher>(
    cfg: Option<&ParentProxyConfig>,
    env: &HashMap<String, String, S>,
) -> Option<ResolvedParentProxy> {
    let http = cfg
        .and_then(|c| c.http.clone())
        .or_else(|| env_first(env, &["HTTP_PROXY", "http_proxy"]).map(String::from));
    let https = cfg
        .and_then(|c| c.https.clone())
        .or_else(|| env_first(env, &["HTTPS_PROXY", "https_proxy"]).map(String::from))
        .or_else(|| http.clone()); // HTTPS falls back to HTTP_PROXY (curl behaviour)
    let no_proxy_raw = cfg
        .and_then(|c| c.no_proxy.clone())
        .or_else(|| env_first(env, &["NO_PROXY", "no_proxy"]).map(String::from))
        .unwrap_or_default();
    if http.is_none() && https.is_none() {
        return None;
    }
    let http_url = http.as_deref().and_then(parse_proxy_url);
    let https_url = https.as_deref().and_then(parse_proxy_url);
    if http_url.is_none() && https_url.is_none() {
        return None;
    }
    Some(ResolvedParentProxy {
        http_url,
        https_url,
        no_proxy: parse_no_proxy(&no_proxy_raw),
    })
}

/// `parseNoProxy` (parent-proxy.js:86-148).
#[must_use]
pub fn parse_no_proxy(raw: &str) -> NoProxy {
    let mut rules = NoProxy::default();
    for entry in raw.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        if entry == "*" {
            rules.all = true;
            continue;
        }
        if entry.contains('/') {
            // CIDR (ignore malformed; never fall through to suffix).
            if let Ok(net) = entry.parse::<IpNet>() {
                rules.cidr.push(net);
            }
            continue;
        }
        let mut v = entry.to_ascii_lowercase();
        // `[v6]:port` → v6
        if let Some(inner) = v.strip_prefix('[').and_then(|s| s.split(']').next()) {
            v = inner.to_string();
        }
        if let Some(stripped) = v.strip_prefix("*.") {
            v = format!(".{stripped}"); // TS slices off the `*`, leaving `.suffix`
        }
        if let Ok(ip) = v.parse::<std::net::IpAddr>() {
            // Bare IP literal → exact /32 or /128.
            let net = match ip {
                std::net::IpAddr::V4(a) => IpNet::from(
                    ipnet::Ipv4Net::new(a, 32).expect("/32 prefix length is always valid"),
                ),
                std::net::IpAddr::V6(a) => IpNet::from(
                    ipnet::Ipv6Net::new(a, 128).expect("/128 prefix length is always valid"),
                ),
            };
            rules.cidr.push(net);
            continue;
        }
        // Strip a trailing `:port` (non-IP only).
        if let Some(colon) = v.rfind(':') {
            if v[colon + 1..].chars().all(|c| c.is_ascii_digit()) && colon + 1 < v.len() {
                v.truncate(colon);
            }
        }
        rules.suffixes.push(v);
    }
    rules
}

/// `shouldBypassParentProxy` (parent-proxy.js:160-188). Loopback always
/// bypasses; then `*`, CIDR, then golang-suffix semantics.
#[must_use]
pub fn should_bypass_parent_proxy(resolved: &ResolvedParentProxy, host: &str) -> bool {
    let lowered = host.to_ascii_lowercase();
    let h = crate::host::strip_brackets(lowered.trim_end_matches('.'));
    if h == "localhost" {
        return true;
    }
    if let Ok(ip) = h.parse::<std::net::IpAddr>() {
        if is_loopback(ip) {
            return true;
        }
        if resolved.no_proxy.all {
            return true;
        }
        if resolved.no_proxy.cidr.iter().any(|net| net.contains(&ip)) {
            return true;
        }
        // IP host: suffix rules don't apply.
        return false;
    }
    if resolved.no_proxy.all {
        return true;
    }
    for v in &resolved.no_proxy.suffixes {
        if let Some(bare) = v.strip_prefix('.') {
            // `.example.com` matches `foo.example.com` AND `example.com`
            if h == bare || h.ends_with(v.as_str()) {
                return true;
            }
        } else {
            // `example.com` matches `example.com` AND `foo.example.com`
            if &h == v || h.ends_with(&format!(".{v}")) {
                return true;
            }
        }
    }
    false
}

/// Loopback set (`LOOPBACK`, parent-proxy.js:190-196): 127/8 + `::1` + v4-mapped.
fn is_loopback(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(a) => a.octets()[0] == 127,
        std::net::IpAddr::V6(a) => {
            a == std::net::Ipv6Addr::LOCALHOST
                || a.to_ipv4_mapped().is_some_and(|v4| v4.octets()[0] == 127)
        }
    }
}

/// `selectParentProxyUrl` (parent-proxy.js:201-209): HTTPS prefers
/// `https_url` then `http_url`; plain HTTP only uses `http_url`.
#[must_use]
pub fn select_parent_proxy_url(resolved: &ResolvedParentProxy, is_https: bool) -> Option<&Url> {
    if is_https {
        resolved.https_url.as_ref().or(resolved.http_url.as_ref())
    } else {
        resolved.http_url.as_ref()
    }
}

/// Cap on the CONNECT response header we will buffer before giving up on a
/// misbehaving proxy (`openConnectTunnel`'s 16 KiB limit, parent-proxy.js:253).
const CONNECT_HEADER_CAP: usize = 16 * 1024;

/// Hop-by-hop / proxy-specific headers stripped before forwarding upstream
/// (`HOP_BY_HOP`, parent-proxy.js:28-38). All entries are lowercase.
const HOP_BY_HOP: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// `stripHopByHop` (parent-proxy.js:326-342): drop hop-by-hop headers plus any
/// header named in the incoming `Connection` token list (RFC 7230 §6.1).
#[must_use]
pub fn strip_hop_by_hop(headers: &[(String, String)]) -> Vec<(String, String)> {
    let mut extra: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("connection") {
            for tok in v.split(',') {
                extra.insert(tok.trim().to_ascii_lowercase());
            }
        }
    }
    headers
        .iter()
        .filter(|(k, _)| {
            let lk = k.to_ascii_lowercase();
            !HOP_BY_HOP.contains(&lk.as_str()) && !extra.contains(&lk)
        })
        .cloned()
        .collect()
}

/// `proxyAuthHeader` (parent-proxy.js:307-320): `Basic base64(user:pass)` from
/// the proxy URL userinfo, percent-decoded; `None` if no credentials. Mirrors
/// the TS try/catch: malformed percent-encoding falls back to the raw value.
#[must_use]
pub fn proxy_auth_header(proxy_url: &Url) -> Option<String> {
    let user = proxy_url.username();
    let pass = proxy_url.password().unwrap_or("");
    if user.is_empty() && pass.is_empty() {
        return None;
    }
    let dec = |s: &str| {
        percent_encoding::percent_decode_str(s)
            .decode_utf8()
            .map_or_else(|_| s.to_string(), std::borrow::Cow::into_owned)
    };
    let creds = format!("{}:{}", dec(user), dec(pass));
    Some(format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(creds.as_bytes())
    ))
}

/// `redactUrl` (parent-proxy.js:347-355): replace userinfo with `***:***` for
/// safe logging; `"-"` for `None`.
#[must_use]
pub fn redact_url(u: Option<&Url>) -> String {
    let Some(u) = u else {
        return "-".to_string();
    };
    if u.username().is_empty() && u.password().is_none_or(str::is_empty) {
        return u.as_str().to_string();
    }
    let mut c = u.clone();
    let _ = c.set_username("***");
    let _ = c.set_password(Some("***"));
    c.as_str().to_string()
}

/// A tunnelled stream that first replays the bytes received after the CONNECT
/// response header terminator (the TS `sock.unshift(rest)`), then delegates to
/// the inner transport. Generic over the transport so a plain `TcpStream` or a
/// `tokio_rustls::client::TlsStream<TcpStream>` can be tunnelled identically.
pub struct TunnelStream<S> {
    inner: S,
    leftover: Vec<u8>,
    leftover_pos: usize,
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for TunnelStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.leftover_pos < this.leftover.len() {
            let remaining = &this.leftover[this.leftover_pos..];
            let n = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..n]);
            this.leftover_pos += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for TunnelStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// `openConnectTunnel` (parent-proxy.js:214-279): over an already-connected
/// proxy transport `sock`, send `CONNECT host:port`, await a 2xx status line,
/// and return a [`TunnelStream`] with any post-header bytes preserved at the
/// front (the TS `unshift` semantics).
///
/// The dial is done by the caller (cleaner than the TS dial-closure and lets
/// the parent-TLS path hand in a `TlsStream`); host/port validation, the
/// byte-exact CONNECT request, the 16 KiB header cap, and the `^HTTP/1\.[01]
/// 2\d\d` status check are faithful to the reference.
///
/// # Errors
/// Invalid host/port, non-2xx status, oversized (>16 KiB) header, or the proxy
/// closing during the handshake.
pub async fn open_connect_tunnel<S>(
    mut sock: S,
    dest_host: &str,
    dest_port: u16,
    auth_header: Option<&str>,
) -> std::io::Result<TunnelStream<S>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let bare = crate::host::strip_brackets(dest_host);
    if !crate::host::is_valid_host(&bare) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid destination host for CONNECT",
        ));
    }
    if dest_port == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid destination port",
        ));
    }
    let authority = if bare.parse::<Ipv6Addr>().is_ok() {
        format!("[{bare}]:{dest_port}")
    } else {
        format!("{bare}:{dest_port}")
    };
    let req = format!(
        "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n{}\r\n",
        auth_header.map_or_else(String::new, |a| format!("Proxy-Authorization: {a}\r\n"))
    );
    sock.write_all(req.as_bytes()).await?;
    // Read the response head up to `\r\n\r\n` (cap at 16 KiB).
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    let mut chunk = [0u8; 256];
    let end = loop {
        let n = sock.read(&mut chunk).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "proxy closed during CONNECT handshake",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > CONNECT_HEADER_CAP {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "CONNECT response header too large",
            ));
        }
    };
    let line_end = buf.iter().position(|&b| b == b'\r').unwrap_or(buf.len());
    let status_line = std::str::from_utf8(&buf[..line_end]).unwrap_or("");
    if !connect_status_is_2xx(status_line) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            format!("proxy refused CONNECT: {}", status_line.trim()),
        ));
    }
    let leftover = buf[end + 4..].to_vec();
    Ok(TunnelStream {
        inner: sock,
        leftover,
        leftover_pos: 0,
    })
}

/// `/^HTTP\/1\.[01] 2\d\d(?:\s|$)/` (parent-proxy.js:263).
fn connect_status_is_2xx(status_line: &str) -> bool {
    let Some(rest) = status_line.strip_prefix("HTTP/1.") else {
        return false;
    };
    let Some(rest) = rest.strip_prefix('0').or_else(|| rest.strip_prefix('1')) else {
        return false;
    };
    let Some(after_sp) = rest.strip_prefix(' ') else {
        return false;
    };
    let b = after_sp.as_bytes();
    // `2\d\d` followed by whitespace or end-of-string.
    b.len() >= 3
        && b[0] == b'2'
        && b[1].is_ascii_digit()
        && b[2].is_ascii_digit()
        && (b.len() == 3 || b[3].is_ascii_whitespace())
}

/// Marker for a duplex stream usable as a CONNECT tunnel transport. Blanket-
/// impl'd for every `AsyncRead + AsyncWrite + Unpin + Send`, so it can stand in
/// the `dyn` return type of [`connect_via_parent_proxy`] (a `dyn` object may
/// name at most one non-auto trait).
pub trait TunnelTransport: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> TunnelTransport for T {}

/// `connectViaParentProxy` (parent-proxy.js:285-303): dial the parent proxy
/// (TCP for `http://`, TLS for `https://`) and open a CONNECT tunnel through
/// it. The two schemes yield different transport types, so the tunnel is boxed
/// behind a trait object — Task 2's hyper handler only needs
/// `AsyncRead + AsyncWrite + Unpin + Send`.
///
/// # Errors
/// Proxy URL with no host, TCP/TLS dial failure, TLS handshake failure, or any
/// CONNECT-handshake error from [`open_connect_tunnel`].
pub async fn connect_via_parent_proxy(
    proxy_url: &Url,
    dest_host: &str,
    dest_port: u16,
) -> std::io::Result<Pin<Box<dyn TunnelTransport>>> {
    let proxy_host = crate::host::strip_brackets(proxy_url.host_str().unwrap_or(""));
    if proxy_host.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "parent proxy URL has no host",
        ));
    }
    let use_tls = proxy_url.scheme() == "https";
    let proxy_port = proxy_url.port().unwrap_or(if use_tls { 443 } else { 80 });
    let auth = proxy_auth_header(proxy_url);

    let tcp = TcpStream::connect((proxy_host.as_str(), proxy_port)).await?;
    if use_tls {
        let tls = tls_connect(tcp, &proxy_host).await?;
        let tun = open_connect_tunnel(tls, dest_host, dest_port, auth.as_deref()).await?;
        Ok(Box::pin(tun))
    } else {
        let tun = open_connect_tunnel(tcp, dest_host, dest_port, auth.as_deref()).await?;
        Ok(Box::pin(tun))
    }
}

/// TLS-handshake a connected TCP stream to a parent proxy using `tokio-rustls`
/// 0.25 with the `webpki-roots` trust anchors. SNI is the proxy host only when
/// it is not an IP literal (RFC 6066 §3).
async fn tls_connect(
    tcp: TcpStream,
    proxy_host: &str,
) -> std::io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    // RFC 6066 §3: SNI must be a hostname, never an IP literal. rustls'
    // `ServerName::try_from` already rejects IPs as DNS names, but we keep the
    // explicit IP check so an IP-literal proxy host still handshakes (with no
    // SNI) rather than erroring.
    let server_name: tokio_rustls::rustls::pki_types::ServerName<'static> =
        if proxy_host.parse::<std::net::IpAddr>().is_ok() {
            tokio_rustls::rustls::pki_types::ServerName::IpAddress(
                proxy_host
                    .parse::<std::net::IpAddr>()
                    .expect("checked is_ok above")
                    .into(),
            )
        } else {
            tokio_rustls::rustls::pki_types::ServerName::try_from(proxy_host.to_string()).map_err(
                |_| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "invalid parent proxy SNI host",
                    )
                },
            )?
        };
    connector.connect(server_name, tcp).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// Test-only CIDR membership helper.
    fn cidr_contains(r: &NoProxy, ip: &str) -> bool {
        let addr: std::net::IpAddr = ip.parse().unwrap();
        r.cidr.iter().any(|net| net.contains(&addr))
    }

    #[test]
    fn parse_no_proxy_splits_suffix_cidr_and_star() {
        let r = parse_no_proxy("*");
        assert!(r.all);
        let r =
            parse_no_proxy("example.com, .internal, 10.0.0.0/8, 127.0.0.1, host:8080, [::1]:443");
        assert!(!r.all);
        assert!(r.suffixes.contains(&"example.com".to_string()));
        assert!(r.suffixes.contains(&".internal".to_string()));
        assert!(r.suffixes.contains(&"host".to_string())); // :8080 stripped
                                                           // 10.0.0.0/8 + 127.0.0.1 + ::1 go to cidr
        assert!(cidr_contains(&r, "10.1.2.3"));
        assert!(cidr_contains(&r, "127.0.0.1"));
        assert!(cidr_contains(&r, "::1"));
        assert!(!cidr_contains(&r, "11.0.0.1"));
    }

    #[test]
    fn resolve_reads_env_with_https_falling_back_to_http() {
        let e = env(&[("HTTP_PROXY", "http://up:3128")]);
        let r = resolve_parent_proxy(None, &e).expect("some");
        assert_eq!(r.http_url.as_ref().unwrap().as_str(), "http://up:3128/");
        // https falls back to http
        assert_eq!(r.https_url.as_ref().unwrap().as_str(), "http://up:3128/");
        // schemeless host:port accepted
        let r2 = resolve_parent_proxy(None, &env(&[("HTTPS_PROXY", "up:8080")])).expect("some");
        assert_eq!(r2.https_url.as_ref().unwrap().as_str(), "http://up:8080/");
        // none set → None
        assert!(resolve_parent_proxy(None, &env(&[])).is_none());
    }

    #[test]
    fn bypass_loopback_all_cidr_and_suffix_golang_semantics() {
        let r = resolve_parent_proxy(
            None,
            &env(&[
                ("HTTP_PROXY", "http://up:3128"),
                ("NO_PROXY", "example.com, .corp, 10.0.0.0/8"),
            ]),
        )
        .unwrap();
        // loopback always bypassed
        assert!(should_bypass_parent_proxy(&r, "localhost"));
        assert!(should_bypass_parent_proxy(&r, "127.0.0.1"));
        assert!(should_bypass_parent_proxy(&r, "::1"));
        // exact + subdomain (golang: example.com matches foo.example.com)
        assert!(should_bypass_parent_proxy(&r, "example.com"));
        assert!(should_bypass_parent_proxy(&r, "foo.example.com"));
        // leading-dot suffix: .corp matches foo.corp AND corp
        assert!(should_bypass_parent_proxy(&r, "foo.corp"));
        assert!(should_bypass_parent_proxy(&r, "corp"));
        // CIDR
        assert!(should_bypass_parent_proxy(&r, "10.9.9.9"));
        assert!(!should_bypass_parent_proxy(&r, "other.net"));
        // star = bypass all
        let all = resolve_parent_proxy(
            None,
            &env(&[("HTTP_PROXY", "http://up:3128"), ("NO_PROXY", "*")]),
        )
        .unwrap();
        assert!(should_bypass_parent_proxy(&all, "anything.net"));
    }

    #[test]
    fn select_url_https_prefers_https_http_only_uses_http() {
        let r = resolve_parent_proxy(
            None,
            &env(&[
                ("HTTPS_PROXY", "http://sec:3128"),
                ("HTTP_PROXY", "http://plain:3128"),
            ]),
        )
        .unwrap();
        assert_eq!(
            select_parent_proxy_url(&r, true).unwrap().as_str(),
            "http://sec:3128/"
        );
        assert_eq!(
            select_parent_proxy_url(&r, false).unwrap().as_str(),
            "http://plain:3128/"
        );
    }

    #[test]
    fn strip_hop_by_hop_removes_standard_and_connection_listed() {
        // parent-proxy.js:28-38 + 326-342
        let h = vec![
            ("Host".to_string(), "x".to_string()),
            ("Connection".to_string(), "keep-alive, X-Custom".to_string()),
            ("Keep-Alive".to_string(), "timeout=5".to_string()),
            ("Transfer-Encoding".to_string(), "chunked".to_string()),
            ("X-Custom".to_string(), "drop-me".to_string()),
            ("Accept".to_string(), "*/*".to_string()),
        ];
        let out = strip_hop_by_hop(&h);
        let names: Vec<String> = out.iter().map(|(k, _)| k.to_ascii_lowercase()).collect();
        assert!(names.contains(&"host".to_string()));
        assert!(names.contains(&"accept".to_string()));
        assert!(!names.contains(&"connection".to_string()));
        assert!(!names.contains(&"keep-alive".to_string()));
        assert!(!names.contains(&"transfer-encoding".to_string()));
        assert!(!names.contains(&"x-custom".to_string())); // named in Connection
    }

    #[test]
    fn proxy_auth_header_basic_and_none() {
        assert_eq!(
            proxy_auth_header(&Url::parse("http://u:p@x:3128").unwrap()).as_deref(),
            Some("Basic dTpw") // base64("u:p")
        );
        assert!(proxy_auth_header(&Url::parse("http://x:3128").unwrap()).is_none());
        // percent-encoded userinfo is decoded before base64 (`a b:p@ss` form).
        assert_eq!(
            proxy_auth_header(&Url::parse("http://a%20b:p%40ss@x:3128").unwrap()).as_deref(),
            Some(
                format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD.encode("a b:p@ss")
                )
                .as_str()
            )
        );
    }

    #[test]
    fn redact_url_hides_userinfo() {
        assert_eq!(
            redact_url(Some(&Url::parse("http://u:p@x:3128/").unwrap())),
            "http://***:***@x:3128/"
        );
        assert_eq!(
            redact_url(Some(&Url::parse("http://x:3128/").unwrap())),
            "http://x:3128/"
        );
        assert_eq!(redact_url(None), "-");
    }

    #[test]
    fn connect_status_2xx_matches_ts_regex() {
        // `^HTTP/1\.[01] 2\d\d(?:\s|$)`
        assert!(connect_status_is_2xx("HTTP/1.1 200 OK"));
        assert!(connect_status_is_2xx("HTTP/1.0 200 Connection Established"));
        assert!(connect_status_is_2xx("HTTP/1.1 204")); // end-of-string after code
        assert!(!connect_status_is_2xx("HTTP/1.1 2000")); // 4th char not whitespace
        assert!(!connect_status_is_2xx("HTTP/1.1 302 Found"));
        assert!(!connect_status_is_2xx("HTTP/2 200"));
        assert!(!connect_status_is_2xx("HTTP/1.2 200"));
    }

    #[tokio::test]
    async fn open_connect_tunnel_preserves_leftover() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};
        // Fake parent proxy: accept, read the CONNECT, reply 200 + trailing bytes.
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let pport = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = [0u8; 256];
            let _ = s.read(&mut buf).await.unwrap();
            s.write_all(b"HTTP/1.1 200 OK\r\n\r\nLEFTOVER")
                .await
                .unwrap();
            // Echo whatever the client writes next over the tunnel.
            let mut b2 = [0u8; 16];
            let n = s.read(&mut b2).await.unwrap_or(0);
            let _ = s.write_all(&b2[..n]).await;
        });
        let sock = TcpStream::connect(("127.0.0.1", pport)).await.unwrap();
        let mut tun = open_connect_tunnel(sock, "example.com", 443, None)
            .await
            .expect("tunnel");
        // Post-200 LEFTOVER bytes must be replayed (unshift semantics).
        let mut lead = [0u8; 8];
        tun.read_exact(&mut lead).await.unwrap();
        assert_eq!(&lead, b"LEFTOVER");
        // Then a normal read/write must hit the inner stream (the echo).
        tun.write_all(b"ping").await.unwrap();
        let mut echo = [0u8; 4];
        tun.read_exact(&mut echo).await.unwrap();
        assert_eq!(&echo, b"ping");
    }

    #[tokio::test]
    async fn open_connect_tunnel_rejects_non_2xx_and_bad_host() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::{TcpListener, TcpStream};
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let pport = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            s.write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n")
                .await
                .unwrap();
        });
        let sock = TcpStream::connect(("127.0.0.1", pport)).await.unwrap();
        assert!(open_connect_tunnel(sock, "example.com", 443, None)
            .await
            .is_err());

        // CRLF-injection host rejected before writing CONNECT. Validation runs
        // first, so a dummy in-memory duplex is never touched.
        let (client, _server) = tokio::io::duplex(64);
        assert!(open_connect_tunnel(client, "evil\r\n.com", 443, None)
            .await
            .is_err());
        // Port 0 rejected too.
        let (client, _server) = tokio::io::duplex(64);
        assert!(open_connect_tunnel(client, "example.com", 0, None)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn open_connect_tunnel_sends_byte_exact_request_with_auth() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let pport = l.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = vec![0u8; 256];
            let n = s.read(&mut buf).await.unwrap();
            s.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await.unwrap();
            buf.truncate(n);
            buf
        });
        let sock = TcpStream::connect(("127.0.0.1", pport)).await.unwrap();
        let _tun = open_connect_tunnel(sock, "example.com", 443, Some("Basic dTpw"))
            .await
            .expect("tunnel");
        let got = handle.await.unwrap();
        assert_eq!(
            got,
            b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\nProxy-Authorization: Basic dTpw\r\n\r\n"
        );
    }

    #[tokio::test]
    async fn open_connect_tunnel_ipv6_authority_is_bracketed() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let pport = l.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = vec![0u8; 256];
            let n = s.read(&mut buf).await.unwrap();
            s.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await.unwrap();
            buf.truncate(n);
            buf
        });
        let sock = TcpStream::connect(("127.0.0.1", pport)).await.unwrap();
        let _tun = open_connect_tunnel(sock, "[::1]", 443, None)
            .await
            .expect("tunnel");
        let got = handle.await.unwrap();
        assert_eq!(
            got,
            b"CONNECT [::1]:443 HTTP/1.1\r\nHost: [::1]:443\r\n\r\n"
        );
    }

    #[tokio::test]
    async fn connect_via_parent_proxy_tcp_tunnels_through_http_proxy() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        // Fake http parent proxy on loopback; the https-TLS branch is exercised
        // by the P4 integration suite (needs a real rustls server cert).
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let pport = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = [0u8; 256];
            let _ = s.read(&mut buf).await.unwrap();
            s.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await.unwrap();
        });
        let proxy_url = Url::parse(&format!("http://127.0.0.1:{pport}")).unwrap();
        let tun = connect_via_parent_proxy(&proxy_url, "example.com", 443).await;
        assert!(tun.is_ok());
    }
}
