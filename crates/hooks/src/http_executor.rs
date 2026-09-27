//! HTTP hook executor — POSTs the event JSON, parses the body.
//!
//! Drives `HookExecutor::Http` against an injected `Arc<dyn HttpTransport>`,
//! consulting the [`SsrfGuard`] before dispatch, applying a per-hook or
//! default timeout, and folding the response body through
//! [`crate::hook_payload::parse_response`].
//!
//! Signals back to the caller (in `executor.rs`) which arm-level telemetry
//! event the orchestrator should emit (`HOOK_HTTP_SKIPPED_SSRF` or
//! `HOOK_TIMEOUT`).
//!
//! Plan deviation: `HttpRequest` already carries an optional `timeout` field,
//! so we splice the effective timeout into the request rather than wrapping
//! the future in `tokio::time::timeout` (which would race two timeouts and
//! lose the structured `HttpError::Timeout`).

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use platform_api::{HttpError, HttpTransport};
use protocol::{HttpMethod, HttpRequest};
use regex::Regex;

use crate::definition::{HookDefinition, HookExecutor};
use crate::hook_payload::parse_response;
use crate::response::{HookOutcome, HookResponse, HookResult};
use crate::ssrf_guard::SsrfGuard;

/// Interpolate a single header VALUE, substituting `$VAR` / `${VAR}` references
/// gated on `allowed`. Byte-faithful port of claude-code `cHm`
/// (`execHttpHook.ts`, BIN off ~206545453):
///
/// - the match regex is `/\$\{([A-Z_][A-Z0-9_]*)\}|\$([A-Z_][A-Z0-9_]*)/g`
///   (UPPERCASE/underscore names only — a lowercase `$var` never matches and is
///   left verbatim);
/// - a name NOT in `allowed` resolves to `""` (claude-code also logs a
///   `Hooks: env var $NAME not in allowedEnvVars, skipping interpolation` warning);
/// - a name in `allowed` resolves to `process.env[NAME] ?? ""`;
/// - finally `\r`, `\n`, `\x00` are stripped from the whole result (`lHm`,
///   header-injection hardening).
fn interpolate_header_value(value: &str, allowed: &HashSet<&str>) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re =
        RE.get_or_init(|| Regex::new(r"\$\{([A-Z_][A-Z0-9_]*)\}|\$([A-Z_][A-Z0-9_]*)").unwrap());
    let substituted = re.replace_all(value, |caps: &regex::Captures| {
        let name = caps
            .get(1)
            .or_else(|| caps.get(2))
            .map_or("", |m| m.as_str());
        if !allowed.contains(name) {
            tracing::warn!("Hooks: env var ${name} not in allowedEnvVars, skipping interpolation");
            return String::new();
        }
        std::env::var(name).unwrap_or_default()
    });
    // `lHm`: strip CR / LF / NUL from the interpolated value.
    substituted
        .chars()
        .filter(|c| !matches!(c, '\r' | '\n' | '\u{0}'))
        .collect()
}

/// HTTP-hook security policy sourced from CC 2.1.207 settings
/// (`allowedHttpHookUrls` / `httpHookAllowedEnvVars`) at the composition root and
/// threaded into the executor. Byte-faithful port of claude-code `PFy()`
/// (`function PFy(){let e=Wn();return{allowedUrls:e.allowedHttpHookUrls,
/// allowedEnvVars:e.httpHookAllowedEnvVars}}`). CC reads these live per execution
/// (`Wn()`); lingxi sources them once at boot (like the sibling
/// `disableAllHooks` managed gate) — behavior-identical absent a mid-session
/// settings refresh, which lingxi does not wire into the hook executor.
#[derive(Debug, Clone, Default)]
pub(crate) struct HttpHookPolicy {
    /// `allowedHttpHookUrls`: `None` ⇒ all URLs allowed; `Some(empty)` ⇒ block
    /// ALL HTTP hooks; `Some(patterns)` ⇒ the hook URL must match ≥1 pattern via
    /// [`url_matches_pattern`] or the request is blocked before dispatch.
    pub(crate) allowed_urls: Option<Vec<String>>,
    /// `httpHookAllowedEnvVars`: `None` ⇒ no restriction (the per-hook
    /// `allowedEnvVars` is used as-is); `Some(list)` ⇒ each hook's effective
    /// allowlist is its own `allowedEnvVars` intersected with this global list.
    pub(crate) allowed_env_vars: Option<Vec<String>>,
}

/// Placeholder token substituted for `*` in a URL pattern before URL-parsing it,
/// so the wildcard survives `Url::parse` normalization. Mirrors claude-code's
/// per-process random `Uzt = "zzwildcard<16 hex>zz"` (`vji` module); CC
/// randomizes ONLY to avoid colliding with real URL content, so a fixed
/// lowercase token that never occurs in a real URL is functionally identical and
/// deterministic for tests. MUST stay lowercase — hostnames are lowercased
/// before the placeholder is swapped back to `*`.
const WILDCARD_PLACEHOLDER: &str = "zzwildcardplaceholder7f3a9b2c1e5d0zz";

/// Escape exactly the regex metacharacters claude-code's `NBr` escapes
/// (`/[.+?^${}()|[\]\\]/g` → `\\$&`). Notably `*` and `/` are NOT in the set:
/// `*` is substituted afterwards (to `[^/]*` or `.*`), `/` is a literal path
/// separator.
fn escape_regex_metachars_except_star(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '.' | '+' | '?' | '^' | '$' | '{' | '}' | '(' | ')' | '|' | '[' | ']' | '\\'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Strip a SINGLE trailing dot (claude-code `.replace(/\.$/,"")` — one char, not
/// all). `host..` → `host.`, not `host`.
fn strip_one_trailing_dot(s: &str) -> &str {
    s.strip_suffix('.').unwrap_or(s)
}

/// `?<query>` suffix matching JS `URL.search` (leading `?`, empty when absent).
fn query_suffix(u: &url::Url) -> String {
    u.query().map(|q| format!("?{q}")).unwrap_or_default()
}

/// Reconstruct `${protocol}//${host}${pathname}${search}` matching JS `URL`'s
/// serialization: `protocol` has a trailing colon, `host` includes a non-default
/// port, `search` a leading `?`.
fn reconstruct_url_string(u: &url::Url) -> String {
    let host = match u.port() {
        Some(p) => format!("{}:{}", u.host_str().unwrap_or(""), p),
        None => u.host_str().unwrap_or("").to_string(),
    };
    format!("{}://{}{}{}", u.scheme(), host, u.path(), query_suffix(u))
}

/// Replace the FIRST `:<placeholder>` occurrence immediately followed by `/`,
/// `?`, `#`, or end-of-string with `:0` (claude-code's NON-global
/// `n.replace(/:${Uzt}(?=[/?#]|$)/,":0")` — no `g` flag ⇒ first match only).
/// Returns `None` when there is no such occurrence (string unchanged), matching
/// CC's `if(u!==n)` guard. The Rust `regex` crate has no lookahead, so the
/// boundary is checked manually.
fn replace_first_port_wildcard(n: &str) -> Option<String> {
    let needle = format!(":{WILDCARD_PLACEHOLDER}");
    let bytes = n.as_bytes();
    for (idx, _) in n.match_indices(&needle) {
        let after = idx + needle.len();
        let boundary = after >= n.len() || matches!(bytes[after], b'/' | b'?' | b'#');
        if boundary {
            let mut out = String::with_capacity(n.len());
            out.push_str(&n[..idx]);
            out.push_str(":0");
            out.push_str(&n[after..]);
            return Some(out);
        }
    }
    None
}

/// Does `url` match the wildcard `pattern`? Byte-faithful port of claude-code
/// `NBr(url, pattern)` (BIN off ~108914079, module `vji`).
///
/// - `"*"` matches everything.
/// - `*` is a wildcard for a segment of scheme / host / port / path+query. Host
///   wildcards expand to `[^/]*` (never crossing `/`), path+query wildcards to
///   `.*` (crossing `/`).
/// - A pattern that is not URL-parseable (even after a `:*` port retry) falls
///   back to a whole-URL regex over `${protocol}//${host}${pathname}${search}`.
/// - A target `url` that does not parse never matches.
pub(crate) fn url_matches_pattern(url: &str, pattern: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let Ok(target) = url::Url::parse(url) else {
        return false;
    };

    // Substitute `*` → placeholder so the wildcard survives URL parsing.
    let n = pattern.replace('*', WILDCARD_PLACEHOLDER);
    let mut port_wildcarded = false;
    let mut pat = url::Url::parse(&n).ok();
    if pat.is_none() {
        // A `*` in port position yields a non-numeric port that fails to parse;
        // retry with `:0` and remember the port was a wildcard.
        if let Some(fixed) = replace_first_port_wildcard(&n) {
            if let Ok(p) = url::Url::parse(&fixed) {
                pat = Some(p);
                port_wildcarded = true;
            }
        }
    }

    let Some(pat) = pat else {
        // Pattern not URL-parseable at all — whole-URL regex fallback (built from
        // the ORIGINAL pattern `t`, `*` → `[^/]*`) tested against the
        // reconstructed target URL string.
        let reconstructed = reconstruct_url_string(&target);
        let re_src = format!(
            "^{}$",
            escape_regex_metachars_except_star(pattern).replace('*', "[^/]*")
        );
        return Regex::new(&re_src)
            .map(|re| re.is_match(&reconstructed))
            .unwrap_or(false);
    };

    // Protocol: skip when the pattern scheme itself was a wildcard (placeholder).
    if pat.scheme() != WILDCARD_PLACEHOLDER && pat.scheme() != target.scheme() {
        return false;
    }

    // Hostname: strip a single trailing dot + lowercase both sides; the pattern
    // host becomes a full-match regex with placeholder → `*` → `[^/]*`.
    let target_host = strip_one_trailing_dot(target.host_str().unwrap_or("")).to_lowercase();
    let pat_host = strip_one_trailing_dot(pat.host_str().unwrap_or("")).to_lowercase();
    let host_re_src = format!(
        "^{}$",
        escape_regex_metachars_except_star(&pat_host.replace(WILDCARD_PLACEHOLDER, "*"))
            .replace('*', "[^/]*")
    );
    match Regex::new(&host_re_src) {
        Ok(re) if re.is_match(&target_host) => {}
        _ => return false,
    }

    // Port: a wildcard host with no explicit port also wildcards the port.
    if pat.port().is_none() && pat.host_str().unwrap_or("").contains(WILDCARD_PLACEHOLDER) {
        port_wildcarded = true;
    }
    if !port_wildcarded && pat.port() != target.port() {
        return false;
    }

    // Path: a pattern with no meaningful path (root/empty, no query, and whose
    // substituted form does not end in `/`) matches ANY path.
    let pat_path = pat.path();
    if (pat_path == "/" || pat_path.is_empty()) && pat.query().is_none() && !n.ends_with('/') {
        return true;
    }

    // Otherwise a full-match regex over path+query (wildcards cross `/` → `.*`).
    let pat_path_search = format!("{}{}", pat.path(), query_suffix(&pat));
    let target_path_search = format!("{}{}", target.path(), query_suffix(&target));
    let path_re_src = format!(
        "^{}$",
        escape_regex_metachars_except_star(&pat_path_search.replace(WILDCARD_PLACEHOLDER, "*"))
            .replace('*', ".*")
    );
    Regex::new(&path_re_src)
        .map(|re| re.is_match(&target_path_search))
        .unwrap_or(false)
}

/// Telemetry hint the caller (`HookExecutorImpl::execute_single`) emits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HttpExecutionSignal {
    /// Request completed (success or non-SSRF non-timeout error).
    Ok,
    /// SSRF guard rejected the URL before dispatch.
    SsrfBlocked(String),
    /// `allowedHttpHookUrls` rejected the URL before dispatch. CC emits NO
    /// telemetry for this (only a warn log), so the caller does too.
    UrlBlocked,
    /// Request exceeded the effective timeout.
    TimedOut,
}

/// Wrapper struct returned by [`HttpExecutor::execute`].
pub(crate) struct HttpExecutionOutcome {
    /// The raw `HookResult` to fold into `AggregateHookResult`.
    pub(crate) result: HookResult,
    /// Hint to the caller for arm-level telemetry emission.
    pub(crate) signal: HttpExecutionSignal,
}

pub(crate) struct HttpExecutor {
    pub(crate) http: Arc<dyn HttpTransport>,
    pub(crate) ssrf_guard: SsrfGuard,
    pub(crate) timeout: Duration,
    /// CC 2.1.207 HTTP-hook security policy (`allowedHttpHookUrls` /
    /// `httpHookAllowedEnvVars`). Default (both `None`) = no restriction, so this
    /// is behavior-neutral until a settings policy is wired at the composition
    /// root.
    pub(crate) policy: HttpHookPolicy,
}

impl HttpExecutor {
    /// Build a new executor with the supplied transport + SSRF policy. The
    /// HTTP-hook security policy defaults to "no restriction" (both settings
    /// unset); set [`HttpExecutor::policy`] to enforce `allowedHttpHookUrls` /
    /// `httpHookAllowedEnvVars`.
    #[allow(dead_code)]
    pub(crate) fn new(
        http: Arc<dyn HttpTransport>,
        ssrf_guard: SsrfGuard,
        timeout: Duration,
    ) -> Self {
        Self {
            http,
            ssrf_guard,
            timeout,
            policy: HttpHookPolicy::default(),
        }
    }

    /// Execute one HTTP hook.
    ///
    /// `body` is the pre-serialized envelope JSON. `expected_event` is
    /// `"PreToolUse"` or `"PostToolUse"` and validates the nested
    /// `hookSpecificOutput.hookEventName` field per `claude-code`.
    pub(crate) async fn execute(
        &self,
        hook: &HookDefinition,
        url: &str,
        headers: &HashMap<String, String>,
        body: &str,
        expected_event: &'static str,
    ) -> HttpExecutionOutcome {
        // 0. `allowedHttpHookUrls` gate (H-BIN-12; claude-code `nPs`). When the
        //    policy defines an allowlist, the hook URL must match at least one
        //    wildcard pattern (`NBr`) or the request is blocked before ANY
        //    network work — CC checks this FIRST, ahead of everything else. `None`
        //    ⇒ all URLs allowed; `Some(empty)` ⇒ block ALL HTTP hooks (no pattern
        //    can match). The warn line is byte-exact CC (`C(u,{level:"warn"})`);
        //    the `stderr` embeds it under lingxi's `Hook {id} failed:` convention
        //    (as the SSRF / http-error / timeout arms already do).
        if let Some(patterns) = &self.policy.allowed_urls {
            if !patterns.iter().any(|p| url_matches_pattern(url, p)) {
                let msg = format!(
                    "HTTP hook blocked: {url} does not match any pattern in allowedHttpHookUrls"
                );
                tracing::warn!("{msg}");
                return HttpExecutionOutcome {
                    result: HookResult {
                        outcome: HookOutcome::Error,
                        stdout: String::new(),
                        stderr: format!("Hook {} failed: {msg}", hook.id),
                        exit_code: None,
                        response: None,
                    },
                    signal: HttpExecutionSignal::UrlBlocked,
                };
            }
        }

        // 1. SSRF check.
        let resolved_addrs = match self.ssrf_guard.resolve_url(url).await {
            Ok(resolved) => resolved,
            Err(e) => {
                return HttpExecutionOutcome {
                    result: HookResult {
                        outcome: HookOutcome::Error,
                        stdout: String::new(),
                        stderr: format!("Hook {} failed: SSRF guard rejected url: {}", hook.id, e),
                        exit_code: None,
                        response: None,
                    },
                    signal: HttpExecutionSignal::SsrfBlocked(e.to_string()),
                };
            }
        };

        // 2. Pick effective timeout (per-hook override or default).
        let effective_timeout = match &hook.executor {
            HookExecutor::Http { timeout, .. } if !timeout.is_zero() => *timeout,
            _ => self.timeout,
        };

        // 3. Build the request. Interpolate `$VAR`/`${VAR}` in header VALUES
        //    gated on the hook's `allowedEnvVars` (claude-code `cHm`), then inject
        //    Content-Type: application/json if the caller didn't supply one.
        //
        //    H-BIN-12 env intersection (claude-code `nPs`): the EFFECTIVE
        //    allowlist is the per-hook `allowedEnvVars` intersected with the
        //    global `httpHookAllowedEnvVars` when that setting is present
        //    (`m=e.allowedEnvVars??[], g=i.allowedEnvVars!==void 0 ?
        //    m.filter(_=>i.allowedEnvVars.includes(_)) : m`). Absent global ⇒
        //    per-hook list unchanged.
        let allowed: HashSet<&str> = match &hook.executor {
            HookExecutor::Http {
                allowed_env_vars, ..
            } => match &self.policy.allowed_env_vars {
                Some(global) => allowed_env_vars
                    .iter()
                    .filter(|v| global.iter().any(|g| g == *v))
                    .map(String::as_str)
                    .collect(),
                None => allowed_env_vars.iter().map(String::as_str).collect(),
            },
            _ => HashSet::new(),
        };
        let mut req_headers: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.clone(), interpolate_header_value(v, &allowed)))
            .collect();
        let has_content_type = req_headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("content-type"));
        if !has_content_type {
            req_headers.push(("Content-Type".into(), "application/json".into()));
        }
        let req = HttpRequest {
            method: HttpMethod::Post,
            url: url.to_string(),
            headers: req_headers,
            body: Some(body.to_string()),
            body_bytes: None,
            timeout: Some(effective_timeout),
        };

        // 4. Issue the request. `HttpTransport` enforces the request-level
        //    timeout natively and surfaces `HttpError::Timeout` on elapse.
        let raw = match self
            .http
            .request_with_resolved_addrs(req, resolved_addrs)
            .await
        {
            Ok(r) => r,
            Err(HttpError::Timeout(_)) => {
                return HttpExecutionOutcome {
                    result: HookResult {
                        outcome: HookOutcome::Timeout,
                        stdout: String::new(),
                        stderr: format!(
                            "Hook {} failed: timeout after {}ms",
                            hook.id,
                            effective_timeout.as_millis()
                        ),
                        exit_code: None,
                        response: None,
                    },
                    signal: HttpExecutionSignal::TimedOut,
                };
            }
            Err(e) => {
                return HttpExecutionOutcome {
                    result: HookResult {
                        outcome: HookOutcome::Error,
                        stdout: String::new(),
                        stderr: format!("Hook {} failed: http error: {e}", hook.id),
                        exit_code: None,
                        response: None,
                    },
                    signal: HttpExecutionSignal::Ok,
                };
            }
        };

        // 5. Parse body if any. Even on non-2xx status, attempt to parse
        //    because some hooks return JSON + non-2xx to mean "advisory".
        let success = (200..300).contains(&raw.status);
        let parsed: Option<HookResponse> = if raw.body.is_empty() {
            None
        } else {
            parse_response(&raw.body, expected_event).ok()
        };

        HttpExecutionOutcome {
            result: HookResult {
                outcome: if success {
                    HookOutcome::Success
                } else {
                    HookOutcome::Error
                },
                stdout: raw.body,
                stderr: String::new(),
                exit_code: None,
                response: parsed,
            },
            signal: HttpExecutionSignal::Ok,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::{HookExecutor as DefHookExecutor, HookSource};
    use crate::events::HookEventType;
    use async_trait::async_trait;
    use platform_api::ResolvedAddressOverride;
    use protocol::{HookId, HttpResponse};
    use std::sync::Mutex;

    struct MockHttp {
        recorded: Mutex<Vec<HttpRequest>>,
        resolved: Mutex<Vec<Option<ResolvedAddressOverride>>>,
        response_body: String,
        response_status: u16,
        error_to_return: Mutex<Option<HttpError>>,
    }

    impl MockHttp {
        fn dispatch(
            &self,
            req: HttpRequest,
            resolved: Option<ResolvedAddressOverride>,
        ) -> Result<HttpResponse, HttpError> {
            if let Some(e) = self.error_to_return.lock().unwrap().take() {
                return Err(e);
            }
            self.recorded.lock().unwrap().push(req);
            self.resolved.lock().unwrap().push(resolved);
            Ok(HttpResponse {
                status: self.response_status,
                headers: Vec::new(),
                body: self.response_body.clone(),
                body_bytes: Vec::new(),
            })
        }
    }

    #[async_trait]
    impl HttpTransport for MockHttp {
        async fn request(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
            self.dispatch(req, None)
        }

        async fn request_with_resolved_addrs(
            &self,
            req: HttpRequest,
            resolved: Option<ResolvedAddressOverride>,
        ) -> Result<HttpResponse, HttpError> {
            self.dispatch(req, resolved)
        }
        async fn stream_sse(
            &self,
            _req: HttpRequest,
        ) -> Result<platform_api::http::SseStream, HttpError> {
            Err(HttpError::InvalidRequest("not implemented".into()))
        }
    }

    fn make_http_hook(url: &str) -> HookDefinition {
        HookDefinition {
            id: HookId::new(),
            name: "test-http".into(),
            events: vec![HookEventType::PreToolUse],
            if_condition: None,
            executor: DefHookExecutor::Http {
                url: url.into(),
                method: "POST".into(),
                headers: HashMap::new(),
                allowed_env_vars: Vec::new(),
                timeout: Duration::from_secs(5),
            },
            source: HookSource::Settings(protocol::SettingsScope::User),
            blocking: true,
            timeout: None,
            priority: 0,
            once: false,
            status_message: None,
            async_rewake: false,
            async_timeout: None,
            rewake_message: None,
        }
    }

    #[derive(Default)]
    struct StaticResolver(
        std::collections::HashMap<(String, u16), Result<Vec<std::net::SocketAddr>, String>>,
    );

    impl StaticResolver {
        fn public_example_hosts() -> Self {
            let mut answers = std::collections::HashMap::new();
            for host in [
                "hook.example.com",
                "hooks.example.com",
                "evil.example.com",
                "api.hooks.example.com",
            ] {
                answers.insert(
                    (host.to_string(), 443),
                    Ok(vec!["93.184.216.34:443".parse().unwrap()]),
                );
            }
            Self(answers)
        }
    }

    #[async_trait]
    impl crate::ssrf_guard::DnsResolver for StaticResolver {
        async fn lookup_host(
            &self,
            host: &str,
            port: u16,
        ) -> Result<Vec<std::net::SocketAddr>, String> {
            self.0
                .get(&(host.to_string(), port))
                .cloned()
                .unwrap_or_else(|| Err(format!("missing resolver answer for {host}:{port}")))
        }
    }

    fn test_ssrf_guard() -> SsrfGuard {
        SsrfGuard::with_test_resolver(StaticResolver::public_example_hosts())
    }

    #[test]
    fn interpolate_header_value_honors_allowlist_and_strips_controls() {
        // `set_var` forbids NUL in the value, so test CR/LF stripping via env and
        // NUL stripping via a literal value (below).
        std::env::set_var("LX_HOOK_TEST_TOKEN", "secret\nval\rue");
        let allowed: HashSet<&str> = ["LX_HOOK_TEST_TOKEN"].into_iter().collect();

        // `${VAR}` and `$VAR` both interpolate when allowed; CR/LF stripped.
        assert_eq!(
            interpolate_header_value("Bearer ${LX_HOOK_TEST_TOKEN}", &allowed),
            "Bearer secretvalue"
        );
        assert_eq!(
            interpolate_header_value("$LX_HOOK_TEST_TOKEN", &allowed),
            "secretvalue"
        );
        // `lHm` strips CR/LF/NUL from the final value even with no interpolation.
        assert_eq!(interpolate_header_value("a\u{0}b\r\nc", &allowed), "abc");
        // A name NOT in the allowlist resolves to empty string.
        assert_eq!(interpolate_header_value("a${OTHER_VAR}b", &allowed), "ab");
        // Lowercase `$var` does not match the [A-Z_] grammar — left verbatim.
        assert_eq!(
            interpolate_header_value("x$lowercase y", &allowed),
            "x$lowercase y"
        );
        // Empty allowlist blanks every reference.
        let empty: HashSet<&str> = HashSet::new();
        assert_eq!(
            interpolate_header_value("Bearer $LX_HOOK_TEST_TOKEN", &empty),
            "Bearer "
        );
        std::env::remove_var("LX_HOOK_TEST_TOKEN");
    }

    #[tokio::test]
    async fn returns_approve_when_endpoint_responds_with_allow() {
        let http = Arc::new(MockHttp {
            recorded: Mutex::new(Vec::new()),
            response_status: 200,
            response_body:
                r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow"}}"#
                    .into(),
            error_to_return: Mutex::new(None),
            resolved: Mutex::new(Vec::new()),
        });
        let exec = HttpExecutor::new(http.clone(), test_ssrf_guard(), Duration::from_secs(5));
        let hook = make_http_hook("https://hook.example.com/pre");

        let outcome = exec
            .execute(
                &hook,
                "https://hook.example.com/pre",
                &HashMap::new(),
                "{}",
                "PreToolUse",
            )
            .await;

        assert_eq!(outcome.signal, HttpExecutionSignal::Ok);
        assert!(matches!(outcome.result.outcome, HookOutcome::Success));
        let resp = outcome.result.response.expect("response parsed");
        assert_eq!(resp.decision, Some(crate::response::HookDecision::Approve));
        assert_eq!(http.recorded.lock().unwrap().len(), 1);
        let resolved = http.resolved.lock().unwrap();
        let pinned = resolved
            .first()
            .cloned()
            .flatten()
            .expect("domain request should pin vetted addresses");
        assert_eq!(pinned.domain, "hook.example.com");
        assert_eq!(pinned.addrs, vec!["93.184.216.34:443".parse().unwrap()]);
    }

    #[tokio::test]
    async fn ssrf_blocks_link_local() {
        let http = Arc::new(MockHttp {
            recorded: Mutex::new(Vec::new()),
            response_status: 200,
            response_body: String::new(),
            error_to_return: Mutex::new(None),
            resolved: Mutex::new(Vec::new()),
        });
        let exec = HttpExecutor::new(
            http.clone(),
            SsrfGuard::with_defaults(),
            Duration::from_secs(5),
        );
        let hook = make_http_hook("http://169.254.169.254/meta");

        let outcome = exec
            .execute(
                &hook,
                "http://169.254.169.254/meta",
                &HashMap::new(),
                "{}",
                "PreToolUse",
            )
            .await;

        assert!(matches!(
            outcome.signal,
            HttpExecutionSignal::SsrfBlocked(_)
        ));
        assert!(matches!(outcome.result.outcome, HookOutcome::Error));
        assert_eq!(
            http.recorded.lock().unwrap().len(),
            0,
            "no request should be sent"
        );
    }

    #[tokio::test]
    async fn timeout_surfaces_as_timed_out() {
        let http = Arc::new(MockHttp {
            recorded: Mutex::new(Vec::new()),
            response_status: 200,
            response_body: String::new(),
            error_to_return: Mutex::new(Some(HttpError::Timeout(Duration::from_millis(1)))),
            resolved: Mutex::new(Vec::new()),
        });
        let exec = HttpExecutor::new(http.clone(), test_ssrf_guard(), Duration::from_millis(1));
        let hook = make_http_hook("https://hook.example.com/pre");

        let outcome = exec
            .execute(
                &hook,
                "https://hook.example.com/pre",
                &HashMap::new(),
                "{}",
                "PreToolUse",
            )
            .await;

        assert_eq!(outcome.signal, HttpExecutionSignal::TimedOut);
        assert!(matches!(outcome.result.outcome, HookOutcome::Timeout));
    }

    // ── H-BIN-12: `NBr` URL wildcard matcher ─────────────────────────────────

    #[test]
    fn nbr_bare_star_matches_everything() {
        assert!(url_matches_pattern("https://anything.example/x?y=1", "*"));
        assert!(url_matches_pattern("http://10.0.0.1:9000/", "*"));
    }

    #[test]
    fn nbr_unparseable_target_never_matches() {
        // A target URL that does not parse returns false for any non-`*` pattern.
        assert!(!url_matches_pattern(
            "not a url",
            "https://hooks.example.com/*"
        ));
        // ...but bare `*` short-circuits before parsing.
        assert!(url_matches_pattern("not a url", "*"));
    }

    #[test]
    fn nbr_scheme_wildcard() {
        assert!(url_matches_pattern(
            "https://hooks.example.com/webhook",
            "*://hooks.example.com/webhook"
        ));
        assert!(url_matches_pattern(
            "http://hooks.example.com/webhook",
            "*://hooks.example.com/webhook"
        ));
        // Non-wildcard scheme must match exactly.
        assert!(!url_matches_pattern(
            "http://hooks.example.com/webhook",
            "https://hooks.example.com/webhook"
        ));
    }

    #[test]
    fn nbr_host_wildcard_does_not_cross_slash() {
        // `*` in the host expands to `[^/]*` — matches subdomains but not paths.
        assert!(url_matches_pattern(
            "https://api.hooks.example.com/x",
            "https://*.example.com/*"
        ));
        assert!(url_matches_pattern(
            "https://a.example.com/deep/path",
            "https://*.example.com/*"
        ));
        // Different apex domain does not match.
        assert!(!url_matches_pattern(
            "https://api.evil.com/x",
            "https://*.example.com/*"
        ));
    }

    #[test]
    fn nbr_path_wildcard_crosses_slash() {
        let p = "https://hooks.example.com/*";
        // `/*` in the path expands to `.*` (crosses `/`, spans query).
        assert!(url_matches_pattern("https://hooks.example.com/webhook", p));
        assert!(url_matches_pattern(
            "https://hooks.example.com/deep/path?q=1",
            p
        ));
        assert!(url_matches_pattern("https://hooks.example.com/", p));
        // Host / scheme still bind exactly.
        assert!(!url_matches_pattern("https://evil.example.com/x", p));
        assert!(!url_matches_pattern("http://hooks.example.com/x", p));
    }

    #[test]
    fn nbr_no_path_matches_any_path_unless_trailing_slash() {
        // Pattern with no path component matches ANY path.
        assert!(url_matches_pattern(
            "https://hooks.example.com/anything/here",
            "https://hooks.example.com"
        ));
        // A trailing slash pins the path to exactly `/`.
        assert!(url_matches_pattern(
            "https://hooks.example.com/",
            "https://hooks.example.com/"
        ));
        assert!(!url_matches_pattern(
            "https://hooks.example.com/deep",
            "https://hooks.example.com/"
        ));
    }

    #[test]
    fn nbr_port_wildcard() {
        let p = "https://hooks.example.com:*/webhook";
        assert!(url_matches_pattern(
            "https://hooks.example.com:8443/webhook",
            p
        ));
        assert!(url_matches_pattern(
            "https://hooks.example.com:1234/webhook",
            p
        ));
        // Wildcard port also matches the default (absent) port.
        assert!(url_matches_pattern("https://hooks.example.com/webhook", p));
        // An explicit non-wildcard port must match exactly.
        assert!(url_matches_pattern(
            "https://hooks.example.com:8443/webhook",
            "https://hooks.example.com:8443/webhook"
        ));
        assert!(!url_matches_pattern(
            "https://hooks.example.com:9999/webhook",
            "https://hooks.example.com:8443/webhook"
        ));
    }

    #[test]
    fn nbr_non_url_pattern_regex_fallback() {
        // A pattern that is not URL-parseable (no `:` scheme separator) falls
        // back to a whole-URL regex over `${protocol}//${host}${path}${search}`.
        assert!(url_matches_pattern(
            "https://hooks.example.com/webhook",
            "*//hooks.example.com/*"
        ));
        // A scheme-less host-only pattern cannot match a full `https://…` URL in
        // the fallback (the `[^/]*` before `.example.com` cannot cross `//`).
        assert!(!url_matches_pattern(
            "https://foo.example.com/",
            "*.example.com"
        ));
    }

    #[test]
    fn nbr_trailing_dot_and_case_insensitive_host() {
        // Hostnames are compared case-insensitively with a single trailing dot
        // stripped from both sides.
        assert!(url_matches_pattern(
            "https://Hooks.Example.COM./x",
            "https://hooks.example.com/*"
        ));
    }

    // ── H-BIN-12: `allowedHttpHookUrls` gate + env intersection (execute) ─────

    fn mock_http() -> Arc<MockHttp> {
        Arc::new(MockHttp {
            recorded: Mutex::new(Vec::new()),
            resolved: Mutex::new(Vec::new()),
            response_status: 200,
            response_body: String::new(),
            error_to_return: Mutex::new(None),
        })
    }

    fn exec_with_policy(http: Arc<MockHttp>, policy: HttpHookPolicy) -> HttpExecutor {
        HttpExecutor {
            http,
            ssrf_guard: test_ssrf_guard(),
            timeout: Duration::from_secs(5),
            policy,
        }
    }

    #[tokio::test]
    async fn url_gate_blocks_non_matching_url_before_dispatch() {
        let http = mock_http();
        let exec = exec_with_policy(
            http.clone(),
            HttpHookPolicy {
                allowed_urls: Some(vec!["https://hooks.example.com/*".into()]),
                allowed_env_vars: None,
            },
        );
        let hook = make_http_hook("https://evil.example.com/exfil");
        let outcome = exec
            .execute(
                &hook,
                "https://evil.example.com/exfil",
                &HashMap::new(),
                "{}",
                "PreToolUse",
            )
            .await;

        assert_eq!(outcome.signal, HttpExecutionSignal::UrlBlocked);
        assert!(matches!(outcome.result.outcome, HookOutcome::Error));
        assert!(
            outcome.result.stdout.is_empty(),
            "blocked body must be empty"
        );
        // Byte-exact CC block message embedded in stderr.
        assert!(
            outcome.result.stderr.contains(
                "HTTP hook blocked: https://evil.example.com/exfil does not match any pattern in allowedHttpHookUrls"
            ),
            "stderr = {:?}",
            outcome.result.stderr
        );
        assert_eq!(
            http.recorded.lock().unwrap().len(),
            0,
            "no request should be sent when the URL is blocked"
        );
    }

    #[tokio::test]
    async fn url_gate_allows_matching_url() {
        let http = mock_http();
        let exec = exec_with_policy(
            http.clone(),
            HttpHookPolicy {
                allowed_urls: Some(vec!["https://hooks.example.com/*".into()]),
                allowed_env_vars: None,
            },
        );
        let hook = make_http_hook("https://hooks.example.com/webhook");
        let outcome = exec
            .execute(
                &hook,
                "https://hooks.example.com/webhook",
                &HashMap::new(),
                "{}",
                "PreToolUse",
            )
            .await;

        assert_ne!(outcome.signal, HttpExecutionSignal::UrlBlocked);
        assert_eq!(http.recorded.lock().unwrap().len(), 1, "request dispatched");
    }

    #[tokio::test]
    async fn url_gate_empty_allowlist_blocks_all() {
        // `Some(empty)` ⇒ no pattern can match ⇒ every HTTP hook is blocked.
        let http = mock_http();
        let exec = exec_with_policy(
            http.clone(),
            HttpHookPolicy {
                allowed_urls: Some(Vec::new()),
                allowed_env_vars: None,
            },
        );
        let hook = make_http_hook("https://hooks.example.com/webhook");
        let outcome = exec
            .execute(
                &hook,
                "https://hooks.example.com/webhook",
                &HashMap::new(),
                "{}",
                "PreToolUse",
            )
            .await;

        assert_eq!(outcome.signal, HttpExecutionSignal::UrlBlocked);
        assert_eq!(http.recorded.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn url_gate_none_allows_all() {
        // Default policy (`None`) imposes no restriction — dispatch proceeds.
        let http = mock_http();
        let exec = exec_with_policy(http.clone(), HttpHookPolicy::default());
        let hook = make_http_hook("https://hooks.example.com/webhook");
        let outcome = exec
            .execute(
                &hook,
                "https://hooks.example.com/webhook",
                &HashMap::new(),
                "{}",
                "PreToolUse",
            )
            .await;

        assert_ne!(outcome.signal, HttpExecutionSignal::UrlBlocked);
        assert_eq!(http.recorded.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn global_env_allowlist_intersects_per_hook_list() {
        // Per-hook allowedEnvVars = [A, B]; global httpHookAllowedEnvVars = [A].
        // Effective allowlist = {A}, so `$LX_HBIN12_A` interpolates and
        // `$LX_HBIN12_B` blanks. With global None, both interpolate.
        std::env::set_var("LX_HBIN12_A", "valA");
        std::env::set_var("LX_HBIN12_B", "valB");

        let hook = HookDefinition {
            id: HookId::new(),
            name: "test-http".into(),
            events: vec![HookEventType::PreToolUse],
            if_condition: None,
            executor: DefHookExecutor::Http {
                url: "https://hooks.example.com/webhook".into(),
                method: "POST".into(),
                headers: HashMap::new(),
                allowed_env_vars: vec!["LX_HBIN12_A".into(), "LX_HBIN12_B".into()],
                timeout: Duration::from_secs(5),
            },
            source: HookSource::Settings(protocol::SettingsScope::User),
            blocking: true,
            timeout: None,
            priority: 0,
            once: false,
            status_message: None,
            async_rewake: false,
            async_timeout: None,
            rewake_message: None,
        };
        let mut headers = HashMap::new();
        headers.insert(
            "X-Auth".to_string(),
            "$LX_HBIN12_A-$LX_HBIN12_B".to_string(),
        );

        // Global Some([A]) — B is intersected OUT and blanks.
        let http = mock_http();
        let exec = exec_with_policy(
            http.clone(),
            HttpHookPolicy {
                allowed_urls: None,
                allowed_env_vars: Some(vec!["LX_HBIN12_A".into()]),
            },
        );
        exec.execute(
            &hook,
            "https://hooks.example.com/webhook",
            &headers,
            "{}",
            "PreToolUse",
        )
        .await;
        let recorded = http.recorded.lock().unwrap();
        let req = recorded.first().expect("request dispatched");
        let auth = req
            .headers
            .iter()
            .find(|(k, _)| k == "X-Auth")
            .map(|(_, v)| v.as_str())
            .unwrap();
        assert_eq!(auth, "valA-", "global allowlist intersects B out");
        drop(recorded);

        // Global None — no restriction, both per-hook vars interpolate.
        let http2 = mock_http();
        let exec2 = exec_with_policy(http2.clone(), HttpHookPolicy::default());
        exec2
            .execute(
                &hook,
                "https://hooks.example.com/webhook",
                &headers,
                "{}",
                "PreToolUse",
            )
            .await;
        let recorded2 = http2.recorded.lock().unwrap();
        let auth2 = recorded2
            .first()
            .unwrap()
            .headers
            .iter()
            .find(|(k, _)| k == "X-Auth")
            .map(|(_, v)| v.as_str())
            .unwrap();
        assert_eq!(
            auth2, "valA-valB",
            "no global restriction ⇒ both interpolate"
        );
        drop(recorded2);

        std::env::remove_var("LX_HBIN12_A");
        std::env::remove_var("LX_HBIN12_B");
    }
}
