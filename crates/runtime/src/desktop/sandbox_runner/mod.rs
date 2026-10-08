//! Desktop sandbox adapter — the LIVE [`tool_api::SandboxRunner`] backed by a
//! per-session [`sandbox_runtime::SandboxManager`].
//!
//! The engine's shell/skill tools call `ctx.sandbox_runner.wrap(...)` to wrap a
//! command before spawning it. The default
//! [`tool_api::LegacyWrapRunner`] forwards straight to the sync
//! [`sandbox::wrap::wrap_with_sandbox`]. This module provides the opt-in live
//! alternative: [`SandboxRuntimeRunner`] owns a `SandboxManager` that brings up
//! the host HTTP/SOCKS forward proxies (and, on Linux, the `socat` bridge +
//! MITM/seccomp), giving the engine the faithful sandbox-runtime network +
//! filesystem stack.
//!
//! ## Lifecycle
//!
//! The manager is **lazily initialized** on the first [`wrap`] call (so a
//! session that never sandboxes a command pays nothing) and reused across
//! subsequent calls. The engine config can change between calls; the runner
//! distinguishes:
//!
//! - **domain-only change** (only `network.allowed_domains` /
//!   `network.denied_domains` differ) → [`SandboxManager::update_config`], a
//!   LIVE swap of the running proxies' allow/deny lists with no rebind.
//! - **structural change** (anything else: ports, weaker-mode flags, …) →
//!   [`SandboxManager::reset`] + [`SandboxManager::initialize`], because those
//!   are baked into the proxy bind and are not live-swappable (faithful to the
//!   manager's own live-vs-structural contract).
//!
//! Filesystem paths are neither: each command's config arrives already rooted
//! at that command's directory (an agent's worktree or the session workspace,
//! see `sandbox::root`), so they differ between concurrent agents. They are
//! handed to the manager per command as its custom config and never reset the
//! proxies other commands are using.
//!
//! This module is **desktop-only** (it pulls hyper/rustls/tokio via
//! `sandbox-runtime`); the mobile profile does not compile this adapter.

#![forbid(unsafe_code)]

mod convert;

pub use convert::to_runtime_config;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use sandbox::runtime_config::{Platform, SandboxRuntimeConfig as EngineConfig};
use sandbox::wrap::SandboxWrapError;
use sandbox_runtime::SandboxManager;
use sandbox_runtime::linux::cleanup_bwrap_mount_points;
use sandbox_runtime::manager::ManagerError;
pub use sandbox_runtime::matcher::AskFn;
use tokio::sync::Mutex;

/// Map a [`ManagerError`] into the engine's [`SandboxWrapError`].
///
/// Everything degrades to [`SandboxWrapError::Unsupported`] carrying the
/// manager's message, so a host that can't bring up the sandbox (missing deps,
/// bind failure, an unsupported platform branch) fails the same way the legacy
/// path does today — the caller surfaces the string and falls back to running
/// unsandboxed per its `fail_if_unavailable` policy.
// Takes `ManagerError` by value so it can be passed directly as the
// `FnOnce(E)` to `Result::map_err` (which hands ownership of the error).
#[allow(clippy::needless_pass_by_value)]
fn map_err(e: ManagerError) -> SandboxWrapError {
    SandboxWrapError::Unsupported(e.to_string())
}

/// Hash the structural shape of a runtime config.
///
/// The domain allow/deny lists are EXCLUDED because a change limited to those
/// is live-swappable via [`SandboxManager::update_config`]. The filesystem path
/// lists are EXCLUDED because every wrap applies the command's own rooted lists
/// as the manager's custom config. Everything else (ports, weaker-mode flags,
/// MITM/TLS, …) requires a reset + re-initialize. We hash the
/// JSON serialization of the config with those lists blanked out — any
/// structural field flip changes the bytes, while a domain or path edit does
/// not.
fn structural_key(rt: &sandbox_runtime::SandboxRuntimeConfig) -> u64 {
    let mut scrubbed = rt.clone();
    scrubbed.network.allowed_domains.clear();
    scrubbed.network.denied_domains.clear();
    // Per-command, per-root paths: applied through the wrap's custom config.
    scrubbed.filesystem.allow_write.clear();
    scrubbed.filesystem.deny_write.clear();
    scrubbed.filesystem.deny_read.clear();
    scrubbed.filesystem.allow_read = None;
    // `SandboxRuntimeConfig` is `Serialize`; JSON gives a stable, total byte
    // image of every remaining field. (It is not `Hash`, so we hash the bytes.)
    let json = serde_json::to_string(&scrubbed).unwrap_or_default();
    let mut h = DefaultHasher::new();
    json.hash(&mut h);
    h.finish()
}

/// The two network domain lists `(allowed, denied)` — the only part of the
/// config that is live-swappable.
fn domain_lists(rt: &sandbox_runtime::SandboxRuntimeConfig) -> (Vec<String>, Vec<String>) {
    (
        rt.network.allowed_domains.clone(),
        rt.network.denied_domains.clone(),
    )
}

/// Mutable per-session sandbox state, guarded by the runner's async mutex.
struct RunnerState {
    /// The live manager, `Some` after the first successful [`wrap`].
    manager: Option<SandboxManager>,
    /// `(allowed_domains, denied_domains)` of the active config — compared on
    /// each `wrap` to detect a live-swappable domain change.
    active_net: Option<(Vec<String>, Vec<String>)>,
    /// Structural key of the active config — a change forces reset + re-init.
    last_full_key: Option<u64>,
    /// Mount points returned by the last `wrap_with_sandbox` (Linux bwrap
    /// artifacts; always empty on macOS), cleaned by [`cleanup_after_command`].
    mount_points: Vec<PathBuf>,
}

/// The live [`tool_api::SandboxRunner`] backed by a per-session
/// [`SandboxManager`]. Cheap to construct; the manager is brought up lazily on
/// the first [`wrap`](SandboxRunner::wrap).
pub struct SandboxRuntimeRunner {
    state: Mutex<RunnerState>,
    ask_callback: Option<AskFn>,
}

impl Default for SandboxRuntimeRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl SandboxRuntimeRunner {
    /// Create an idle runner. No manager / proxies are started until the first
    /// [`wrap`](SandboxRunner::wrap).
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(RunnerState {
                manager: None,
                active_net: None,
                last_full_key: None,
                mount_points: Vec::new(),
            }),
            ask_callback: None,
        }
    }

    /// Create an idle runner whose live proxies ask the host before allowing an
    /// unmatched domain when `network.strictAllowlist` is disabled.
    #[must_use]
    pub fn with_ask_callback(ask_callback: AskFn) -> Self {
        Self {
            state: Mutex::new(RunnerState {
                manager: None,
                active_net: None,
                last_full_key: None,
                mount_points: Vec::new(),
            }),
            ask_callback: Some(ask_callback),
        }
    }
}

#[async_trait]
impl tool_api::SandboxRunner for SandboxRuntimeRunner {
    async fn wrap(
        &self,
        command: &str,
        cfg: &EngineConfig,
        _platform: Platform,
        bin_shell: Option<&str>,
        cwd: Option<&Path>,
    ) -> Result<String, SandboxWrapError> {
        // `platform` is informational here: the manager detects the host OS
        // itself (it must, since the proxies/bridge are host-side) and dispatches
        // the bwrap/seatbelt branch accordingly. The engine's `platform` already
        // matches the host in every wired call site.
        let rt = to_runtime_config(cfg);
        let key = structural_key(&rt);
        let net = domain_lists(&rt);

        let mut state = self.state.lock().await;

        if state.manager.is_none() {
            // ── Lazy first init ──
            let mut m = SandboxManager::new();
            m.initialize(rt.clone(), self.ask_callback.clone(), false)
                .await
                .map_err(map_err)?;
            state.manager = Some(m);
            state.last_full_key = Some(key);
            state.active_net = Some(net.clone());
        } else if state.last_full_key != Some(key) {
            // ── Structural change → reset + re-init ──
            let m = state.manager.as_mut().expect("manager checked Some");
            m.reset();
            m.initialize(rt.clone(), self.ask_callback.clone(), false)
                .await
                .map_err(map_err)?;
            state.last_full_key = Some(key);
            state.active_net = Some(net.clone());
        } else if state.active_net.as_ref() != Some(&net) {
            // ── Domain-only change → live update (no rebind) ──
            let m = state.manager.as_mut().expect("manager checked Some");
            m.update_config(rt.clone());
            state.active_net = Some(net.clone());
        }

        // The sandbox root `rt` was resolved against. Without one the manager
        // falls back to the host process's cwd, as before.
        let cwd_s = cwd.and_then(Path::to_str).unwrap_or(".");
        let m = state.manager.as_ref().expect("manager initialized above");
        // HP-6: drop `sandbox.credentials` variables before the command runs.
        //
        // Prefixing the COMMAND (rather than the bwrap/seatbelt invocation) is
        // what makes this portable: both platform branches end in a shell, and
        // dropping the variable there covers the command and everything it
        // spawns. Names are validated as POSIX env names first — they come from
        // a settings file, which on the project tier can be checked into a
        // repository. The spelling follows `bin_shell`, because this seam is
        // shared with the PowerShell tool, which has no `unset`.
        let unset =
            sandbox::credentials::unset_prefix_for_shell(&cfg.credential_deny_env, bin_shell);
        let command: std::borrow::Cow<'_, str> = if unset.is_empty() {
            std::borrow::Cow::Borrowed(command)
        } else {
            std::borrow::Cow::Owned(format!("{unset}{command}"))
        };
        let (wrapped, mounts) = m
            .wrap_with_sandbox(&command, bin_shell, Some(&rt), cwd_s)
            .map_err(map_err)?;
        state.mount_points = mounts;
        Ok(wrapped)
    }

    async fn cleanup_after_command(&self) {
        let mut state = self.state.lock().await;
        cleanup_bwrap_mount_points(&state.mount_points);
        state.mount_points.clear();
    }

    async fn reset(&self) {
        let mut state = self.state.lock().await;
        if let Some(m) = &mut state.manager {
            m.reset();
        }
        state.manager = None;
        state.active_net = None;
        state.last_full_key = None;
        state.mount_points.clear();
    }

    async fn command_violations(
        &self,
        command: &str,
    ) -> tool_api::sandbox_runner::SandboxCommandViolations {
        let state = self.state.lock().await;
        let lines = state
            .manager
            .as_ref()
            .map(|manager| manager.violation_lines_for_command(command))
            .unwrap_or_default();
        tool_api::sandbox_runner::SandboxCommandViolations { lines }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandbox::runtime_config::NetworkRestrictionConfig;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tool_api::SandboxRunner as _;

    fn cfg_with_domains(domains: &[&str]) -> EngineConfig {
        EngineConfig {
            network: NetworkRestrictionConfig {
                allowed_domains: domains.iter().map(|s| (*s).to_string()).collect(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// HP-6 end to end: a `sandbox.credentials` env var must actually be
    /// dropped from the command the sandbox runs.
    ///
    /// 🚨 Asserting only the pure `unset_prefix` would be a tautology — the
    /// whole failure mode this feature exists to close is a credential setting
    /// that is parsed, resolved, and then never reaches the command. So this
    /// asserts on the WRAPPED STRING the sandbox actually hands back.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_credential_env_var_is_unset_in_the_wrapped_command() {
        let runner = SandboxRuntimeRunner::new();
        let mut cfg = cfg_with_domains(&["github.com"]);
        cfg.credential_deny_env = vec![
            "AWS_SECRET_ACCESS_KEY".to_string(),
            // …and a hostile name alongside it, to prove the guard does not
            // simply drop the whole list when one entry is bad.
            "OOPS; touch /tmp/pwned".to_string(),
        ];
        let cwd = std::path::PathBuf::from("/tmp");

        let wrapped = runner
            .wrap("echo hi", &cfg, Platform::Mac, Some("bash"), Some(&cwd))
            .await
            .expect("macOS initialize+wrap should succeed");
        let decoded = decode_wrapped(&wrapped);
        assert!(
            decoded.contains("unset AWS_SECRET_ACCESS_KEY"),
            "the credential variable must be unset in the command the sandbox \
             runs, got: {decoded}"
        );
        assert!(
            !decoded.contains("touch /tmp/pwned"),
            "a settings-supplied name that is not a POSIX env name must never \
             reach the shell: {decoded}"
        );

        // …and with no credentials configured the command is untouched.
        let plain = runner
            .wrap(
                "echo hi",
                &cfg_with_domains(&["github.com"]),
                Platform::Mac,
                Some("bash"),
                Some(&cwd),
            )
            .await
            .expect("wrap should succeed");
        assert!(
            !decode_wrapped(&plain).contains("unset "),
            "an unconfigured sandbox must produce the same command as before"
        );
    }

    /// The wrapped command embeds the user command base64-encoded; decode every
    /// base64 run so the assertions above look at what the shell will see.
    #[cfg(target_os = "macos")]
    fn decode_wrapped(wrapped: &str) -> String {
        use base64::Engine as _;
        let mut out = wrapped.to_string();
        for token in wrapped
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='))
        {
            if token.len() < 8 {
                continue;
            }
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(token) {
                if let Ok(text) = String::from_utf8(bytes) {
                    out.push('\n');
                    out.push_str(&text);
                }
            }
        }
        out
    }

    /// On macOS the manager binds its proxies locally (no bridge needed) and
    /// `wrap_with_sandbox` returns the Seatbelt `sandbox-exec` shell string.
    /// If the host can't initialize (deps missing — should not happen on macOS,
    /// where Seatbelt is built in), the runner returns `Unsupported` instead.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn wrap_on_macos_returns_sandbox_exec_and_reuses_manager() {
        let runner = SandboxRuntimeRunner::new();
        let cfg = cfg_with_domains(&["github.com"]);
        let cwd = std::path::PathBuf::from("/tmp");

        let first = runner
            .wrap("echo hi", &cfg, Platform::Mac, Some("bash"), Some(&cwd))
            .await
            .expect("macOS initialize+wrap should succeed (Seatbelt is built in)");
        assert!(
            first.contains("sandbox-exec"),
            "expected a sandbox-exec wrap, got: {first}"
        );

        // The manager is initialized; capture its proxy port to prove the 2nd
        // wrap reuses it (no re-init → same port).
        let port_after_first = {
            let state = runner.state.lock().await;
            state
                .manager
                .as_ref()
                .expect("manager initialized after first wrap")
                .http_proxy_port()
                .expect("proxy port bound")
        };

        // 2nd wrap with the SAME config: must reuse the manager (stable port).
        let second = runner
            .wrap("echo hi2", &cfg, Platform::Mac, Some("bash"), Some(&cwd))
            .await
            .expect("second wrap should succeed");
        assert!(second.contains("sandbox-exec"));
        let port_after_second = {
            let state = runner.state.lock().await;
            state.manager.as_ref().unwrap().http_proxy_port().unwrap()
        };
        assert_eq!(
            port_after_first, port_after_second,
            "same-config wrap must reuse the manager (stable proxy port)"
        );

        // reset() then wrap must re-initialize (a fresh manager, likely a new
        // port — at minimum a live manager again).
        runner.reset().await;
        {
            let state = runner.state.lock().await;
            assert!(state.manager.is_none(), "reset must drop the manager");
        }
        let third = runner
            .wrap("echo hi3", &cfg, Platform::Mac, Some("bash"), Some(&cwd))
            .await
            .expect("wrap after reset should re-initialize");
        assert!(third.contains("sandbox-exec"));
        runner.reset().await;
    }

    /// A domain-only change between wraps must NOT reset the manager: the proxy
    /// port stays stable (live `update_config`), while the structural key is
    /// unchanged.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn domain_change_is_live_no_reinit() {
        let runner = SandboxRuntimeRunner::new();
        let cwd = std::path::PathBuf::from("/tmp");

        runner
            .wrap(
                "echo a",
                &cfg_with_domains(&["github.com"]),
                Platform::Mac,
                Some("bash"),
                Some(&cwd),
            )
            .await
            .expect("first wrap");
        let port1 = {
            let s = runner.state.lock().await;
            s.manager.as_ref().unwrap().http_proxy_port().unwrap()
        };

        // Only the allowed_domains list changes → live update, same manager.
        runner
            .wrap(
                "echo b",
                &cfg_with_domains(&["github.com", "npmjs.org"]),
                Platform::Mac,
                Some("bash"),
                Some(&cwd),
            )
            .await
            .expect("second wrap (domain change)");
        let port2 = {
            let s = runner.state.lock().await;
            s.manager.as_ref().unwrap().http_proxy_port().unwrap()
        };
        assert_eq!(port1, port2, "domain-only change must not rebind the proxy");
        runner.reset().await;
    }

    /// The production runner must install the host callback on the live proxy.
    /// Before this regression, `SandboxManager::initialize` always received
    /// `None`, so non-strict and strict allowlists both denied unmatched hosts.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn unmatched_domain_reaches_host_ask_callback() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_in = Arc::clone(&calls);
        let ask: AskFn = Arc::new(move |host, port| {
            let calls = Arc::clone(&calls_in);
            let host = host.to_owned();
            Box::pin(async move {
                assert_eq!(host, "unmatched.example");
                assert_eq!(port, 443);
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(false)
            })
        });
        let runner = SandboxRuntimeRunner::with_ask_callback(ask);
        let cwd = PathBuf::from("/tmp");
        runner
            .wrap(
                "echo ask",
                &EngineConfig::default(),
                Platform::Mac,
                Some("bash"),
                Some(&cwd),
            )
            .await
            .expect("initialize proxy");

        let proxy_port = {
            let state = runner.state.lock().await;
            state
                .manager
                .as_ref()
                .and_then(SandboxManager::http_proxy_port)
                .expect("live HTTP proxy")
        };
        let mut socket = TcpStream::connect(("127.0.0.1", proxy_port))
            .await
            .expect("connect to proxy");
        socket
            .write_all(
                b"CONNECT unmatched.example:443 HTTP/1.1\r\n\
                  Host: unmatched.example:443\r\n\r\n",
            )
            .await
            .expect("write CONNECT");
        let mut response = [0_u8; 128];
        let read = socket.read(&mut response).await.expect("read response");
        assert!(
            String::from_utf8_lossy(&response[..read]).contains("403"),
            "a rejected host prompt must remain denied"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        runner.reset().await;
    }

    /// Structural keys: domain and filesystem-path edits keep the same key (the
    /// paths differ per command root and ride the wrap's custom config), while a
    /// structural flag still moves it. Host-independent.
    #[test]
    fn structural_key_ignores_domains_and_paths_but_tracks_flags() {
        let base = to_runtime_config(&cfg_with_domains(&["github.com"]));
        let domain_changed = to_runtime_config(&cfg_with_domains(&["github.com", "npmjs.org"]));
        assert_eq!(
            structural_key(&base),
            structural_key(&domain_changed),
            "domain-only change must not move the structural key"
        );

        let mut rooted_elsewhere = cfg_with_domains(&["github.com"]);
        rooted_elsewhere.filesystem.allow_write = vec!["/repo/worktrees/p1".into()];
        rooted_elsewhere.filesystem.deny_read = vec!["/etc/secret".into()];
        assert_eq!(
            structural_key(&base),
            structural_key(&to_runtime_config(&rooted_elsewhere)),
            "a per-root path change must not reset the proxies other commands use"
        );

        let mut weaker = cfg_with_domains(&["github.com"]);
        weaker.enable_weaker_nested_sandbox = !weaker.enable_weaker_nested_sandbox;
        assert_ne!(
            structural_key(&base),
            structural_key(&to_runtime_config(&weaker)),
            "a structural flag change must still move the key"
        );
    }

    /// A `strictAllowlist` flip is STRUCTURAL: it must move the key (reset +
    /// re-init picks up the new bit) rather than being mistaken for a no-op —
    /// `active_net` compares only the domain lists, so nothing else would
    /// propagate the change to the running proxies. Host-independent.
    #[test]
    fn structural_key_tracks_strict_allowlist() {
        let base = to_runtime_config(&cfg_with_domains(&["github.com"]));
        let mut strict_cfg = cfg_with_domains(&["github.com"]);
        strict_cfg.network.strict_allowlist = true;
        let strict = to_runtime_config(&strict_cfg);
        assert_ne!(
            structural_key(&base),
            structural_key(&strict),
            "a strictAllowlist change must move the structural key"
        );
    }

    /// `cleanup_after_command` and `reset` on a never-wrapped runner are no-ops
    /// (no manager, empty mount points) and must not panic — host-independent.
    #[tokio::test]
    async fn cleanup_and_reset_idle_are_noops() {
        let runner = SandboxRuntimeRunner::new();
        runner.cleanup_after_command().await;
        runner.reset().await;
        let state = runner.state.lock().await;
        assert!(state.manager.is_none());
        assert!(state.mount_points.is_empty());
    }

    #[tokio::test]
    async fn command_violations_reads_live_manager_store() {
        let runner = SandboxRuntimeRunner::new();
        {
            let mut state = runner.state.lock().await;
            let manager = state.manager.get_or_insert_with(SandboxManager::new);
            manager
                .violation_store()
                .add_violation(sandbox_runtime::Violation {
                    encoded_command: sandbox_runtime::env::encode_sandboxed_command("echo hi"),
                    line: Some(
                        "deny network-outbound example.com:443 (host is not on the allow list)"
                            .into(),
                    ),
                    ..Default::default()
                });
        }
        let got = runner.command_violations("echo hi").await;
        assert_eq!(
            got.lines,
            vec!["deny network-outbound example.com:443 (host is not on the allow list)"]
        );
    }
}

/// Reuse the sandbox's authenticated HTTP(S) CONNECT and NO_PROXY implementation
/// for in-process Monitor WebSockets. Destination DNS is vetted by HttpTransport.
pub struct MonitorProxyConnector;

#[async_trait::async_trait]
impl tool_api::MonitorWebSocketProxy for MonitorProxyConnector {
    async fn connect_proxy(
        &self,
        host: &str,
        port: u16,
        secure: bool,
    ) -> std::io::Result<Option<Box<dyn tool_api::MonitorSocketIo>>> {
        let environment = std::env::vars().collect();
        connect_monitor_proxy(host, port, secure, &environment).await
    }
}

async fn connect_monitor_proxy(
    host: &str,
    port: u16,
    secure: bool,
    environment: &std::collections::HashMap<String, String>,
) -> std::io::Result<Option<Box<dyn tool_api::MonitorSocketIo>>> {
    use sandbox_runtime::parent_proxy::{
        connect_via_parent_proxy, resolve_parent_proxy, select_parent_proxy_url,
        should_bypass_parent_proxy,
    };
    let Some(proxy) = resolve_parent_proxy(None, environment) else {
        return Ok(None);
    };
    if should_bypass_parent_proxy(&proxy, host) {
        return Ok(None);
    }
    let Some(url) = select_parent_proxy_url(&proxy, secure) else {
        return Ok(None);
    };
    let tunnel = connect_via_parent_proxy(url, host, port).await?;
    Ok(Some(Box::new(tunnel)))
}

#[cfg(test)]
mod monitor_proxy_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    #[tokio::test]
    async fn monitor_proxy_tunnels_with_auth_and_honors_no_proxy() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                header.push(socket.read_u8().await.unwrap());
                assert!(header.len() < 16384);
            }
            let header = String::from_utf8(header).unwrap();
            assert!(
                header.starts_with("CONNECT events.example.com:443 HTTP/1.1\r\n"),
                "{header}"
            );
            assert!(
                header.contains("Proxy-Authorization: Basic dTpw"),
                "{header}"
            );
            socket
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\nhello")
                .await
                .unwrap();
        });
        let mut env = std::collections::HashMap::from([(
            "HTTPS_PROXY".into(),
            format!("http://u:p@{address}"),
        )]);
        let mut tunnel = connect_monitor_proxy("events.example.com", 443, true, &env)
            .await
            .unwrap()
            .unwrap();
        let mut payload = [0; 5];
        tunnel.read_exact(&mut payload).await.unwrap();
        assert_eq!(&payload, b"hello");
        server.await.unwrap();
        env.insert("NO_PROXY".into(), "example.com".into());
        assert!(
            connect_monitor_proxy("events.example.com", 443, true, &env)
                .await
                .unwrap()
                .is_none()
        );
    }
}
