//! §17 — MCP **protocol-era negotiation**: decide whether a server connection
//! uses the legacy (single-shot) handshake or an "auto" era-probing one.
//!
//! Oracle `co(e,t,o)` (cc_all.txt @~182282123, chunk-local names `co`/`Wr`/
//! `cn`) is called exactly once per connect, from `connectToServer` (`Ve`):
//! `Pe=co(Wr(t.type,{inProcess,ccrProxy}),t,X)` where `X=cn(t)` is the
//! server-denylist check precomputed once. `co` itself:
//!
//! 1. reads `MCP_PROTOCOL_NEGOTIATION`; only the literal strings `"legacy"`
//!    and `"auto"` are recognized — anything else (including `""`) logs
//!    `MCP_PROTOCOL_NEGOTIATION=<v> is invalid; expected 'legacy' or 'auto'
//!    — ignoring` at `warn` and is treated exactly as if the var were unset;
//! 2. an explicit `"legacy"` wins unconditionally: `{mode:"legacy"}`, no
//!    further checks;
//! 3. an explicit `"auto"` applies only to `Kr = {"http","claudeai-proxy",
//!    "ccr-proxy","stdio"}` — every other label is forced back to legacy
//!    even though the env var asked for auto;
//! 4. otherwise (var unset or invalid), HTTP uses its per-transport gate
//!    (default on in 2.1.286); stdio and the two proxy labels have separate
//!    gates (default off). SSE, WS, IDE, in-process and SDK-control stay legacy;
//! 5. when — and ONLY when — the FLAG-GATED decision in (4) lands on
//!    `auto`, a final server-denylist check
//!    (`tengu_mcp_negotiation_server_denylist`, an ARRAY flag: a list of
//!    hostnames, or the literal `"*"` to denylist every server) can still
//!    downgrade it to legacy, logging `MCP era negotiation denylist matched
//!    <host>; the legacy handshake applies`. Steps (2) and (3) `return`
//!    from `co` before this check exists, so an explicit
//!    `MCP_PROTOCOL_NEGOTIATION=auto` is NOT denylistable — the guard reads
//!    `h.mode`, and `h` is the gated switch's result alone.
//!
//! `ccr-proxy` is represented by the compatible HTTP spec in this port. The
//! original config discriminator is supplied separately by
//! `McpServerMetadata::transport`; arbitrary metadata labels are ignored.
//!
//! `claudeai-proxy` and `ccr-proxy` may therefore select their own feature
//! gates when metadata identifies one of those labels on an HTTP spec. Other
//! metadata values cannot spoof a transport label.
//!
//! Downstream of the resolved mode, the oracle also gates `skills-capable` /
//! `channel-capable` / `live-connection` off the NEGOTIATED protocol
//! revision returned by an `auto`-mode server. The transport performs the
//! `server/discover` probe and one pinned-revision corrective retry, while the
//! registry carries this immutable mode and probe budget through cache and
//! handshake decisions. A legacy handshake still requests 2025-11-25.

use lingxi_core::host::{McpTransportKind, McpTransportSpec};

/// `tengu_mcp_protocol_negotiation_http` — default on in 2.1.286.
const FLAG_HTTP: &str = "tengu_mcp_protocol_negotiation_http";
/// `tengu_mcp_protocol_negotiation_stdio` — default off.
const FLAG_STDIO: &str = "tengu_mcp_protocol_negotiation_stdio";
/// `tengu_mcp_protocol_negotiation_claudeai` — default off.
const FLAG_CLAUDEAI: &str = "tengu_mcp_protocol_negotiation_claudeai";
/// `tengu_mcp_protocol_negotiation_ccr` — default off. The compatible HTTP
/// spec carries this original config label in metadata.
const FLAG_CCR: &str = "tengu_mcp_protocol_negotiation_ccr";
/// `tengu_mcp_negotiation_server_denylist` — an ARRAY flag (list of
/// hostnames, or `["*"]` to denylist every server), default `[]`.
const FLAG_SERVER_DENYLIST: &str = "tengu_mcp_negotiation_server_denylist";

/// Probe-timeout cap for the `stdio` label (oracle `zr = 3000`).
const PROBE_TIMEOUT_STDIO_CAP_MS: u64 = 3_000;
/// Probe-timeout cap for every other auto-eligible label (oracle `qr = 5000`).
const PROBE_TIMEOUT_OTHER_CAP_MS: u64 = 5_000;

/// The env var this whole module reads (`a.MCP_PROTOCOL_NEGOTIATION`).
const ENV_VAR: &str = "MCP_PROTOCOL_NEGOTIATION";

/// Resolved negotiation mode for one connect attempt (oracle `{mode:...}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NegotiationMode {
    /// Single-shot handshake using the fixed legacy revision.
    Legacy,
    /// Era-probing handshake, bounded by `probe_timeout_ms` (oracle
    /// `{mode:"auto",probe:{timeoutMs}}`).
    Auto {
        /// `min(cap, floor(base_timeout_ms/3))` — see [`probe_timeout_ms`].
        probe_timeout_ms: u64,
    },
}

/// Outcome of [`resolve`]: the mode plus the (at most one of two) oracle
/// warning strings a caller should log at `warn`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiationResolution {
    /// The resolved mode.
    pub mode: NegotiationMode,
    /// Set when `MCP_PROTOCOL_NEGOTIATION` was present but neither `"legacy"`
    /// nor `"auto"` — oracle's `MCP_PROTOCOL_NEGOTIATION=<v> is invalid; …`.
    pub env_warning: Option<String>,
    /// Set when the server denylist downgraded an `Auto` decision to
    /// `Legacy` — oracle's `MCP era negotiation denylist matched <host>; …`.
    pub denylist_warning: Option<String>,
}

/// Parsed `MCP_PROTOCOL_NEGOTIATION` value (oracle's `d`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnvMode {
    Legacy,
    Auto,
}

/// Parse the raw env var value. Oracle: `r===void 0` (Rust `None`, the var
/// truly unset) never warns; any OTHER value that isn't exactly `"legacy"`/
/// `"auto"` (including `""`) warns and is treated as `None` (unset).
fn parse_env_mode(raw: Option<&str>) -> (Option<EnvMode>, Option<String>) {
    match raw {
        None => (None, None),
        Some("legacy") => (Some(EnvMode::Legacy), None),
        Some("auto") => (Some(EnvMode::Auto), None),
        Some(other) => (
            None,
            Some(format!(
                "{ENV_VAR}={other} is invalid; expected 'legacy' or 'auto' — ignoring"
            )),
        ),
    }
}

/// `Kr` — labels an explicit `MCP_PROTOCOL_NEGOTIATION=auto` can actually
/// engage for. `claudeai-proxy`/`ccr-proxy` are accepted only when the
/// compatible HTTP spec carries those original config labels in metadata.
fn env_auto_eligible(label: &str) -> bool {
    matches!(label, "http" | "claudeai-proxy" | "ccr-proxy" | "stdio")
}

/// `min(cap, floor(base_timeout_ms/3))`, capped at 3000ms for `stdio` and
/// 5000ms for every other label (oracle `zr`/`qr`). Integer division already
/// floors for non-negative operands, matching `Math.floor`.
fn probe_timeout_ms(label: &str, base_timeout_ms: u64) -> u64 {
    let cap = if label == "stdio" {
        PROBE_TIMEOUT_STDIO_CAP_MS
    } else {
        PROBE_TIMEOUT_OTHER_CAP_MS
    };
    cap.min(base_timeout_ms / 3)
}

/// The per-transport gate applied when the env var is unset/invalid (oracle's
/// `h=(()=>{switch(e){...}})()`).
fn gated_mode(label: &str, base_timeout_ms: u64) -> NegotiationMode {
    let auto = || NegotiationMode::Auto {
        probe_timeout_ms: probe_timeout_ms(label, base_timeout_ms),
    };
    let gate_on = |flag: &str| telemetry::flag_bool(flag, false);
    match label {
        "http" if telemetry::flag_bool(FLAG_HTTP, true) => auto(),
        "stdio" if gate_on(FLAG_STDIO) => auto(),
        "claudeai-proxy" if gate_on(FLAG_CLAUDEAI) => auto(),
        "ccr-proxy" if gate_on(FLAG_CCR) => auto(),
        // Either the gate above was off, or the label is one of
        // `sse` / `ws` / `ide` / `in-process` / `sdk-control` (always
        // legacy), or an unrecognized label (never produced by
        // `transport_label`, kept only so the match is exhaustive).
        _ => NegotiationMode::Legacy,
    }
}

/// `Wr(transportType, {inProcess, ccrProxy})` restricted to the concrete
/// [`McpTransportSpec`] kinds in this port. Original proxy labels are applied
/// by [`transport_label_for_spec`] only for compatible HTTP specs.
fn transport_label(kind: McpTransportKind) -> &'static str {
    match kind {
        McpTransportKind::InProcess => "in-process",
        McpTransportKind::Http => "http",
        McpTransportKind::Sse => "sse",
        McpTransportKind::WebSocket => "ws",
        McpTransportKind::SseIde => "ide",
        // Oracle `Wr`: `sse-ide`/`ws-ide` both map to the `"ide"` label.
        McpTransportKind::WsIde => "ide",
        McpTransportKind::SdkControl => "sdk-control",
        McpTransportKind::Stdio => "stdio",
    }
}

fn transport_label_for_spec(
    spec: &McpTransportSpec,
    metadata_transport: Option<&str>,
) -> &'static str {
    if matches!(spec, McpTransportSpec::Http { .. }) {
        match metadata_transport {
            Some("claudeai-proxy") => return "claudeai-proxy",
            Some("ccr-proxy") => return "ccr-proxy",
            _ => {}
        }
    }
    transport_label(spec.transport_kind())
}

/// The static `url` a spec carries, if any (only the remote transports have
/// one). Mirrors the oracle's `"url"in t&&typeof t.url==="string"` guard.
fn spec_url(spec: &McpTransportSpec) -> Option<&str> {
    match spec {
        McpTransportSpec::Sse { url, .. }
        | McpTransportSpec::Http { url, .. }
        | McpTransportSpec::WebSocket { url, .. }
        | McpTransportSpec::SseIde { url, .. }
        | McpTransportSpec::WsIde { url, .. } => Some(url.as_str()),
        McpTransportSpec::Stdio { .. }
        | McpTransportSpec::InProcess { .. }
        | McpTransportSpec::SdkControl { .. } => None,
    }
}

/// `cn(t)` / `Ot(e,t)` (cc_all.txt @~182279107): does `url`'s hostname fall
/// under the denylist ARRAY flag — either listed explicitly (exact or
/// suffix-of-a-dot match, case-insensitive) or wildcarded via a bare `"*"`
/// entry (which also denylists a url-less server, e.g. `stdio`)?
fn denylist_matches(url: Option<&str>, denylist: &[String]) -> bool {
    if denylist.is_empty() {
        return false;
    }
    if denylist.iter().any(|d| d == "*") {
        return true;
    }
    let Some(url) = url else {
        return false;
    };
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.to_lowercase();
    denylist.iter().any(|d| {
        if d.is_empty() {
            return false;
        }
        let d = d.to_lowercase();
        host == d || host.ends_with(&format!(".{d}"))
    })
}

/// The oracle's denylist-match display text — a re-parse of `url` purely for
/// the log line, independent of whether the parse also drove the match
/// (a url-less server can only ever match via the `"*"` wildcard).
fn denylist_match_display(url: Option<&str>) -> String {
    match url {
        None => "a url-less server (the '*' entry)".to_string(),
        Some(u) => match url::Url::parse(u) {
            Ok(parsed) => parsed.host_str().unwrap_or("").to_string(),
            Err(_) => "a server with an unparseable url".to_string(),
        },
    }
}

/// Pure core: `co(label, url, base_timeout_ms, env_raw, denylist_override)`.
/// `denylist_override`, when `Some`, replaces the flag-driven denylist check
/// entirely (oracle's `o??cn(t)`) — used by tests; the real caller
/// ([`resolve_for_spec`]) always passes `None`. Note the override is only
/// consulted on the flag-gated path, exactly where the oracle consults
/// `o??cn(t)`.
fn resolve(
    label: &str,
    url: Option<&str>,
    base_timeout_ms: u64,
    env_raw: Option<&str>,
    denylist_override: Option<bool>,
) -> NegotiationResolution {
    let (env_mode, env_warning) = parse_env_mode(env_raw);

    // Oracle `co`: BOTH explicit-env branches `return` before the denylist
    // block is ever reached — `if(d==="legacy")return{mode:"legacy"}` and,
    // inside `if(d==="auto"){...}`, `return e==="stdio"?{mode:"auto",probe:_}:
    // {mode:"auto",probe:u}`. The denylist guard then tests `h.mode`, where
    // `h` is the FLAG-GATED switch's result only. So an operator who sets
    // `MCP_PROTOCOL_NEGOTIATION=auto` overrides the denylist, and the
    // denylist can only ever downgrade a decision the feature flags made.
    if let Some(env_mode) = env_mode {
        let mode = match env_mode {
            EnvMode::Legacy => NegotiationMode::Legacy,
            EnvMode::Auto if env_auto_eligible(label) => NegotiationMode::Auto {
                probe_timeout_ms: probe_timeout_ms(label, base_timeout_ms),
            },
            EnvMode::Auto => NegotiationMode::Legacy,
        };
        return NegotiationResolution {
            mode,
            env_warning,
            denylist_warning: None,
        };
    }

    let mode = gated_mode(label, base_timeout_ms);
    if !matches!(mode, NegotiationMode::Auto { .. }) {
        return NegotiationResolution {
            mode,
            env_warning,
            denylist_warning: None,
        };
    }

    let denylisted = denylist_override.unwrap_or_else(|| {
        denylist_matches(url, &telemetry::flag_string_list(FLAG_SERVER_DENYLIST, &[]))
    });
    if denylisted {
        let host = denylist_match_display(url);
        NegotiationResolution {
            mode: NegotiationMode::Legacy,
            env_warning,
            denylist_warning: Some(format!(
                "MCP era negotiation denylist matched {host}; the legacy handshake applies"
            )),
        }
    } else {
        NegotiationResolution {
            mode,
            env_warning,
            denylist_warning: None,
        }
    }
}

/// Real entry point: resolve the negotiation mode for `spec`, reading the
/// live env var and feature flags, and logging both oracle warnings via
/// `tracing::warn!` exactly as they occur (at most one of each per call).
///
/// `base_timeout_ms` is the connect+initialize deadline already computed by
/// the caller (`registry::mcp_connection_timeout()` — oracle `Fc()`), passed
/// in rather than re-read here so this stays a single source of truth.
#[must_use]
pub fn resolve_for_spec(spec: &McpTransportSpec, base_timeout_ms: u64) -> NegotiationMode {
    resolve_for_spec_with_transport(spec, None, base_timeout_ms)
}

/// Resolve one immutable negotiation decision while retaining an original
/// proxy discriminator carried in MCP config metadata. Only the compatible
/// HTTP spec may override its enum-derived label; arbitrary metadata cannot
/// turn stdio, SSE, or an unrelated transport into a proxy.
#[must_use]
pub fn resolve_for_spec_with_transport(
    spec: &McpTransportSpec,
    metadata_transport: Option<&str>,
    base_timeout_ms: u64,
) -> NegotiationMode {
    let label = transport_label_for_spec(spec, metadata_transport);
    let url = spec_url(spec);
    let env_raw = std::env::var(ENV_VAR).ok();
    let resolution = resolve(label, url, base_timeout_ms, env_raw.as_deref(), None);
    if let Some(w) = &resolution.env_warning {
        tracing::warn!("{w}");
    }
    if let Some(w) = &resolution.denylist_warning {
        tracing::warn!("{w}");
    }
    resolution.mode
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `test_set_flag`/`test_clear_flag` (and `std::env::set_var`/
    /// `remove_var` for `MCP_PROTOCOL_NEGOTIATION`) mutate process-global
    /// state; cargo runs a crate's unit tests on multiple threads by
    /// default, so any two of THIS module's flag-mutating tests can
    /// interleave and flip each other's flag mid-assertion. Every test below
    /// that touches `FLAG_HTTP`/`FLAG_CLAUDEAI`/`FLAG_CCR` or `ENV_VAR` holds
    /// this lock for its whole body so they run serially with respect to
    /// each other (verified flaky without it — see the batch report).
    fn flag_test_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    fn http_spec(url: &str) -> McpTransportSpec {
        McpTransportSpec::Http {
            url: url.to_string(),
            headers: lingxi_core::host::McpHeaders::new(),
            headers_helper: None,
            oauth: None,
        }
    }

    // ── env parsing ──────────────────────────────────────────────────────

    #[test]
    fn env_unset_is_none_with_no_warning() {
        assert_eq!(parse_env_mode(None), (None, None));
    }

    #[test]
    fn env_legacy_and_auto_parse_clean() {
        assert_eq!(
            parse_env_mode(Some("legacy")),
            (Some(EnvMode::Legacy), None)
        );
        assert_eq!(parse_env_mode(Some("auto")), (Some(EnvMode::Auto), None));
    }

    #[test]
    fn env_invalid_value_warns_with_byte_exact_message() {
        let (mode, warning) = parse_env_mode(Some("bogus"));
        assert_eq!(mode, None);
        assert_eq!(
            warning.as_deref(),
            Some(
                "MCP_PROTOCOL_NEGOTIATION=bogus is invalid; expected 'legacy' or 'auto' — ignoring"
            )
        );
    }

    #[test]
    fn env_empty_string_is_invalid_not_unset() {
        // Oracle: r="" !== void 0, so an explicitly-empty env var still warns
        // (distinct from the var being absent entirely).
        let (mode, warning) = parse_env_mode(Some(""));
        assert_eq!(mode, None);
        assert_eq!(
            warning.as_deref(),
            Some("MCP_PROTOCOL_NEGOTIATION= is invalid; expected 'legacy' or 'auto' — ignoring")
        );
    }

    // ── Kr / probe timeout ───────────────────────────────────────────────

    #[test]
    fn auto_eligible_set_matches_kr() {
        for label in ["http", "claudeai-proxy", "ccr-proxy", "stdio"] {
            assert!(env_auto_eligible(label), "{label} should be Kr-eligible");
        }
        for label in ["sse", "ws", "ide", "in-process", "sdk-control"] {
            assert!(
                !env_auto_eligible(label),
                "{label} should NOT be Kr-eligible"
            );
        }
    }

    #[test]
    fn probe_timeout_caps_by_label_at_default_base() {
        // base=30000 (default MCP_TIMEOUT) => floor(30000/3)=10000, above
        // both caps, so each label reports its own cap verbatim.
        assert_eq!(probe_timeout_ms("stdio", 30_000), 3_000);
        assert_eq!(probe_timeout_ms("http", 30_000), 5_000);
    }

    #[test]
    fn probe_timeout_floors_below_the_cap_for_a_small_base() {
        // base=9000 => floor(9000/3)=3000: stdio's own 3000 cap ties it;
        // http's 5000 cap is now ABOVE the floor, so the floor wins.
        assert_eq!(probe_timeout_ms("stdio", 9_000), 3_000);
        assert_eq!(probe_timeout_ms("http", 9_000), 3_000);

        // base=100 => floor(100/3)=33, under both caps.
        assert_eq!(probe_timeout_ms("stdio", 100), 33);
        assert_eq!(probe_timeout_ms("http", 100), 33);
    }

    // ── transport_label / Wr ─────────────────────────────────────────────

    #[test]
    fn transport_label_matches_wr_for_every_kind_in_this_port() {
        assert_eq!(transport_label(McpTransportKind::Stdio), "stdio");
        assert_eq!(transport_label(McpTransportKind::Sse), "sse");
        assert_eq!(transport_label(McpTransportKind::Http), "http");
        assert_eq!(transport_label(McpTransportKind::WebSocket), "ws");
        assert_eq!(transport_label(McpTransportKind::InProcess), "in-process");
        assert_eq!(transport_label(McpTransportKind::SseIde), "ide");
        assert_eq!(transport_label(McpTransportKind::WsIde), "ide");
        assert_eq!(transport_label(McpTransportKind::SdkControl), "sdk-control");
    }

    // ── gated_mode (env unset) ───────────────────────────────────────────

    #[test]
    fn gated_mode_stdio_and_local_transports_are_always_legacy() {
        let _guard = flag_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        for label in ["stdio", "sse", "ws", "ide", "in-process", "sdk-control"] {
            telemetry::test_set_flag(FLAG_HTTP, true);
            telemetry::test_set_flag(FLAG_CLAUDEAI, true);
            telemetry::test_set_flag(FLAG_CCR, true);
            assert_eq!(
                gated_mode(label, 30_000),
                NegotiationMode::Legacy,
                "{label} must stay legacy even with every remote gate forced on"
            );
            telemetry::test_clear_flag(FLAG_HTTP);
            telemetry::test_clear_flag(FLAG_CLAUDEAI);
            telemetry::test_clear_flag(FLAG_CCR);
        }
    }

    #[test]
    fn gated_mode_http_follows_its_own_flag() {
        let _guard = flag_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        telemetry::test_clear_flag(FLAG_HTTP);
        assert_eq!(
            gated_mode("http", 30_000),
            NegotiationMode::Auto {
                probe_timeout_ms: 5_000
            }
        );
        telemetry::test_set_flag(FLAG_HTTP, false);
        assert_eq!(gated_mode("http", 30_000), NegotiationMode::Legacy);
        telemetry::test_set_flag(FLAG_HTTP, true);
        assert_eq!(
            gated_mode("http", 30_000),
            NegotiationMode::Auto {
                probe_timeout_ms: 5_000
            }
        );
        telemetry::test_clear_flag(FLAG_HTTP);
    }

    #[test]
    fn gated_mode_stdio_has_its_own_default_off_flag() {
        let _guard = flag_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        telemetry::test_clear_flag(FLAG_STDIO);
        assert_eq!(gated_mode("stdio", 30_000), NegotiationMode::Legacy);
        telemetry::test_set_flag(FLAG_STDIO, true);
        assert_eq!(
            gated_mode("stdio", 30_000),
            NegotiationMode::Auto {
                probe_timeout_ms: 3_000
            }
        );
        telemetry::test_clear_flag(FLAG_STDIO);
    }

    #[test]
    fn policy_matches_pinned_upstream_execution() {
        let _guard = flag_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../platforms/posix/tests/fixtures/mcp_negotiation_286_oracle.json"
        ))
        .unwrap();
        for case in fixture["policyCases"].as_array().unwrap() {
            for flag in [FLAG_HTTP, FLAG_STDIO, FLAG_CLAUDEAI, FLAG_CCR] {
                telemetry::test_clear_flag(flag);
            }
            if let Some(flags) = case["flags"].as_object() {
                for (flag, value) in flags {
                    telemetry::test_set_flag(flag, value.as_bool().unwrap());
                }
            }
            let out = resolve(
                case["transport"].as_str().unwrap(),
                case.pointer("/server/url")
                    .and_then(serde_json::Value::as_str),
                case["timeout"].as_u64().unwrap_or(30_000),
                case["environment"].as_str(),
                Some(case["denylisted"].as_bool().unwrap_or(false)),
            );
            let policy = match out.mode {
                NegotiationMode::Legacy => serde_json::json!({"mode":"legacy"}),
                NegotiationMode::Auto { probe_timeout_ms } => {
                    serde_json::json!({"mode":"auto", "probe":{"timeoutMs":probe_timeout_ms}})
                }
            };
            assert_eq!(policy, case["expected"]["policy"], "{}", case["name"]);
            let warnings: Vec<String> = out
                .env_warning
                .into_iter()
                .chain(out.denylist_warning)
                .collect();
            assert_eq!(
                serde_json::json!(warnings),
                case["expected"]["warnings"],
                "{}",
                case["name"]
            );
        }
        for flag in [FLAG_HTTP, FLAG_STDIO, FLAG_CLAUDEAI, FLAG_CCR] {
            telemetry::test_clear_flag(flag);
        }
    }

    // ── denylist ─────────────────────────────────────────────────────────

    #[test]
    fn denylist_empty_never_matches() {
        assert!(!denylist_matches(Some("https://mcp.example.com"), &[]));
        assert!(!denylist_matches(None, &[]));
    }

    #[test]
    fn denylist_wildcard_matches_everything_including_url_less() {
        let list = vec!["*".to_string()];
        assert!(denylist_matches(Some("https://mcp.example.com"), &list));
        assert!(denylist_matches(None, &list));
    }

    #[test]
    fn denylist_exact_and_suffix_hostname_match_case_insensitively() {
        let list = vec!["Example.COM".to_string()];
        assert!(denylist_matches(Some("https://EXAMPLE.com/mcp"), &list));
        assert!(denylist_matches(Some("https://sub.example.com/mcp"), &list));
        assert!(!denylist_matches(Some("https://notexample.com/mcp"), &list));
    }

    #[test]
    fn denylist_unparseable_or_url_less_without_wildcard_never_matches() {
        let list = vec!["example.com".to_string()];
        assert!(!denylist_matches(Some("not a url"), &list));
        assert!(!denylist_matches(None, &list));
    }

    #[test]
    fn denylist_display_text_covers_all_three_cases() {
        assert_eq!(
            denylist_match_display(Some("https://mcp.example.com/x")),
            "mcp.example.com"
        );
        assert_eq!(
            denylist_match_display(None),
            "a url-less server (the '*' entry)"
        );
        assert_eq!(
            denylist_match_display(Some("not a url")),
            "a server with an unparseable url"
        );
    }

    // ── resolve() — full integration ─────────────────────────────────────

    #[test]
    fn resolve_env_legacy_wins_even_over_an_eligible_auto_flag() {
        let _guard = flag_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        telemetry::test_set_flag(FLAG_HTTP, true);
        let out = resolve("http", None, 30_000, Some("legacy"), None);
        assert_eq!(out.mode, NegotiationMode::Legacy);
        assert_eq!(out.env_warning, None);
        assert_eq!(out.denylist_warning, None);
        telemetry::test_clear_flag(FLAG_HTTP);
    }

    #[test]
    fn resolve_env_auto_on_eligible_label_returns_auto_with_probe_timeout() {
        let out = resolve("http", None, 30_000, Some("auto"), None);
        assert_eq!(
            out.mode,
            NegotiationMode::Auto {
                probe_timeout_ms: 5_000
            }
        );
    }

    #[test]
    fn resolve_env_auto_on_ineligible_label_falls_back_to_legacy() {
        let out = resolve("sse", None, 30_000, Some("auto"), None);
        assert_eq!(out.mode, NegotiationMode::Legacy);
    }

    #[test]
    fn resolve_invalid_env_warns_and_falls_through_to_the_gate() {
        let _guard = flag_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        telemetry::test_clear_flag(FLAG_HTTP);
        let out = resolve("http", None, 30_000, Some("nonsense"), None);
        assert_eq!(
            out.mode,
            NegotiationMode::Auto {
                probe_timeout_ms: 5_000
            }
        );
        assert_eq!(
            out.env_warning.as_deref(),
            Some("MCP_PROTOCOL_NEGOTIATION=nonsense is invalid; expected 'legacy' or 'auto' — ignoring")
        );
    }

    #[test]
    fn resolve_denylist_override_downgrades_the_gated_auto_to_legacy_with_log() {
        // env UNSET: the only path the oracle ever applies the denylist to.
        let _guard = flag_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        telemetry::test_set_flag(FLAG_HTTP, true);
        let out = resolve(
            "http",
            Some("https://blocked.example.com/mcp"),
            30_000,
            None,
            Some(true),
        );
        telemetry::test_clear_flag(FLAG_HTTP);
        assert_eq!(out.mode, NegotiationMode::Legacy);
        assert_eq!(
            out.denylist_warning.as_deref(),
            Some("MCP era negotiation denylist matched blocked.example.com; the legacy handshake applies")
        );
    }

    #[test]
    fn resolve_denylist_override_false_leaves_the_gated_auto_untouched() {
        let _guard = flag_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        telemetry::test_set_flag(FLAG_HTTP, true);
        let out = resolve("http", None, 30_000, None, Some(false));
        telemetry::test_clear_flag(FLAG_HTTP);
        assert_eq!(
            out.mode,
            NegotiationMode::Auto {
                probe_timeout_ms: 5_000
            }
        );
        assert_eq!(out.denylist_warning, None);
    }

    /// Oracle `co`'s `if(d==="auto"){...return...}` block RETURNS before the
    /// denylist guard exists, and that guard then tests `h.mode` — the
    /// flag-gated result alone. So an operator's explicit
    /// `MCP_PROTOCOL_NEGOTIATION=auto` beats the denylist: mode stays `auto`
    /// with the 5000ms probe budget and NOTHING is logged.
    #[test]
    fn resolve_explicit_env_auto_is_never_downgraded_by_the_denylist() {
        let out = resolve(
            "http",
            Some("https://blocked.example.com/mcp"),
            30_000,
            Some("auto"),
            Some(true),
        );
        assert_eq!(
            out.mode,
            NegotiationMode::Auto {
                probe_timeout_ms: 5_000
            },
            "an explicit MCP_PROTOCOL_NEGOTIATION=auto outranks the denylist"
        );
        assert_eq!(out.denylist_warning, None);
        // `stdio` (also in Kr) takes the same early return, with its own cap.
        let out = resolve("stdio", None, 30_000, Some("auto"), Some(true));
        assert_eq!(
            out.mode,
            NegotiationMode::Auto {
                probe_timeout_ms: 3_000
            }
        );
        assert_eq!(out.denylist_warning, None);
    }

    /// An explicit `legacy` also returns early — the denylist never runs and
    /// therefore never logs, even for a denylisted host.
    #[test]
    fn resolve_explicit_env_legacy_never_consults_the_denylist() {
        let out = resolve(
            "http",
            Some("https://blocked.example.com/mcp"),
            30_000,
            Some("legacy"),
            Some(true),
        );
        assert_eq!(out.mode, NegotiationMode::Legacy);
        assert_eq!(out.denylist_warning, None);
    }

    // ── resolve_for_spec — the real entry point ─────────────────────────

    #[test]
    fn resolve_for_spec_stdio_is_always_legacy_regardless_of_flags() {
        let _guard = flag_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        telemetry::test_set_flag(FLAG_HTTP, true);
        std::env::remove_var(ENV_VAR);
        let spec = McpTransportSpec::Stdio {
            command: "echo".to_string(),
            args: vec![],
            env: std::collections::HashMap::new(),
        };
        assert_eq!(resolve_for_spec(&spec, 30_000), NegotiationMode::Legacy);
        telemetry::test_clear_flag(FLAG_HTTP);
    }

    /// The `denylist_override` used by the tests above short-circuits the
    /// ONE production call site of `telemetry::flag_string_list`, so without
    /// this test nothing in the tree ever reads `FLAG_SERVER_DENYLIST`
    /// through the real reader: a misspelled key, a wrong `FeatureValue`
    /// arm, or an override layer consulted in the wrong order would all stay
    /// green. This drives `resolve_for_spec` end to end — live env var, live
    /// boolean gate, live ARRAY flag.
    ///
    /// It writes the flag under the ORACLE's literal key rather than through
    /// `FLAG_SERVER_DENYLIST`: routing both sides through the same constant
    /// would make the test agree with any misspelling of it (verified — a
    /// deliberately corrupted constant left the whole module green).
    #[test]
    fn resolve_for_spec_denylist_flag_downgrades_the_gated_auto_mode() {
        // Oracle `cn(e)=Ot("tengu_mcp_negotiation_server_denylist",e)`.
        const ORACLE_DENYLIST_FLAG: &str = "tengu_mcp_negotiation_server_denylist";
        let _guard = flag_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(ENV_VAR);
        telemetry::test_set_flag(FLAG_HTTP, true);
        telemetry::test_clear_flag_list(ORACLE_DENYLIST_FLAG);
        let spec = http_spec("https://mcp.example.com/v1");

        // Baseline: the gate alone yields auto, so any Legacy below is the
        // denylist's doing and not the gate's.
        assert_eq!(
            resolve_for_spec(&spec, 30_000),
            NegotiationMode::Auto {
                probe_timeout_ms: 5_000
            },
            "flag on + empty denylist must resolve to auto"
        );

        // A non-matching host leaves it alone.
        telemetry::test_set_flag_list(ORACLE_DENYLIST_FLAG, vec!["other.example".to_string()]);
        assert_eq!(
            resolve_for_spec(&spec, 30_000),
            NegotiationMode::Auto {
                probe_timeout_ms: 5_000
            }
        );

        // The server's own hostname, read through `flag_string_list`, wins.
        telemetry::test_set_flag_list(ORACLE_DENYLIST_FLAG, vec!["mcp.example.com".to_string()]);
        assert_eq!(resolve_for_spec(&spec, 30_000), NegotiationMode::Legacy);

        // And so does the `"*"` wildcard.
        telemetry::test_set_flag_list(ORACLE_DENYLIST_FLAG, vec!["*".to_string()]);
        assert_eq!(resolve_for_spec(&spec, 30_000), NegotiationMode::Legacy);

        telemetry::test_clear_flag_list(ORACLE_DENYLIST_FLAG);
        telemetry::test_clear_flag(FLAG_HTTP);
    }

    #[test]
    fn resolve_for_spec_http_reads_the_live_gate() {
        let _guard = flag_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(ENV_VAR);
        telemetry::test_clear_flag(FLAG_HTTP);
        let spec = http_spec("https://mcp.example.com");
        assert_eq!(
            resolve_for_spec(&spec, 30_000),
            NegotiationMode::Auto {
                probe_timeout_ms: 5_000
            }
        );
        telemetry::test_set_flag(FLAG_HTTP, false);
        assert_eq!(resolve_for_spec(&spec, 30_000), NegotiationMode::Legacy);
        telemetry::test_set_flag(FLAG_HTTP, true);
        assert_eq!(
            resolve_for_spec(&spec, 30_000),
            NegotiationMode::Auto {
                probe_timeout_ms: 5_000
            }
        );
        telemetry::test_clear_flag(FLAG_HTTP);
    }

    #[test]
    fn metadata_transport_selects_only_compatible_proxy_gates() {
        let _guard = flag_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(ENV_VAR);
        telemetry::test_set_flag(FLAG_HTTP, false);
        telemetry::test_set_flag(FLAG_CLAUDEAI, true);
        telemetry::test_set_flag(FLAG_CCR, true);
        let spec = http_spec("https://mcp.example.com");

        assert_eq!(
            resolve_for_spec_with_transport(&spec, Some("claudeai-proxy"), 30_000),
            NegotiationMode::Auto {
                probe_timeout_ms: 5_000
            }
        );
        assert_eq!(
            resolve_for_spec_with_transport(&spec, Some("ccr-proxy"), 30_000),
            NegotiationMode::Auto {
                probe_timeout_ms: 5_000
            }
        );

        // A metadata label cannot spoof an incompatible concrete transport or
        // invent an unsupported label.
        let sse = McpTransportSpec::Sse {
            url: "https://mcp.example.com".into(),
            headers: lingxi_core::host::McpHeaders::new(),
            headers_helper: None,
            oauth: None,
        };
        assert_eq!(
            resolve_for_spec_with_transport(&sse, Some("claudeai-proxy"), 30_000),
            NegotiationMode::Legacy
        );
        assert_eq!(
            resolve_for_spec_with_transport(&spec, Some("not-a-transport"), 30_000),
            NegotiationMode::Legacy
        );

        telemetry::test_clear_flag(FLAG_HTTP);
        telemetry::test_clear_flag(FLAG_CLAUDEAI);
        telemetry::test_clear_flag(FLAG_CCR);
    }
}
