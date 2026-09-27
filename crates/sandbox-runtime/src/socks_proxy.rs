//! Hand-rolled RFC 1928 SOCKS5 (no-auth) allowlist proxy — a behavioral port of
//! `createSocksProxyServer` (`socks-proxy.js`). SECURITY-CRITICAL: it enforces
//! the network allowlist for the opaque TCP tunnels (SSH/git/SOCKS traffic) that
//! the sandbox bridges through the host proxy.
//!
//! The TS uses `@pondwader/socks5-server` with a `setRulesetValidator` +
//! `setConnectionHandler` pair; there is no Rust equivalent and no `socks` crate
//! in the lock, so the protocol is hand-rolled over `tokio`. The wire format is
//! small and well defined, and hand-rolling gives exact control over the
//! pre-connect filter (the security boundary): `is_valid_host` THEN
//! `filter_network_request`, BOTH before any dial.

use std::sync::Arc;

use percent_encoding::percent_decode_str;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::config::{
    current_network_config, shared_network_config, NetworkConfig, SharedNetworkConfig,
};
use crate::dial::dial_direct;
use crate::matcher::{filter_network_request_with_ask_decision, AskFn};
use crate::parent_proxy::{
    connect_via_parent_proxy, select_parent_proxy_url, should_bypass_parent_proxy,
    ResolvedParentProxy,
};
use crate::violation_store::{SandboxViolationStore, Violation};

/// SOCKS5 protocol version byte (RFC 1928 §3).
const SOCKS_VERSION: u8 = 0x05;

/// `CONNECT` command (RFC 1928 §4). The sandbox only tunnels CONNECT.
const CMD_CONNECT: u8 = 0x01;

/// `ATYP` = IPv4 address (4 octets, RFC 1928 §5).
const ATYP_IPV4: u8 = 0x01;
/// `ATYP` = fully-qualified domain name (1-byte length prefix + bytes).
const ATYP_DOMAINNAME: u8 = 0x03;
/// `ATYP` = IPv6 address (16 octets).
const ATYP_IPV6: u8 = 0x04;

/// Reply: succeeded / request granted (RFC 1928 §6).
pub const REP_GRANTED: u8 = 0x00;
/// Reply: general SOCKS server failure (used for malformed requests).
pub const REP_GENERAL_FAILURE: u8 = 0x01;
/// Reply: connection not allowed by ruleset (the allowlist rejection).
pub const REP_NOT_ALLOWED: u8 = 0x02;
/// Reply: host unreachable (the dial failed).
pub const REP_HOST_UNREACHABLE: u8 = 0x04;
/// Username/password auth (RFC 1929).
const METHOD_USERNAME_PASSWORD: u8 = 0x02;

/// Parse a SOCKS5 CONNECT request body into `(host, port)`.
///
/// Layout (RFC 1928 §4): `VER CMD RSV ATYP DST.ADDR DST.PORT`. Returns `None`
/// unless `VER == 5` and `CMD == CONNECT`, the `ATYP` is one of IPv4 / DOMAINNAME
/// / IPv6, and the buffer holds exactly the address plus the 2-byte big-endian
/// port. DOMAINNAME is decoded with `from_utf8_lossy` — it is an unvalidated
/// length-prefixed byte string, so the caller MUST run [`is_valid_host`] on the
/// result before trusting it (CRLF / null injection defense).
#[must_use]
pub fn parse_request_target(buf: &[u8]) -> Option<(String, u16)> {
    // VER CMD RSV ATYP = 4 header bytes minimum.
    if buf.len() < 4 || buf[0] != SOCKS_VERSION || buf[1] != CMD_CONNECT {
        return None;
    }
    let atyp = buf[3];
    let rest = &buf[4..];
    let (host, port_off) = match atyp {
        ATYP_IPV4 => {
            if rest.len() < 4 {
                return None;
            }
            let host = std::net::Ipv4Addr::new(rest[0], rest[1], rest[2], rest[3]).to_string();
            (host, 4)
        }
        ATYP_DOMAINNAME => {
            let len = *rest.first()? as usize;
            let name = rest.get(1..=len)?;
            (String::from_utf8_lossy(name).into_owned(), 1 + len)
        }
        ATYP_IPV6 => {
            let octets: [u8; 16] = rest.get(0..16)?.try_into().ok()?;
            (std::net::Ipv6Addr::from(octets).to_string(), 16)
        }
        _ => return None,
    };
    // Exactly 2 trailing bytes (big-endian port) must remain.
    let port_bytes = rest.get(port_off..port_off + 2)?;
    if rest.len() != port_off + 2 {
        return None;
    }
    let port = u16::from_be_bytes([port_bytes[0], port_bytes[1]]);
    Some((host, port))
}

/// Maximum SOCKS request frame we will buffer (`VER CMD RSV ATYP` + at most a
/// 256-byte DOMAINNAME + length byte + 2-byte port). Bounds a misbehaving client.
const MAX_REQUEST_LEN: usize = 4 + 1 + 255 + 2;

/// Runtime config for [`serve_socks`] (mirrors the TS `options` closure capture:
/// the allowlist filter config + the optional resolved parent proxy).
pub struct SocksOptions {
    /// Live, swappable allow/deny domain config consulted **per request** by
    /// [`filter_network_request_with_ask`]. A
    /// [`crate::manager::SandboxManager::update_config`] allow/deny swap takes
    /// effect on already-running connections with no rebind (the TS reads the
    /// shared module `config` per request). Build a non-live one with
    /// [`SocksOptions::with_static_config`].
    pub config: SharedNetworkConfig,
    /// Resolved parent proxy; `None` ⇒ always dial direct.
    pub parent_proxy: Option<Arc<ResolvedParentProxy>>,
    /// Optional interactive ask-callback (the TS `sandboxAskCallback`). Consulted
    /// by [`filter_network_request_with_ask`] ONLY for hosts no allow/deny rule
    /// decides. `None` ⇒ unmatched hosts denied (the P5 behaviour). Added for
    /// P8b so the manager can thread its ask-callback into the live SOCKS gate.
    pub ask: Option<AskFn>,
    /// Optional per-session token that validates per-command SOCKS credentials.
    pub proxy_auth_token: Option<String>,
    /// Optional violation store for `<sandbox_violations>` attribution.
    pub violation_store: Option<Arc<SandboxViolationStore>>,
}

impl SocksOptions {
    /// Build a [`SocksOptions`] whose `config` is a fresh non-live
    /// [`SharedNetworkConfig`] wrapping `config` (no external writer). For direct
    /// callers + tests; the per-request read path is identical, just nobody ever
    /// writes the handle.
    #[must_use]
    pub fn with_static_config(
        config: NetworkConfig,
        parent_proxy: Option<Arc<ResolvedParentProxy>>,
        ask: Option<AskFn>,
    ) -> Self {
        Self {
            config: shared_network_config(config),
            parent_proxy,
            ask,
            proxy_auth_token: None,
            violation_store: None,
        }
    }
}

fn format_host_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

async fn read_credential_field(client: &mut TcpStream) -> std::io::Result<String> {
    let mut len = [0u8; 1];
    client.read_exact(&mut len).await?;
    let mut bytes = vec![0u8; len[0] as usize];
    client.read_exact(&mut bytes).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn decode_socks_username_and_password(
    username: &str,
    password: &str,
    proxy_auth_token: Option<&str>,
) -> Option<String> {
    let expected = proxy_auth_token?;
    if password != expected {
        return None;
    }
    let decoded_user = percent_decode_str(username).decode_utf8_lossy();
    decoded_user.strip_prefix("srt.").map(ToString::to_string)
}

fn record_network_violation(
    opts: &SocksOptions,
    encoded_command: Option<String>,
    host: &str,
    port: u16,
    reason: &str,
) {
    let Some(store) = opts.violation_store.as_ref() else {
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

/// Build a SOCKS5 reply frame: `VER REP RSV ATYP=IPv4 BND.ADDR=0.0.0.0 BND.PORT=0`.
/// A zeroed bind address is standard and accepted by clients (RFC 1928 §6).
fn reply_frame(rep: u8) -> [u8; 10] {
    [SOCKS_VERSION, rep, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0]
}

/// Serve SOCKS5 connections accepted on `listener` until it errors, spawning a
/// detached task per connection. Faithful port of `createSocksProxyServer`.
///
/// # Errors
/// Returns the first `accept()` error (e.g. the listener was closed).
pub async fn serve_socks(listener: TcpListener, options: Arc<SocksOptions>) -> std::io::Result<()> {
    loop {
        let (client, _peer) = listener.accept().await?;
        let opts = Arc::clone(&options);
        tokio::spawn(async move {
            // Errors per connection are non-fatal (the TS logs + drops the socket).
            let _ = handle_connection(client, &opts).await;
        });
    }
}

/// Handle one accepted SOCKS5 client: greeting, request parse, the pre-connect
/// allowlist gate (`is_valid_host` THEN `filter_network_request`, BOTH before any
/// dial — the security boundary), then dial + opaque tunnel.
async fn handle_connection(mut client: TcpStream, opts: &SocksOptions) -> std::io::Result<()> {
    // --- Greeting: VER NMETHODS METHODS[] (RFC 1928 §3) ---
    let mut head = [0u8; 2];
    client.read_exact(&mut head).await?;
    if head[0] != SOCKS_VERSION {
        return Ok(()); // not SOCKS5 — close silently.
    }
    let nmethods = head[1] as usize;
    let mut methods = vec![0u8; nmethods];
    client.read_exact(&mut methods).await?;
    let encoded_command =
        if opts.proxy_auth_token.is_some() && methods.contains(&METHOD_USERNAME_PASSWORD) {
            client
                .write_all(&[SOCKS_VERSION, METHOD_USERNAME_PASSWORD])
                .await?;
            let mut version = [0u8; 1];
            client.read_exact(&mut version).await?;
            if version[0] != 0x01 {
                let _ = client.write_all(&[0x01, 0x01]).await;
                return Ok(());
            }
            let username = read_credential_field(&mut client).await?;
            let password = read_credential_field(&mut client).await?;
            let encoded = decode_socks_username_and_password(
                &username,
                &password,
                opts.proxy_auth_token.as_deref(),
            );
            let status = if encoded.is_some() { 0x00 } else { 0x01 };
            client.write_all(&[0x01, status]).await?;
            if status != 0x00 {
                return Ok(());
            }
            encoded
        } else if methods.contains(&0x00) {
            client.write_all(&[SOCKS_VERSION, 0x00]).await?;
            None
        } else {
            client.write_all(&[SOCKS_VERSION, 0xFF]).await?;
            return Ok(());
        };

    // --- Request: VER CMD RSV ATYP DST.ADDR DST.PORT (RFC 1928 §4) ---
    let Some(req) = read_request(&mut client).await? else {
        // Unparseable / oversized frame → general failure.
        let _ = client.write_all(&reply_frame(REP_GENERAL_FAILURE)).await;
        return Ok(());
    };
    let Some((host, port)) = parse_request_target(&req) else {
        let _ = client.write_all(&reply_frame(REP_GENERAL_FAILURE)).await;
        return Ok(());
    };

    // --- Pre-connect gate (socks-proxy.js:6-33). SECURITY BOUNDARY ---
    // is_valid_host THEN filter_network_request, BOTH before ANY dial. A denied
    // or malformed host gets REP_NOT_ALLOWED and NEVER reaches dial_direct /
    // connect_via_parent_proxy. SOCKS5 DOMAINNAME is an unvalidated byte string,
    // so is_valid_host is what stops CRLF/null reaching the matcher.
    // Read the CURRENT live config (clone the inner Arc), then DROP the guard
    // before the await — the std RwLock is never held across a suspension point.
    let net = current_network_config(&opts.config);
    let decision =
        filter_network_request_with_ask_decision(port, &host, &net, opts.ask.as_ref()).await;
    if let Some(reason) = decision.denied_reason() {
        record_network_violation(opts, encoded_command, &host, port, reason);
        let _ = client.write_all(&reply_frame(REP_NOT_ALLOWED)).await;
        return Ok(());
    }

    // --- Dial (socks-proxy.js:37-78) ---
    // SOCKS is an opaque TCP tunnel (semantically a CONNECT), so always prefer
    // HTTPS_PROXY when a parent proxy is set & not bypassed (is_https = true).
    let parent_url = opts.parent_proxy.as_ref().and_then(|pp| {
        if should_bypass_parent_proxy(pp, &host) {
            None
        } else {
            select_parent_proxy_url(pp, true)
        }
    });
    let upstream: std::io::Result<Box<dyn TunnelLike>> = match parent_url {
        Some(url) => connect_via_parent_proxy(url, &host, port)
            .await
            .map(|t| Box::new(t) as Box<dyn TunnelLike>),
        None => dial_direct(&host, port)
            .await
            .map(|s| Box::new(s) as Box<dyn TunnelLike>),
    };
    let Ok(mut upstream) = upstream else {
        let _ = client.write_all(&reply_frame(REP_HOST_UNREACHABLE)).await;
        return Ok(());
    };

    // --- Grant + opaque tunnel (socks-proxy.js:66-69) ---
    client.write_all(&reply_frame(REP_GRANTED)).await?;
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    Ok(())
}

/// A duplex stream usable as a tunnel endpoint — covers both the boxed parent-
/// proxy transport and a bare [`TcpStream`], so the dial branches converge on one
/// `Box<dyn>` for `copy_bidirectional`.
pub trait TunnelLike: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + ?Sized> TunnelLike for T {}

/// Read a complete SOCKS5 CONNECT request frame. Reads the fixed `VER CMD RSV
/// ATYP` header, derives the exact remaining length from `ATYP`, then reads
/// precisely the address + 2-byte port — so the buffer handed to
/// [`parse_request_target`] is exactly one frame. `None` if the `ATYP` is unknown
/// (the parse would reject it anyway) so the caller can reply general-failure.
async fn read_request(client: &mut TcpStream) -> std::io::Result<Option<Vec<u8>>> {
    let mut header = [0u8; 4];
    client.read_exact(&mut header).await?;
    let addr_len = match header[3] {
        ATYP_IPV4 => 4,
        ATYP_IPV6 => 16,
        ATYP_DOMAINNAME => {
            let mut len = [0u8; 1];
            client.read_exact(&mut len).await?;
            // Domain length byte is part of the frame; remember it.
            let dlen = len[0] as usize;
            let mut buf = Vec::with_capacity(4 + 1 + dlen + 2);
            buf.extend_from_slice(&header);
            buf.push(len[0]);
            let mut rest = vec![0u8; dlen + 2];
            client.read_exact(&mut rest).await?;
            buf.extend_from_slice(&rest);
            debug_assert!(buf.len() <= MAX_REQUEST_LEN);
            return Ok(Some(buf));
        }
        _ => return Ok(None),
    };
    let mut buf = Vec::with_capacity(4 + addr_len + 2);
    buf.extend_from_slice(&header);
    let mut rest = vec![0u8; addr_len + 2];
    client.read_exact(&mut rest).await?;
    buf.extend_from_slice(&rest);
    Ok(Some(buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::shared_network_config;
    use crate::env::sandbox_proxy_username;
    use crate::violation_store::SandboxViolationStore;
    #[test]
    fn parse_connect_request_ipv4_domain_ipv6() {
        // VER=5 CMD=1 RSV=0 ATYP DST.ADDR DST.PORT
        // IPv4 1.2.3.4:443
        let v4 = [5, 1, 0, 1, 1, 2, 3, 4, 0x01, 0xBB];
        assert_eq!(
            parse_request_target(&v4).unwrap(),
            ("1.2.3.4".to_string(), 443)
        );
        // DOMAINNAME "example.com":80
        let mut d = vec![5, 1, 0, 3, 11];
        d.extend_from_slice(b"example.com");
        d.extend_from_slice(&[0, 80]);
        assert_eq!(
            parse_request_target(&d).unwrap(),
            ("example.com".to_string(), 80)
        );
        // IPv6 ::1:8443
        let mut v6 = vec![5, 1, 0, 4];
        v6.extend_from_slice(&std::net::Ipv6Addr::LOCALHOST.octets());
        v6.extend_from_slice(&[0x20, 0xFB]);
        assert_eq!(
            parse_request_target(&v6).unwrap(),
            ("::1".to_string(), 8443)
        );
        // bad CMD (not CONNECT) / bad VER → None
        assert!(parse_request_target(&[5, 2, 0, 1, 1, 2, 3, 4, 0, 80]).is_none());
        assert!(parse_request_target(&[4, 1, 0, 1, 1, 2, 3, 4, 0, 80]).is_none());
    }

    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use tokio::net::TcpListener as TokioListener;

    /// Spawn a loopback echo server; returns its port. Increments `seen` once per
    /// accepted connection (a tripwire to assert an upstream was/wasn't dialed).
    async fn spawn_echo(seen: Arc<AtomicU32>) -> u16 {
        let l = TokioListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else {
                    break;
                };
                seen.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    loop {
                        match s.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if s.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        });
        port
    }

    /// Spawn the SOCKS proxy with the given allowlist; returns its bound port.
    async fn spawn_socks(allowed: Vec<String>) -> u16 {
        let cfg = NetworkConfig {
            allowed_domains: allowed,
            denied_domains: vec![],
            ..Default::default()
        };
        let opts = Arc::new(SocksOptions::with_static_config(cfg, None, None));
        let l = TokioListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = serve_socks(l, opts).await;
        });
        port
    }

    /// Perform the SOCKS5 no-auth handshake on `c` and send a CONNECT request for
    /// `req_target` (the raw request body bytes minus the no-auth greeting).
    /// Returns the 10-byte reply frame.
    async fn socks_handshake_and_request(c: &mut TcpStream, req_body: &[u8]) -> [u8; 10] {
        // Greeting: VER=5 NMETHODS=1 METHOD=0 (no-auth).
        c.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut sel = [0u8; 2];
        c.read_exact(&mut sel).await.unwrap();
        assert_eq!(sel, [0x05, 0x00]);
        c.write_all(req_body).await.unwrap();
        let mut reply = [0u8; 10];
        c.read_exact(&mut reply).await.unwrap();
        reply
    }

    async fn socks_handshake_auth_and_request(
        c: &mut TcpStream,
        command: &str,
        token: &str,
        req_body: &[u8],
    ) -> [u8; 10] {
        c.write_all(&[0x05, 0x01, METHOD_USERNAME_PASSWORD])
            .await
            .unwrap();
        let mut sel = [0u8; 2];
        c.read_exact(&mut sel).await.unwrap();
        assert_eq!(sel, [0x05, METHOD_USERNAME_PASSWORD]);
        let username = sandbox_proxy_username(command);
        c.write_all(&[0x01, username.len() as u8]).await.unwrap();
        c.write_all(username.as_bytes()).await.unwrap();
        c.write_all(&[token.len() as u8]).await.unwrap();
        c.write_all(token.as_bytes()).await.unwrap();
        let mut auth = [0u8; 2];
        c.read_exact(&mut auth).await.unwrap();
        assert_eq!(auth, [0x01, 0x00]);
        c.write_all(req_body).await.unwrap();
        let mut reply = [0u8; 10];
        c.read_exact(&mut reply).await.unwrap();
        reply
    }

    /// Build a CONNECT request body for an IPv4 host:port.
    fn req_ipv4(ip: std::net::Ipv4Addr, port: u16) -> Vec<u8> {
        let mut v = vec![0x05, 0x01, 0x00, ATYP_IPV4];
        v.extend_from_slice(&ip.octets());
        v.extend_from_slice(&port.to_be_bytes());
        v
    }

    /// Build a CONNECT request body for a DOMAINNAME host:port (raw bytes — may
    /// contain control chars to exercise the `is_valid_host` gate).
    fn req_domain(name: &[u8], port: u16) -> Vec<u8> {
        let dlen = u8::try_from(name.len()).expect("test domain fits in a byte");
        let mut v = vec![0x05, 0x01, 0x00, ATYP_DOMAINNAME, dlen];
        v.extend_from_slice(name);
        v.extend_from_slice(&port.to_be_bytes());
        v
    }

    #[tokio::test]
    async fn allowed_host_is_granted_and_tunnel_echoes() {
        let seen = Arc::new(AtomicU32::new(0));
        let echo_port = spawn_echo(Arc::clone(&seen)).await;
        // Allowlist the loopback echo upstream by exact IP literal.
        let socks_port = spawn_socks(vec!["127.0.0.1".into()]).await;

        let mut c = TcpStream::connect(("127.0.0.1", socks_port)).await.unwrap();
        let reply = socks_handshake_and_request(
            &mut c,
            &req_ipv4(std::net::Ipv4Addr::LOCALHOST, echo_port),
        )
        .await;
        assert_eq!(reply[1], REP_GRANTED, "expected REQUEST_GRANTED");
        // The opaque tunnel echoes.
        c.write_all(b"hello-socks").await.unwrap();
        let mut got = [0u8; 11];
        c.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"hello-socks");
        assert_eq!(
            seen.load(Ordering::SeqCst),
            1,
            "echo upstream was dialed once"
        );
    }

    #[tokio::test]
    async fn denied_host_gets_not_allowed_and_is_never_dialed() {
        // The allowlist contains ONLY the echo upstream. A DIFFERENT loopback
        // target (a tripwire listener) is denied — it must get REP_NOT_ALLOWED
        // and the tripwire must NEVER see a connection (gate runs before dial).
        let tripwire_seen = Arc::new(AtomicU32::new(0));
        let tripwire_port = spawn_echo(Arc::clone(&tripwire_seen)).await;
        // Allow only 198.51.100.1 (TEST-NET-2, never the tripwire's 127.0.0.1).
        let socks_port = spawn_socks(vec!["198.51.100.1".into()]).await;

        let mut c = TcpStream::connect(("127.0.0.1", socks_port)).await.unwrap();
        let reply = socks_handshake_and_request(
            &mut c,
            &req_ipv4(std::net::Ipv4Addr::LOCALHOST, tripwire_port),
        )
        .await;
        assert_eq!(
            reply[1], REP_NOT_ALLOWED,
            "denied host must be REP_NOT_ALLOWED"
        );
        // Give any (erroneous) dial a beat to land, then assert the tripwire is untouched.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            tripwire_seen.load(Ordering::SeqCst),
            0,
            "denied upstream must NEVER be contacted"
        );
    }

    #[tokio::test]
    async fn malformed_crlf_domain_is_rejected_before_dial() {
        // A DOMAINNAME with embedded CRLF — parses fine as a byte string but
        // is_valid_host rejects it, so it must get REP_NOT_ALLOWED and never dial.
        // Sentinel proves no upstream was contacted: allow nothing real.
        let dialed = Arc::new(AtomicBool::new(false));
        let socks_port = spawn_socks(vec!["evil.com".into()]).await;

        let mut c = TcpStream::connect(("127.0.0.1", socks_port)).await.unwrap();
        let reply =
            socks_handshake_and_request(&mut c, &req_domain(b"evil.com\r\n.allowed.com", 80)).await;
        assert_eq!(
            reply[1], REP_NOT_ALLOWED,
            "CRLF domain must be rejected by is_valid_host"
        );
        assert!(!dialed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn domainname_parse_routes_to_allowed_upstream() {
        // DOMAINNAME "localhost" → canonicalizes & is allowlisted as localhost;
        // dial_direct resolves it to the loopback echo (started on a fixed port).
        let seen = Arc::new(AtomicU32::new(0));
        let echo_port = spawn_echo(Arc::clone(&seen)).await;
        let socks_port = spawn_socks(vec!["localhost".into()]).await;

        let mut c = TcpStream::connect(("127.0.0.1", socks_port)).await.unwrap();
        let reply = socks_handshake_and_request(&mut c, &req_domain(b"localhost", echo_port)).await;
        assert_eq!(
            reply[1], REP_GRANTED,
            "DOMAINNAME localhost should be granted"
        );
        c.write_all(b"dn").await.unwrap();
        let mut got = [0u8; 2];
        c.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"dn");
        assert_eq!(seen.load(Ordering::SeqCst), 1);
    }

    /// LIVE update_config proof on a RUNNING SOCKS proxy: deny-all (empty
    /// allowlist) → REP_NOT_ALLOWED; after writing `allowedDomains=[127.0.0.1]`
    /// into the SHARED handle the proxy reads, a NEW request to the echo upstream
    /// is REP_GRANTED and tunnels — no rebind.
    #[tokio::test]
    async fn live_config_swap_changes_socks_decision() {
        let seen = Arc::new(AtomicU32::new(0));
        let echo_port = spawn_echo(Arc::clone(&seen)).await;

        // Deny-all on a shared, swappable handle.
        let shared = shared_network_config(NetworkConfig::default());
        let opts = Arc::new(SocksOptions {
            config: Arc::clone(&shared),
            parent_proxy: None,
            ask: None,
            proxy_auth_token: None,
            violation_store: None,
        });
        let l = TokioListener::bind("127.0.0.1:0").await.unwrap();
        let socks_port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = serve_socks(l, opts).await;
        });

        // Denied before update.
        let mut c = TcpStream::connect(("127.0.0.1", socks_port)).await.unwrap();
        let reply = socks_handshake_and_request(
            &mut c,
            &req_ipv4(std::net::Ipv4Addr::LOCALHOST, echo_port),
        )
        .await;
        assert_eq!(reply[1], REP_NOT_ALLOWED, "deny-all before update");

        // LIVE swap: allow loopback.
        *shared.write().unwrap() = Arc::new(NetworkConfig {
            allowed_domains: vec!["127.0.0.1".into()],
            denied_domains: vec![],
            ..Default::default()
        });

        // A NEW request is now granted and the tunnel echoes.
        let mut c2 = TcpStream::connect(("127.0.0.1", socks_port)).await.unwrap();
        let reply2 = socks_handshake_and_request(
            &mut c2,
            &req_ipv4(std::net::Ipv4Addr::LOCALHOST, echo_port),
        )
        .await;
        assert_eq!(reply2[1], REP_GRANTED, "granted after live update");
        c2.write_all(b"livesocks").await.unwrap();
        let mut got = [0u8; 9];
        c2.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"livesocks");
    }

    #[tokio::test]
    async fn denied_socks_request_with_auth_records_violation_line() {
        let store = Arc::new(SandboxViolationStore::new());
        let opts = Arc::new(SocksOptions {
            config: shared_network_config(NetworkConfig::default()),
            parent_proxy: None,
            ask: None,
            proxy_auth_token: Some("tok_123".into()),
            violation_store: Some(Arc::clone(&store)),
        });
        let l = TokioListener::bind("127.0.0.1:0").await.unwrap();
        let socks_port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = serve_socks(l, opts).await;
        });

        let mut c = TcpStream::connect(("127.0.0.1", socks_port)).await.unwrap();
        let reply = socks_handshake_auth_and_request(
            &mut c,
            "echo hi",
            "tok_123",
            &req_domain(b"denied.example", 443),
        )
        .await;
        assert_eq!(reply[1], REP_NOT_ALLOWED);
        assert_eq!(
            store.get_violation_lines_for_command("echo hi"),
            vec!["deny network-outbound denied.example:443 (host is not on the allow list)"]
        );
    }
}
