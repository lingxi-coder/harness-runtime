//! Proxy environment variables + command encoding (`sandbox-utils.js:261-389`).
//! [`generate_proxy_env_vars`] produces the exact `--setenv` pairs injected into
//! the bwrap child so its HTTP/SOCKS clients route through the host proxy bridge.

use base64::Engine;
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};

/// Per-tool CA-trust env vars set when TLS-MITM is configured so HTTPS clients
/// trust the proxy-minted leaf certs (`CA_TRUST_VARS`, sandbox-utils.js:261-270).
pub const CA_TRUST_VARS: [&str; 9] = [
    "NODE_EXTRA_CA_CERTS",
    "SSL_CERT_FILE",
    "CURL_CA_BUNDLE",
    "REQUESTS_CA_BUNDLE",
    "PIP_CERT",
    "GIT_SSL_CAINFO",
    "AWS_CA_BUNDLE",
    "CARGO_HTTP_CAINFO",
    "DENO_CERT",
];

/// Target platform for the `GIT_SSH_COMMAND` branch (`getPlatform`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// macOS — `nc -X 5 -x` SOCKS5 `ProxyCommand`.
    Macos,
    /// Linux — `socat ... PROXY:` `ProxyCommand` (requires `http_port`).
    Linux,
    /// Windows — no `GIT_SSH_COMMAND` is emitted. The TS `windows-sandbox-utils`
    /// calls `generateProxyEnvVars` without a platform arg, and on Windows the
    /// `getPlatform()==='windows'` branch fires neither the macOS-`nc` nor the
    /// Linux-`socat` `GIT_SSH_COMMAND` arm. This additive variant preserves that
    /// (the `match platform` Windows arm is a no-op) so the SOCKS branch on
    /// Windows skips `GIT_SSH_COMMAND`. P9b.
    Windows,
}

const NO_PROXY_ADDRESSES: &str =
    "localhost,127.0.0.1,::1,169.254.0.0/16,10.0.0.0/8,172.16.0.0/12,192.168.0.0/16";
const SANDBOX_PROXY_USER_PREFIX: &str = "srt.";
const USERINFO_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'/')
    .add(b':')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}')
    .add(b'=')
    .add(b'+');

fn encode_userinfo(value: &str) -> String {
    utf8_percent_encode(value, USERINFO_ENCODE_SET).to_string()
}

/// `sandbox-runtime` proxy username for `command`: `srt.` + the encoded command.
#[must_use]
pub fn sandbox_proxy_username(command: &str) -> String {
    format!(
        "{SANDBOX_PROXY_USER_PREFIX}{}",
        encode_sandboxed_command(command)
    )
}

/// Build `http://user:pass@localhost:port` / `socks5h://user:pass@localhost:port`
/// style proxy URLs when the sandbox manager wants per-command attribution.
#[must_use]
pub fn proxy_url_with_auth(
    scheme: &str,
    port: u16,
    command: &str,
    proxy_auth_token: &str,
) -> String {
    let user = encode_userinfo(&sandbox_proxy_username(command));
    let pass = encode_userinfo(proxy_auth_token);
    format!("{scheme}://{user}:{pass}@localhost:{port}")
}

/// Generate a random per-session proxy auth token. The exact bytes are not
/// user-facing; they only need to be unpredictable and URL-safe.
#[must_use]
pub fn generate_proxy_auth_token() -> String {
    let mut bytes = [0u8; 18];
    if getrandom::getrandom(&mut bytes).is_err() {
        for (idx, byte) in bytes.iter_mut().enumerate() {
            *byte = idx as u8;
        }
    }
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// `generateProxyEnvVars` (sandbox-utils.js:272-377). `tmpdir` is the resolved
/// `LINGXI_TMPDIR || LINGXI_TMPDIR || /tmp/claude` (resolved by the caller
/// to keep this pure). Returns ordered `(key, value)` pairs.
#[must_use]
pub fn generate_proxy_env_vars(
    http_proxy_port: Option<u16>,
    socks_proxy_port: Option<u16>,
    ca_cert_path: Option<&str>,
    platform: Platform,
    tmpdir: &str,
) -> Vec<(String, String)> {
    generate_proxy_env_vars_with_auth(
        http_proxy_port,
        socks_proxy_port,
        ca_cert_path,
        platform,
        tmpdir,
        None,
        None,
    )
}

/// `generateProxyEnvVars` plus optional per-command proxy credentials used by
/// the live sandbox manager to attribute denied egress back to one command.
#[must_use]
pub fn generate_proxy_env_vars_with_auth(
    http_proxy_port: Option<u16>,
    socks_proxy_port: Option<u16>,
    ca_cert_path: Option<&str>,
    platform: Platform,
    tmpdir: &str,
    command: Option<&str>,
    proxy_auth_token: Option<&str>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = vec![
        ("SANDBOX_RUNTIME".into(), "1".into()),
        ("TMPDIR".into(), tmpdir.into()),
    ];
    if let Some(ca) = ca_cert_path {
        for v in CA_TRUST_VARS {
            env.push((v.into(), ca.into()));
        }
    }
    if http_proxy_port.is_none() && socks_proxy_port.is_none() {
        return env;
    }
    let http_proxy_url = |port: u16| match (command, proxy_auth_token) {
        (Some(command), Some(proxy_auth_token)) => {
            proxy_url_with_auth("http", port, command, proxy_auth_token)
        }
        _ => format!("http://localhost:{port}"),
    };
    let socks_proxy_url = |port: u16| match (command, proxy_auth_token) {
        (Some(command), Some(proxy_auth_token)) => {
            proxy_url_with_auth("socks5h", port, command, proxy_auth_token)
        }
        _ => format!("socks5h://localhost:{port}"),
    };
    env.push(("NO_PROXY".into(), NO_PROXY_ADDRESSES.into()));
    env.push(("no_proxy".into(), NO_PROXY_ADDRESSES.into()));
    if let Some(p) = http_proxy_port {
        let u = http_proxy_url(p);
        for k in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
            env.push((k.into(), u.clone()));
        }
    }
    if let Some(s) = socks_proxy_port {
        let socks_url = socks_proxy_url(s);
        env.push(("ALL_PROXY".into(), socks_url.clone()));
        env.push(("all_proxy".into(), socks_url.clone()));
        let ssh_mux = "-o ControlMaster=no -o ControlPath=none";
        match platform {
            Platform::Macos => env.push((
                "GIT_SSH_COMMAND".into(),
                format!("ssh {ssh_mux} -o ProxyCommand='nc -X 5 -x localhost:{s} %h %p'"),
            )),
            Platform::Linux => {
                if let Some(h) = http_proxy_port {
                    let proxyauth = match (command, proxy_auth_token) {
                        (Some(command), Some(proxy_auth_token)) => {
                            format!(
                                ",proxyauth={}:{}",
                                sandbox_proxy_username(command),
                                proxy_auth_token
                            )
                        }
                        _ => String::new(),
                    };
                    env.push((
                        "GIT_SSH_COMMAND".into(),
                        format!(
                            "ssh {ssh_mux} -o ProxyCommand='socat - PROXY:localhost:%h:%p,proxyport={h}{proxyauth}'"
                        ),
                    ));
                }
            }
            // Windows: TS getPlatform()==='windows' fires neither GIT_SSH arm.
            Platform::Windows => {}
        }
        env.push(("FTP_PROXY".into(), socks_url.clone()));
        env.push(("ftp_proxy".into(), socks_url.clone()));
        env.push(("RSYNC_PROXY".into(), format!("localhost:{s}")));
        let docker = format!("http://localhost:{}", http_proxy_port.unwrap_or(s));
        env.push(("DOCKER_HTTP_PROXY".into(), docker.clone()));
        env.push(("DOCKER_HTTPS_PROXY".into(), docker));
        if let Some(h) = http_proxy_port {
            env.push(("CLOUDSDK_PROXY_TYPE".into(), "http".into()));
            env.push(("CLOUDSDK_PROXY_ADDRESS".into(), "localhost".into()));
            env.push(("CLOUDSDK_PROXY_PORT".into(), format!("{h}")));
        }
        env.push(("GRPC_PROXY".into(), socks_url.clone()));
        env.push(("grpc_proxy".into(), socks_url));
    }
    env
}

/// `encodeSandboxedCommand` (sandbox-utils.js:378-381): truncate to 100 chars,
/// base64. (Char-truncation matches the TS `.slice(0,100)`; for non-ASCII a
/// byte-vs-char nuance exists — TS slices UTF-16 code units; we slice by
/// `char` boundary, which is the closest faithful behavior. Documented.)
#[must_use]
pub fn encode_sandboxed_command(command: &str) -> String {
    let truncated: String = command.chars().take(100).collect();
    base64::engine::general_purpose::STANDARD.encode(truncated.as_bytes())
}

/// `decodeSandboxedCommand` (sandbox-utils.js:385-389): base64-decode to UTF-8
/// (lossy on invalid UTF-8, matching Node's `toString('utf8')`).
#[must_use]
pub fn decode_sandboxed_command(encoded: &str) -> String {
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has(v: &[(String, String)], k: &str, val: &str) -> bool {
        v.iter().any(|(kk, vv)| kk == k && vv == val)
    }

    #[test]
    fn no_ports_returns_minimal_plus_tmpdir() {
        let e = generate_proxy_env_vars(None, None, None, Platform::Linux, "/tmp/claude");
        assert!(has(&e, "SANDBOX_RUNTIME", "1"));
        assert!(has(&e, "TMPDIR", "/tmp/claude"));
        assert!(!e.iter().any(|(k, _)| k == "HTTP_PROXY")); // no proxy ports → no proxy vars
    }

    #[test]
    fn http_port_sets_proxy_and_no_proxy() {
        let e = generate_proxy_env_vars(Some(3128), None, None, Platform::Linux, "/tmp/claude");
        assert!(has(&e, "HTTP_PROXY", "http://localhost:3128"));
        assert!(has(&e, "HTTPS_PROXY", "http://localhost:3128"));
        assert!(has(&e, "http_proxy", "http://localhost:3128"));
        assert!(has(&e, "https_proxy", "http://localhost:3128"));
        assert!(has(
            &e,
            "NO_PROXY",
            "localhost,127.0.0.1,::1,169.254.0.0/16,10.0.0.0/8,172.16.0.0/12,192.168.0.0/16"
        ));
        assert!(has(
            &e,
            "no_proxy",
            "localhost,127.0.0.1,::1,169.254.0.0/16,10.0.0.0/8,172.16.0.0/12,192.168.0.0/16"
        ));
    }

    #[test]
    fn socks_port_sets_all_proxy_git_ssh_ftp_grpc() {
        // linux + http port → socat GIT_SSH_COMMAND
        let e =
            generate_proxy_env_vars(Some(3128), Some(1080), None, Platform::Linux, "/tmp/claude");
        assert!(has(&e, "ALL_PROXY", "socks5h://localhost:1080"));
        assert!(has(&e, "all_proxy", "socks5h://localhost:1080"));
        assert!(has(&e, "FTP_PROXY", "socks5h://localhost:1080"));
        assert!(has(&e, "RSYNC_PROXY", "localhost:1080"));
        assert!(has(&e, "GRPC_PROXY", "socks5h://localhost:1080"));
        assert!(has(&e, "DOCKER_HTTP_PROXY", "http://localhost:3128"));
        assert!(has(&e, "CLOUDSDK_PROXY_TYPE", "http"));
        assert!(has(&e, "CLOUDSDK_PROXY_PORT", "3128"));
        assert!(e.iter().any(|(k, v)| k == "GIT_SSH_COMMAND"
            && v == "ssh -o ControlMaster=no -o ControlPath=none -o ProxyCommand='socat - PROXY:localhost:%h:%p,proxyport=3128'"));
    }

    #[test]
    fn macos_socks_uses_nc_git_ssh() {
        let e = generate_proxy_env_vars(None, Some(1080), None, Platform::Macos, "/tmp/claude");
        assert!(e.iter().any(|(k, v)| k == "GIT_SSH_COMMAND"
            && v == "ssh -o ControlMaster=no -o ControlPath=none -o ProxyCommand='nc -X 5 -x localhost:1080 %h %p'"));
    }

    #[test]
    fn ca_cert_path_sets_all_trust_vars() {
        let e = generate_proxy_env_vars(
            Some(3128),
            None,
            Some("/tmp/ca.pem"),
            Platform::Linux,
            "/tmp/claude",
        );
        for v in CA_TRUST_VARS {
            assert!(has(&e, v, "/tmp/ca.pem"), "missing {v}");
        }
    }

    #[test]
    fn auth_proxy_urls_encode_the_command_identity() {
        let e = generate_proxy_env_vars_with_auth(
            Some(3128),
            Some(1080),
            None,
            Platform::Linux,
            "/tmp/claude",
            Some("echo hi"),
            Some("tok_123"),
        );
        assert!(has(
            &e,
            "HTTP_PROXY",
            "http://srt.ZWNobyBoaQ%3D%3D:tok_123@localhost:3128"
        ));
        assert!(has(
            &e,
            "ALL_PROXY",
            "socks5h://srt.ZWNobyBoaQ%3D%3D:tok_123@localhost:1080"
        ));
        assert!(e.iter().any(
            |(k, v)| k == "GIT_SSH_COMMAND" && v.contains("proxyauth=srt.ZWNobyBoaQ==:tok_123")
        ));
    }

    #[test]
    fn encode_decode_roundtrip_and_truncation() {
        assert_eq!(
            decode_sandboxed_command(&encode_sandboxed_command("hello")),
            "hello"
        );
        let long = "x".repeat(200);
        let dec = decode_sandboxed_command(&encode_sandboxed_command(&long));
        assert_eq!(dec.len(), 100); // truncated to 100 chars before base64
    }
}
