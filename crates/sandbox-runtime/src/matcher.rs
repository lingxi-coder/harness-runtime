//! The allow/deny brain (`sandbox-manager.js:46-119`). [`filter_network_request`]
//! is the synchronous, ask-callback-free core (absent callback ⇒ deny, which is
//! the unmatched default anyway). [`filter_network_request_with_ask`] is the
//! full async port that consults the interactive [`AskFn`] for the unmatched
//! case — unless `strict_allowlist` is set, which denies unmatched hosts
//! deterministically before the callback (2.1.219) — the security gate the
//! `SandboxManager` wires into the running proxies.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::config::NetworkConfig;
use crate::host::{canonicalize_host, is_valid_host, strip_brackets};

/// Denial reason for an invalid/malformed host byte sequence.
pub const NETWORK_REASON_MALFORMED_HOST: &str = "malformed host";
/// Denial reason when the host matches an explicit deny rule.
pub const NETWORK_REASON_DENY_LIST: &str = "host is on the deny list";
/// Denial reason when no allow rule matches.
pub const NETWORK_REASON_NOT_ON_ALLOW_LIST: &str = "host is not on the allow list";
/// Denial reason when the interactive callback refused the request.
pub const NETWORK_REASON_USER_DENIED: &str = "user denied";
/// Denial reason when the interactive callback failed.
pub const NETWORK_REASON_PERMISSION_PROMPT_FAILED: &str = "permission prompt failed";

/// Final allow/deny decision for one network request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkRequestDecision {
    /// The request is allowed to proceed.
    Allow,
    /// The request is denied with the surfaced reason string.
    Deny(&'static str),
}

impl NetworkRequestDecision {
    /// Whether the request may proceed.
    #[must_use]
    pub fn is_allowed(self) -> bool {
        matches!(self, Self::Allow)
    }

    /// The surfaced denial reason, when denied.
    #[must_use]
    pub fn denied_reason(self) -> Option<&'static str> {
        match self {
            Self::Allow => None,
            Self::Deny(reason) => Some(reason),
        }
    }
}

/// Error returned by an [`AskFn`] callback when it cannot reach a decision (the
/// Rust analogue of the TS `sandboxAskCallback` throwing). The manager treats
/// any such error as a denial (`filterNetworkRequest`'s `catch` returns
/// `false`), matching `sandbox-manager.js:114-119`.
#[derive(Debug)]
pub struct AskCallbackError(pub String);

impl std::fmt::Display for AskCallbackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AskCallbackError {}

/// The interactive permission callback (`sandboxAskCallback` in
/// `sandbox-manager.js`). Invoked only for the *unmatched* case — a host that no
/// `allowed_domains`/`denied_domains` rule decides. Receives `(host, port)` and
/// resolves to `Ok(true)` (allow), `Ok(false)` (deny), or `Err(_)` (treated as
/// deny — the faithful port of the TS `try/catch`).
///
/// DIVERGENCE FROM THE PLAN (documented): the plan sketches `Fn(&str, u16) ->
/// Future<Output = bool>`. The TS `filterNetworkRequest` wraps the `await
/// sandboxAskCallback(...)` in a `try/catch` and denies on *throw* — a path the
/// `ask-error → false` test must exercise. A bare `Output = bool` future has no
/// error channel, so the output is `Result<bool, AskCallbackError>`: `Err`
/// reproduces the TS `catch`-denies branch. This is strictly more faithful than
/// `Output = bool`.
pub type AskFn = Arc<
    dyn Fn(&str, u16) -> Pin<Box<dyn Future<Output = Result<bool, AskCallbackError>> + Send>>
        + Send
        + Sync,
>;

/// `matchesDomainPattern` (`sandbox-manager.js:46-61`): `*.base` ⇒
/// `host.ends_with(".base")` (never for IP literals); else case-insensitive
/// exact equality.
#[must_use]
pub fn matches_domain_pattern(hostname: &str, pattern: &str) -> bool {
    let h = hostname.to_ascii_lowercase();
    if let Some(base) = pattern.strip_prefix("*.") {
        if strip_brackets(&h).parse::<std::net::IpAddr>().is_ok() {
            return false;
        }
        let base = base.to_ascii_lowercase();
        return h.ends_with(&format!(".{base}"));
    }
    h == pattern.to_ascii_lowercase()
}

/// `filterNetworkRequest` (`sandbox-manager.js:62-119`) without the async
/// ask-callback: reject malformed hosts, canonicalize, deny-first, then allow,
/// else deny. Empty `allowed_domains` ⇒ deny-all.
///
/// The TS `filterNetworkRequest` is `async` and consults an interactive
/// `sandboxAskCallback` for the unmatched case; that is an interactive-wiring
/// concern for a later sub-project. In its absence TS denies the unmatched
/// request — identical to the final `false` returned here. `port` is not part
/// of the decision (patterns are hostname-only); it is kept for signature
/// parity with the TS source.
#[must_use]
pub fn filter_network_request(port: u16, host: &str, config: &NetworkConfig) -> bool {
    let _ = port; // not part of the decision; kept for signature parity
    if !is_valid_host(host) {
        return false;
    }
    let canonical = canonicalize_host(host).unwrap_or_else(|| host.to_string());
    for denied in &config.denied_domains {
        if matches_domain_pattern(&canonical, denied) {
            return false;
        }
    }
    for allowed in &config.allowed_domains {
        if matches_domain_pattern(&canonical, allowed) {
            return true;
        }
    }
    // No ask-callback exists on this path, so the unmatched case already
    // denies — `strict_allowlist` cannot make it stricter and is not consulted.
    false
}

/// Full decision variant of [`filter_network_request_with_ask`], including the
/// user-facing denial reason the proxy records into `<sandbox_violations>`.
pub async fn filter_network_request_with_ask_decision(
    port: u16,
    host: &str,
    config: &NetworkConfig,
    ask: Option<&AskFn>,
) -> NetworkRequestDecision {
    if !is_valid_host(host) {
        let quoted = serde_json::to_string(host).unwrap_or_else(|_| format!("{host:?}"));
        yo(&format!("Denying malformed host: {quoted}:{port}"));
        return NetworkRequestDecision::Deny(NETWORK_REASON_MALFORMED_HOST);
    }
    let canonical = canonicalize_host(host).unwrap_or_else(|| host.to_string());
    for denied in &config.denied_domains {
        if matches_domain_pattern(&canonical, denied) {
            yo(&format!("Denied by config rule: {host}:{port}"));
            return NetworkRequestDecision::Deny(NETWORK_REASON_DENY_LIST);
        }
    }
    for allowed in &config.allowed_domains {
        if matches_domain_pattern(&canonical, allowed) {
            yo(&format!("Allowed by config rule: {host}:{port}"));
            return NetworkRequestDecision::Allow;
        }
    }
    let strict = config.strict_allowlist == Some(true);
    let Some(cb) = ask.filter(|_| !strict) else {
        yo(&format!("No matching config rule, denying: {host}:{port}"));
        return NetworkRequestDecision::Deny(NETWORK_REASON_NOT_ON_ALLOW_LIST);
    };
    yo(&format!(
        "No matching config rule, asking user: {host}:{port}"
    ));
    match cb(host, port).await {
        Ok(true) => {
            yo(&format!("User allowed: {host}:{port}"));
            NetworkRequestDecision::Allow
        }
        Ok(false) => {
            yo(&format!("User denied: {host}:{port}"));
            NetworkRequestDecision::Deny(NETWORK_REASON_USER_DENIED)
        }
        Err(e) => {
            yo(&format!("Error in permission callback: {e}"));
            NetworkRequestDecision::Deny(NETWORK_REASON_PERMISSION_PROMPT_FAILED)
        }
    }
}

/// The stderr line `yo(...)` emits for `msg`, or `None` when it stays silent.
///
/// 2.1.220 `yo` @229781624:
///
/// ```text
/// function yo(e,t){if(!process.env.SRT_DEBUG)return;let r=t?.level||"info",
/// n="[SandboxDebug]";switch(r){case"error":console.error(`${n} ${e}`);break;
/// case"warn":console.warn(`${n} ${e}`);break;default:console.error(`${n} ${e}`)}}
/// ```
///
/// Nothing is emitted without `SRT_DEBUG`, every level lands on stderr
/// (`console.warn` is stderr under Node too), and the level only picks the
/// console method: the rendered line is `[SandboxDebug] ` + the message either
/// way, so the port carries no level at all.
fn sandbox_debug_line(srt_debug: bool, msg: &str) -> Option<String> {
    srt_debug.then(|| format!("[SandboxDebug] {msg}"))
}

/// `yo(...)` — the only channel `srt` logs network-filter decisions on.
///
/// Deliberately NOT `tracing`: these lines must be invisible unless the user
/// asked for them with `SRT_DEBUG`, and a `tracing` event at `error`/`warn`
/// would clear the CLI's default `warn` stderr filter in `--print` / `--no-tui`
/// mode, printing where claude-code prints nothing.
fn yo(msg: &str) {
    #[cfg(test)]
    tests::record_yo(msg);
    if let Some(line) = sandbox_debug_line(std::env::var_os("SRT_DEBUG").is_some(), msg) {
        eprintln!("{line}");
    }
}

/// `filterNetworkRequest` WITH the interactive ask-callback path
/// (`sandbox-manager.js:62-119`; 2.1.220 `wSu` @229871400). Identical
/// deny-first/allow core as [`filter_network_request`], but the *unmatched*
/// case consults `ask`:
///
/// - malformed host (`!is_valid_host`) → `false` and **never asks** (the host
///   bytes are untrusted; we refuse before any callback);
/// - `denied_domains` match → `false`;
/// - `allowed_domains` match → `true`;
/// - unmatched + `ask = None` **or** `strict_allowlist == Some(true)` → `false`
///   and **never asks** — the 2.1.219 strict gate runs before the callback
///   (`if(!r||xl.network.strictAllowlist)` @229871903), logging the
///   deterministic-deny line;
/// - unmatched + `ask = Some(cb)` → `await cb(host, port)`; `Ok(true)` → allow,
///   `Ok(false)` or `Err(_)` → deny (the TS `try/catch` denies on throw).
///
/// Every outcome goes through the [`yo`] port, so the emitted line matches the
/// oracle byte-for-byte — including its `SRT_DEBUG` gate and `[SandboxDebug] `
/// prefix — with the original `host`, not the canonical form (the TS templates
/// interpolate `t`).
///
/// `host` (not the canonicalized form) is passed to the callback, matching the
/// TS which forwards the original `{ host, port }`.
pub async fn filter_network_request_with_ask(
    port: u16,
    host: &str,
    config: &NetworkConfig,
    ask: Option<&AskFn>,
) -> bool {
    filter_network_request_with_ask_decision(port, host, config, ask)
        .await
        .is_allowed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NetworkConfig;

    thread_local! {
        /// Every message handed to [`yo`] on this thread, recorded regardless
        /// of `SRT_DEBUG` so a test can pin BOTH that a branch routes through
        /// the gated port and the exact text it passes.
        static YO_LOG: std::cell::RefCell<Vec<String>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    pub(super) fn record_yo(msg: &str) {
        YO_LOG.with(|l| l.borrow_mut().push(msg.to_string()));
    }

    fn drain_yo() -> Vec<String> {
        YO_LOG.with(|l| std::mem::take(&mut *l.borrow_mut()))
    }

    /// `yo` @229781624 returns before printing anything unless `SRT_DEBUG` is
    /// set, and prefixes what it does print with `[SandboxDebug] `. Nothing may
    /// reach stderr on the default (unset) path — the CLI's non-TUI `warn`
    /// filter would otherwise surface a `tracing::error!` where claude-code is
    /// silent.
    #[test]
    fn debug_lines_are_srt_debug_gated_and_prefixed() {
        assert_eq!(
            sandbox_debug_line(false, "Denying malformed host: \"a b\":443"),
            None
        );
        assert_eq!(
            sandbox_debug_line(true, "Denying malformed host: \"a b\":443").as_deref(),
            Some("[SandboxDebug] Denying malformed host: \"a b\":443")
        );
    }

    /// The malformed-host and callback-error branches are the two the oracle
    /// tags `{level:"error"}` — the level picks a `console` method, it does NOT
    /// lift the `SRT_DEBUG` gate. Both must therefore go through [`yo`] with the
    /// oracle's exact message.
    #[tokio::test]
    async fn error_level_branches_still_route_through_the_gated_port() {
        let _ = drain_yo();
        assert!(!filter_network_request_with_ask(443, "bad host", &allow_example(), None).await);
        assert_eq!(
            drain_yo(),
            vec![r#"Denying malformed host: "bad host":443"#.to_string()]
        );

        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ask = ask_err(std::sync::Arc::clone(&ran));
        assert!(
            !filter_network_request_with_ask(443, "unknown.com", &allow_example(), Some(&ask))
                .await
        );
        assert_eq!(
            drain_yo(),
            vec![
                "No matching config rule, asking user: unknown.com:443".to_string(),
                "Error in permission callback: boom".to_string(),
            ]
        );
    }

    #[test]
    fn wildcard_and_exact_matching() {
        // sandbox-manager.js:46-61
        assert!(matches_domain_pattern("a.example.com", "*.example.com"));
        assert!(matches_domain_pattern("a.b.example.com", "*.example.com"));
        assert!(!matches_domain_pattern("example.com", "*.example.com")); // bare doesn't match wildcard
        assert!(matches_domain_pattern("Example.COM", "example.com")); // case-insensitive exact
        assert!(!matches_domain_pattern("evil.com", "example.com"));
        // wildcard never matches an IP literal
        assert!(!matches_domain_pattern("1.2.3.4", "*.3.4"));
    }

    #[test]
    fn deny_precedes_allow_and_empty_is_deny_all() {
        // empty allowed = deny-all
        let empty = NetworkConfig::default();
        assert!(!filter_network_request(443, "example.com", &empty));
        // allow works
        let allow = NetworkConfig {
            allowed_domains: vec!["*.example.com".into()],
            denied_domains: vec![],
            ..Default::default()
        };
        assert!(filter_network_request(443, "api.example.com", &allow));
        assert!(!filter_network_request(443, "other.com", &allow));
        // deny precedes allow
        let both = NetworkConfig {
            allowed_domains: vec!["*.example.com".into()],
            denied_domains: vec!["evil.example.com".into()],
            ..Default::default()
        };
        assert!(filter_network_request(443, "ok.example.com", &both));
        assert!(!filter_network_request(443, "evil.example.com", &both));
    }

    #[test]
    fn canonicalization_defeats_denylist_evasion() {
        // `2852039166` is the inet_aton decimal shorthand for `169.254.169.254`
        // (the cloud-metadata IP). A denylist that names only the dotted form
        // must still catch the shorthand host, because the request host is
        // canonicalized to what getaddrinfo() would dial before matching.
        // Allow everything (`*` is not a legal pattern, so allow a wide wildcard)
        // and confirm the deny entry still wins via canonicalization.
        let cfg = NetworkConfig {
            allowed_domains: vec!["*.example.com".into()],
            denied_domains: vec!["169.254.169.254".into()],
            ..Default::default()
        };
        // 2852039166 == 169.254.169.254 — the dotted denylist entry must catch it.
        assert!(!filter_network_request(80, "2852039166", &cfg));
    }

    #[test]
    fn malformed_host_denied() {
        let allow_all = NetworkConfig {
            allowed_domains: vec!["*.example.com".into()],
            denied_domains: vec![],
            ..Default::default()
        };
        assert!(!filter_network_request(
            443,
            "evil.com\u{0}.example.com",
            &allow_all
        ));
    }

    // ── filter_network_request_with_ask (sandbox-manager.js:62-119) ──

    /// Build an `AskFn` that always resolves `Ok(answer)`, recording that it ran.
    fn ask_const(answer: bool, ran: std::sync::Arc<std::sync::atomic::AtomicBool>) -> AskFn {
        std::sync::Arc::new(move |_host: &str, _port: u16| {
            let ran = std::sync::Arc::clone(&ran);
            Box::pin(async move {
                ran.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(answer)
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = _> + Send>>
        })
    }

    /// An `AskFn` that errors (the TS callback throwing).
    fn ask_err(ran: std::sync::Arc<std::sync::atomic::AtomicBool>) -> AskFn {
        std::sync::Arc::new(move |_host: &str, _port: u16| {
            let ran = std::sync::Arc::clone(&ran);
            Box::pin(async move {
                ran.store(true, std::sync::atomic::Ordering::SeqCst);
                Err(AskCallbackError("boom".into()))
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = _> + Send>>
        })
    }

    fn allow_example() -> NetworkConfig {
        NetworkConfig {
            allowed_domains: vec!["*.example.com".into()],
            denied_domains: vec!["evil.example.com".into()],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn ask_denied_returns_false_without_asking() {
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ask = ask_const(true, std::sync::Arc::clone(&ran));
        // deny rule wins before the ask path is reached.
        assert!(
            !filter_network_request_with_ask(443, "evil.example.com", &allow_example(), Some(&ask))
                .await
        );
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn ask_allowed_returns_true_without_asking() {
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ask = ask_const(false, std::sync::Arc::clone(&ran));
        assert!(
            filter_network_request_with_ask(443, "api.example.com", &allow_example(), Some(&ask))
                .await
        );
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn unmatched_no_ask_is_false() {
        assert!(!filter_network_request_with_ask(443, "unknown.com", &allow_example(), None).await);
    }

    #[tokio::test]
    async fn unmatched_ask_yes_is_true() {
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ask = ask_const(true, std::sync::Arc::clone(&ran));
        assert!(
            filter_network_request_with_ask(443, "unknown.com", &allow_example(), Some(&ask)).await
        );
        assert!(ran.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn unmatched_ask_no_is_false() {
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ask = ask_const(false, std::sync::Arc::clone(&ran));
        assert!(
            !filter_network_request_with_ask(443, "unknown.com", &allow_example(), Some(&ask))
                .await
        );
        assert!(ran.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn ask_error_is_false() {
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ask = ask_err(std::sync::Arc::clone(&ran));
        assert!(
            !filter_network_request_with_ask(443, "unknown.com", &allow_example(), Some(&ask))
                .await
        );
        assert!(ran.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// `strictAllowlist` (2.1.219): unmatched host + ask callback PRESENT +
    /// strict ⇒ deterministic deny, callback NEVER invoked — the oracle gate
    /// `if(!r||xl.network.strictAllowlist)` runs before the ask
    /// (2.1.220 @229871903).
    #[tokio::test]
    async fn unmatched_strict_denies_without_asking() {
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ask = ask_const(true, std::sync::Arc::clone(&ran));
        let cfg = NetworkConfig {
            strict_allowlist: Some(true),
            ..allow_example()
        };
        assert!(!filter_network_request_with_ask(443, "unknown.com", &cfg, Some(&ask)).await);
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// `strictAllowlist: false` is the same as absent — unmatched still asks.
    #[tokio::test]
    async fn unmatched_strict_false_still_asks() {
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ask = ask_const(true, std::sync::Arc::clone(&ran));
        let cfg = NetworkConfig {
            strict_allowlist: Some(false),
            ..allow_example()
        };
        assert!(filter_network_request_with_ask(443, "unknown.com", &cfg, Some(&ask)).await);
        assert!(ran.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// Strict mode gates only the UNMATCHED case: an `allowed_domains` match
    /// still allows (the loops run before the strict check in the oracle).
    #[tokio::test]
    async fn strict_does_not_shadow_allow_rule() {
        let cfg = NetworkConfig {
            strict_allowlist: Some(true),
            ..allow_example()
        };
        assert!(filter_network_request_with_ask(443, "api.example.com", &cfg, None).await);
    }

    #[tokio::test]
    async fn malformed_host_never_asks() {
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ask = ask_const(true, std::sync::Arc::clone(&ran));
        assert!(
            !filter_network_request_with_ask(
                443,
                "evil.com\u{0}.example.com",
                &allow_example(),
                Some(&ask)
            )
            .await
        );
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
    }
}
