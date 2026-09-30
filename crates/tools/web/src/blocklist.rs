//! Domain-blocklist preflight for `WebFetchTool`.
//!
//! Mirrors `claude-code/src/tools/WebFetchTool/utils.ts:21-35,71-78,171-203,420-435`.
//!
//! Before the main fetch, claude-code GETs
//! `https://api.anthropic.com/api/web/domain_info?domain=<host>` and switches on
//! the result:
//! - `200` + `{ "can_fetch": true }`  → **allowed** (and the host is cached for 5 min)
//! - `200` + `{ "can_fetch": false }` → **blocked** (`DomainBlockedError`)
//! - non-200 (but no throw)           → **check_failed** (`DomainCheckFailedError`)
//! - transport error / timeout        → **check_failed** (`DomainCheckFailedError`)
//!
//! `check_failed` is **fail-open** at the call site only in the sense that the TS
//! maps it to a user-facing `DomainCheckFailedError` and re-throws it; the result
//! variant itself carries the failure. This module reproduces the three-state
//! `DomainCheckResult` faithfully; `web_fetch.rs` maps `Blocked`/`CheckFailed` to
//! the byte-locked error messages.
//!
//! Wire-locked constants (byte-checked against `utils.ts`):
//! - `DOMAIN_CHECK_TIMEOUT_MS = 10_000` (`utils.ts:119`)
//! - `DOMAIN_CHECK_CACHE`: `max: 128`, `ttl: 5 * 60 * 1000` ms (`utils.ts:75-78`)
//! - `DomainBlockedError` message (`utils.ts:23`)
//! - `DomainCheckFailedError` message (`utils.ts:30-32`)
//!
//! Divergences (flagged):
//! - **Base URL.** TS hard-codes the literal `https://api.anthropic.com`. The
//!   shared `AnthropicProvider` does not expose its `base_url`, so we cannot reuse
//!   an enterprise-proxy base here without touching `api-client` (out of scope);
//!   we hard-code the TS literal. Follow-up: add a public base-URL accessor to
//!   route enterprise egress proxies.
//! - **`encodeURIComponent`.** Hand-rolled (no new dependency) — encodes every
//!   byte outside JS's `encodeURIComponent` unreserved set
//!   (`A-Za-z0-9 - _ . ! ~ * ' ( )`). Byte-identical to `encodeURIComponent` for
//!   hostnames (which are ASCII alnum + `.`/`-`, all unreserved) and for the
//!   already-Punycode-encoded form of internationalized domains.
//! - The `process.env.USER_TYPE === 'ant'` `tengu_web_fetch_host` analytics event
//!   (`utils.ts:437-442`) is out of scope per the batch spec and is not emitted
//!   (no equivalent telemetry event exists today).

use lingxi_core::host::http::HttpTransport;
use lingxi_core::types::{HttpMethod, HttpRequest};
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Timeout for the domain blocklist preflight check (10 seconds).
/// Mirrors `utils.ts:119` `DOMAIN_CHECK_TIMEOUT_MS`.
pub const DOMAIN_CHECK_TIMEOUT: Duration = Duration::from_millis(10_000);

/// TTL for an `Allowed` domain-check cache entry: 5 minutes — shorter than the
/// URL content cache (`utils.ts:77`).
pub const DOMAIN_CHECK_CACHE_TTL: Duration = Duration::from_secs(5 * 60);

/// Max distinct hosts cached. Mirrors `utils.ts:76` `max: 128`.
pub const DOMAIN_CHECK_CACHE_MAX: usize = 128;

/// Hard-coded preflight base URL. Matches the TS literal at `utils.ts:184`.
/// (See module docs for the enterprise-proxy follow-up.)
pub const DOMAIN_CHECK_BASE_URL: &str = "https://api.anthropic.com";

/// Outcome of a domain blocklist preflight check.
///
/// Faithful port of the TS `DomainCheckResult` union (`utils.ts:171-174`):
/// `{ status: 'allowed' } | { status: 'blocked' } | { status: 'check_failed'; error }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomainCheckResult {
    /// `200` + `{ can_fetch: true }`. The fetch may proceed.
    Allowed,
    /// `200` + `{ can_fetch: false }`. The fetch must be refused.
    Blocked,
    /// Non-200, or a transport/timeout error. Carries the failure description
    /// (the TS variant carries an `Error`; we carry its message string).
    CheckFailed(String),
}

/// Byte-locked `DomainBlockedError` message (`utils.ts:23`).
#[must_use]
pub fn domain_blocked_msg(domain: &str) -> String {
    format!("LingXi is unable to fetch from {domain}")
}

/// Byte-locked `DomainCheckFailedError` message (`utils.ts:30-32`).
#[must_use]
pub fn domain_check_failed_msg(domain: &str) -> String {
    format!(
        "Unable to verify if domain {domain} is safe to fetch. This may be due to \
         network restrictions or enterprise security policies blocking claude.ai."
    )
}

/// Percent-encode `s` exactly as JavaScript's `encodeURIComponent` does.
///
/// `encodeURIComponent` passes through the unreserved set
/// `A-Z a-z 0-9 - _ . ! ~ * ' ( )` and percent-encodes every other byte (the
/// UTF-8 bytes of any non-ASCII char). Hostnames consist of ASCII alphanumerics,
/// `.`, and `-`, which are all unreserved, so this is a no-op for the common case
/// and byte-identical to `encodeURIComponent` for the edge cases.
#[must_use]
fn encode_uri_component(s: &str) -> String {
    fn is_unreserved(b: u8) -> bool {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
    }
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if is_unreserved(b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(
                char::from_digit(u32::from(b >> 4), 16)
                    .unwrap()
                    .to_ascii_uppercase(),
            );
            out.push(
                char::from_digit(u32::from(b & 0x0f), 16)
                    .unwrap()
                    .to_ascii_uppercase(),
            );
        }
    }
    out
}

/// Build the preflight URL: `{base}/api/web/domain_info?domain=<encoded host>`.
/// Mirrors `utils.ts:184`.
#[must_use]
pub fn domain_info_url(domain: &str) -> String {
    format!(
        "{DOMAIN_CHECK_BASE_URL}/api/web/domain_info?domain={}",
        encode_uri_component(domain)
    )
}

// ---- 5-minute, 128-entry, host-keyed "allowed" cache --------------------------
// Mirrors the hand-rolled `cache.rs` style (no new dependency). Only `Allowed`
// results are cached (`utils.ts:74`: "blocked/failed re-check on next attempt").

/// One slot: insertion time (TTL) + recency tick (LRU eviction).
struct Slot {
    inserted_at: Instant,
    last_used: u64,
}

/// TTL + entry-bounded LRU of hosts known to be `Allowed`.
struct DomainCache {
    map: HashMap<String, Slot>,
    tick: u64,
}

impl DomainCache {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            tick: 0,
        }
    }

    fn next_tick(&mut self) -> u64 {
        self.tick = self.tick.wrapping_add(1);
        self.tick
    }

    /// `DOMAIN_CHECK_CACHE.has(domain)` (`utils.ts:179`): a still-live entry is a
    /// hit. Expired entries are evicted on access (lazy TTL, mirroring
    /// `lru-cache`). A hit refreshes the entry's recency.
    fn has(&mut self, domain: &str, now: Instant) -> bool {
        let expired = self
            .map
            .get(domain)
            .is_some_and(|slot| now.duration_since(slot.inserted_at) >= DOMAIN_CHECK_CACHE_TTL);
        if expired {
            self.map.remove(domain);
            return false;
        }
        let tick = self.next_tick();
        if let Some(slot) = self.map.get_mut(domain) {
            slot.last_used = tick;
            true
        } else {
            false
        }
    }

    /// `DOMAIN_CHECK_CACHE.set(domain, true)` (`utils.ts:189`): record `domain`
    /// as allowed, evicting the LRU host if the 128-entry cap is exceeded.
    fn set(&mut self, domain: String, now: Instant) {
        let tick = self.next_tick();
        self.map.insert(
            domain,
            Slot {
                inserted_at: now,
                last_used: tick,
            },
        );
        while self.map.len() > DOMAIN_CHECK_CACHE_MAX {
            let Some(victim) = self
                .map
                .iter()
                .min_by_key(|(_, slot)| slot.last_used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            self.map.remove(&victim);
        }
    }

    fn clear(&mut self) {
        self.map.clear();
    }
}

static DOMAIN_CHECK_CACHE: Lazy<Mutex<DomainCache>> = Lazy::new(|| Mutex::new(DomainCache::new()));

/// Empty the domain-check cache. Mirrors the `DOMAIN_CHECK_CACHE.clear()` half of
/// `clearWebFetchCache` (`utils.ts:80-83`).
pub fn clear_domain_check_cache() {
    DOMAIN_CHECK_CACHE
        .lock()
        .expect("DOMAIN_CHECK_CACHE poisoned")
        .clear();
}

/// Whether `domain` currently sits in the allowed cache (test/inspection seam).
#[must_use]
pub fn domain_cache_has(domain: &str) -> bool {
    DOMAIN_CHECK_CACHE
        .lock()
        .expect("DOMAIN_CHECK_CACHE poisoned")
        .has(domain, Instant::now())
}

/// Parse a preflight response body for `{ "can_fetch": <bool> }`.
///
/// Mirrors `response.data.can_fetch === true` (`utils.ts:188`): only an explicit
/// boolean `true` counts as fetchable. A missing/non-bool field, or unparseable
/// JSON, reads as `false` (→ `Blocked` at the 200 status).
#[must_use]
fn parse_can_fetch(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("can_fetch").and_then(serde_json::Value::as_bool))
        == Some(true)
}

/// Domain blocklist preflight. Faithful port of `checkDomainBlocklist`
/// (`utils.ts:176-203`).
///
/// On a cache hit returns [`DomainCheckResult::Allowed`] without any network
/// round-trip (`utils.ts:179-181`). Otherwise GETs the `domain_info` endpoint
/// with the [`DOMAIN_CHECK_TIMEOUT`] and maps the result. A transport error or
/// non-200 status yields [`DomainCheckResult::CheckFailed`] (fail-open: the
/// caller surfaces a user-facing `DomainCheckFailedError`).
pub async fn check_domain_blocklist(http: &dyn HttpTransport, domain: &str) -> DomainCheckResult {
    check_domain_blocklist_at(http, domain, Instant::now()).await
}

/// [`check_domain_blocklist`] with an injectable clock (test seam for TTL aging).
pub async fn check_domain_blocklist_at(
    http: &dyn HttpTransport,
    domain: &str,
    now: Instant,
) -> DomainCheckResult {
    // Cache hit short-circuit (`utils.ts:179-181`).
    {
        let mut cache = DOMAIN_CHECK_CACHE
            .lock()
            .expect("DOMAIN_CHECK_CACHE poisoned");
        if cache.has(domain, now) {
            return DomainCheckResult::Allowed;
        }
    }

    let req = HttpRequest {
        method: HttpMethod::Get,
        url: domain_info_url(domain),
        headers: vec![],
        body: None,
        body_bytes: None,
        timeout: Some(DOMAIN_CHECK_TIMEOUT),
    };

    match http.request(req).await {
        Ok(resp) if resp.status == 200 => {
            if parse_can_fetch(&resp.body) {
                let mut cache = DOMAIN_CHECK_CACHE
                    .lock()
                    .expect("DOMAIN_CHECK_CACHE poisoned");
                cache.set(domain.to_string(), now);
                DomainCheckResult::Allowed
            } else {
                DomainCheckResult::Blocked
            }
        }
        // Non-200 status but no transport error (`utils.ts:195-198`).
        Ok(resp) => {
            DomainCheckResult::CheckFailed(format!("Domain check returned status {}", resp.status))
        }
        // Transport error / timeout (`utils.ts:199-202`): fail-open.
        Err(e) => DomainCheckResult::CheckFailed(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::http::HttpError;
    use std::sync::Arc;
    use test_harness::mocks::{MockHttpTransport, ScriptedResponse};

    fn sync_resp(status: u16, body: &str) -> ScriptedResponse {
        ScriptedResponse::Sync(lingxi_core::types::HttpResponse {
            status,
            headers: vec![],
            body: body.to_string(),
            body_bytes: Vec::new(),
        })
    }

    // ---- byte-locked constants & messages ----------------------------------

    #[test]
    fn locked_constants_match_ts() {
        assert_eq!(DOMAIN_CHECK_TIMEOUT, Duration::from_millis(10_000));
        assert_eq!(DOMAIN_CHECK_CACHE_TTL, Duration::from_millis(5 * 60 * 1000));
        assert_eq!(DOMAIN_CHECK_CACHE_MAX, 128);
        assert_eq!(DOMAIN_CHECK_BASE_URL, "https://api.anthropic.com");
    }

    #[test]
    fn blocked_message_byte_locked() {
        assert_eq!(
            domain_blocked_msg("example.com"),
            "LingXi is unable to fetch from example.com"
        );
    }

    #[test]
    fn check_failed_message_byte_locked() {
        assert_eq!(
            domain_check_failed_msg("example.com"),
            "Unable to verify if domain example.com is safe to fetch. This may be due to \
             network restrictions or enterprise security policies blocking claude.ai."
        );
    }

    // ---- encodeURIComponent + preflight URL --------------------------------

    #[test]
    fn encode_uri_component_passes_through_hostnames() {
        // Plain hostnames are all unreserved → identity.
        assert_eq!(encode_uri_component("example.com"), "example.com");
        assert_eq!(
            encode_uri_component("sub-domain.example.co.uk"),
            "sub-domain.example.co.uk"
        );
    }

    #[test]
    fn encode_uri_component_encodes_reserved_and_unicode() {
        // ':' and '/' are reserved → encoded; matches encodeURIComponent.
        assert_eq!(encode_uri_component("a/b:c"), "a%2Fb%3Ac");
        // Space → %20 (NOT '+').
        assert_eq!(encode_uri_component("a b"), "a%20b");
        // Multi-byte UTF-8 (é = C3 A9) → percent-encoded per byte, uppercase hex.
        assert_eq!(encode_uri_component("café"), "caf%C3%A9");
        // encodeURIComponent unreserved punctuation survives.
        assert_eq!(encode_uri_component("a-_.!~*'()"), "a-_.!~*'()");
    }

    #[test]
    fn domain_info_url_is_byte_exact() {
        assert_eq!(
            domain_info_url("example.com"),
            "https://api.anthropic.com/api/web/domain_info?domain=example.com"
        );
        assert_eq!(
            domain_info_url("xn--caf-dma.com"),
            "https://api.anthropic.com/api/web/domain_info?domain=xn--caf-dma.com"
        );
    }

    // ---- parse_can_fetch ---------------------------------------------------

    #[test]
    fn parse_can_fetch_true_only_on_explicit_true() {
        assert!(parse_can_fetch(r#"{"can_fetch":true}"#));
        assert!(!parse_can_fetch(r#"{"can_fetch":false}"#));
        // strict `=== true`: a missing field, non-bool, or bad JSON → not allowed.
        assert!(!parse_can_fetch(r#"{"can_fetch":"true"}"#));
        assert!(!parse_can_fetch(r#"{"can_fetch":1}"#));
        assert!(!parse_can_fetch(r"{}"));
        assert!(!parse_can_fetch("not json"));
        assert!(!parse_can_fetch(""));
    }

    // ---- check_domain_blocklist result mapping -----------------------------
    // Each test uses a UNIQUE host so the process-global DOMAIN_CHECK_CACHE
    // (shared across parallel tests) can't cross-contaminate.

    #[tokio::test]
    async fn allowed_on_200_can_fetch_true() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        clear_domain_check_cache();
        let http = Arc::new(MockHttpTransport::new());
        http.enqueue(sync_resp(200, r#"{"can_fetch":true}"#));
        let res = check_domain_blocklist(http.as_ref(), "allowed-true.example").await;
        assert_eq!(res, DomainCheckResult::Allowed);
        // Preflight GET URL is byte-exact.
        let reqs = http.received_requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(
            reqs[0].url,
            "https://api.anthropic.com/api/web/domain_info?domain=allowed-true.example"
        );
        assert_eq!(reqs[0].method, HttpMethod::Get);
        assert_eq!(reqs[0].timeout, Some(DOMAIN_CHECK_TIMEOUT));
    }

    #[tokio::test]
    async fn blocked_on_200_can_fetch_false() {
        let http = Arc::new(MockHttpTransport::new());
        http.enqueue(sync_resp(200, r#"{"can_fetch":false}"#));
        let res = check_domain_blocklist(http.as_ref(), "blocked.example").await;
        assert_eq!(res, DomainCheckResult::Blocked);
    }

    #[tokio::test]
    async fn check_failed_on_non_200() {
        let http = Arc::new(MockHttpTransport::new());
        http.enqueue(sync_resp(503, "upstream down"));
        let res = check_domain_blocklist(http.as_ref(), "non200.example").await;
        assert_eq!(
            res,
            DomainCheckResult::CheckFailed("Domain check returned status 503".to_string())
        );
    }

    #[tokio::test]
    async fn check_failed_on_transport_error() {
        let http = Arc::new(MockHttpTransport::new());
        http.enqueue(ScriptedResponse::SyncErr(HttpError::Connection(
            "network unreachable".into(),
        )));
        let res = check_domain_blocklist(http.as_ref(), "transport-err.example").await;
        match res {
            DomainCheckResult::CheckFailed(msg) => assert!(msg.contains("network unreachable")),
            other => panic!("expected CheckFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn allowed_result_is_cached_no_second_request() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        clear_domain_check_cache();
        let http = Arc::new(MockHttpTransport::new());
        // Only ONE response enqueued: a cache hit must not consume a second.
        http.enqueue(sync_resp(200, r#"{"can_fetch":true}"#));
        let host = "cached.example";

        assert_eq!(
            check_domain_blocklist(http.as_ref(), host).await,
            DomainCheckResult::Allowed
        );
        assert!(domain_cache_has(host));
        // Second check within TTL: served from cache, NO network round-trip.
        assert_eq!(
            check_domain_blocklist(http.as_ref(), host).await,
            DomainCheckResult::Allowed
        );
        assert_eq!(
            http.received_requests().len(),
            1,
            "second check must hit the cache, not the network"
        );
    }

    #[tokio::test]
    async fn blocked_is_not_cached() {
        let _cache_guard = crate::testsupport::web_globals_lock().await;
        clear_domain_check_cache();
        let http = Arc::new(MockHttpTransport::new());
        // Two blocked responses: if blocked were cached, the second call would
        // not consume the second response.
        http.enqueue(sync_resp(200, r#"{"can_fetch":false}"#));
        http.enqueue(sync_resp(200, r#"{"can_fetch":false}"#));
        let host = "blocked-not-cached.example";
        assert_eq!(
            check_domain_blocklist(http.as_ref(), host).await,
            DomainCheckResult::Blocked
        );
        assert!(!domain_cache_has(host));
        assert_eq!(
            check_domain_blocklist(http.as_ref(), host).await,
            DomainCheckResult::Blocked
        );
        assert_eq!(http.received_requests().len(), 2);
    }

    // ---- cache TTL / eviction (local instance, deterministic) --------------

    #[test]
    fn local_cache_ttl_expires_on_has() {
        let mut c = DomainCache::new();
        let t0 = Instant::now();
        c.set("ttl.example".into(), t0);
        // Just under the TTL: hit.
        let almost = t0
            + DOMAIN_CHECK_CACHE_TTL
                .checked_sub(Duration::from_millis(1))
                .unwrap();
        assert!(c.has("ttl.example", almost));
        // At/after the TTL: miss + eviction.
        let after = t0 + DOMAIN_CHECK_CACHE_TTL;
        assert!(!c.has("ttl.example", after));
        // Even back at t0 it's gone (removed).
        assert!(!c.has("ttl.example", t0));
    }

    #[test]
    fn local_cache_entry_cap_evicts_lru() {
        let mut c = DomainCache::new();
        let now = Instant::now();
        for i in 0..=DOMAIN_CHECK_CACHE_MAX {
            c.set(format!("h{i}.example"), now);
        }
        assert_eq!(c.map.len(), DOMAIN_CHECK_CACHE_MAX);
        // First-inserted (LRU) host evicted; most-recent retained.
        assert!(!c.has("h0.example", now));
        assert!(c.has(&format!("h{DOMAIN_CHECK_CACHE_MAX}.example"), now));
    }

    #[test]
    fn local_cache_recency_protects_touched_host() {
        let mut c = DomainCache::new();
        let now = Instant::now();
        c.set("keep.example".into(), now);
        for i in 0..DOMAIN_CHECK_CACHE_MAX - 1 {
            c.set(format!("f{i}.example"), now);
        }
        assert_eq!(c.map.len(), DOMAIN_CHECK_CACHE_MAX);
        // Touch "keep" → most-recently-used.
        assert!(c.has("keep.example", now));
        // Overflow the cap; LRU victim must NOT be "keep".
        c.set("overflow.example".into(), now);
        assert!(c.has("keep.example", now));
    }
}
