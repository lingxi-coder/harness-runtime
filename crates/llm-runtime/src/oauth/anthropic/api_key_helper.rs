//! `apiKeyHelper` executor + TTL cache.
//!
//! 1:1 port of claude-code 2.1.207's `apiKeyHelper` auth-helper machinery. The
//! `apiKeyHelper` setting (`llm_runtime::…::SettingsJson`-adjacent, wired at the
//! composition root) is a command line whose trimmed stdout is used as the
//! Anthropic auth value. This module ports the three load-bearing pieces:
//!
//! * [`api_key_helper_ttl_ms`] — binary `obc()`: read
//!   `CLAUDE_CODE_API_KEY_HELPER_TTL_MS`, `parseInt(_,10) >= 0` else error-log
//!   the byte-exact message and fall back to the 5-minute default `DTh=300000`.
//! * [`run_api_key_helper`] — binary `LTh()`'s exec core: run the helper with a
//!   600s timeout (`yI(t,{timeout:600000,reject:!1})`), capture + trim stdout,
//!   surface the byte-exact failure messages (`exited {code}[: {stderr}]` /
//!   `timed out` / `did not return a value`).
//! * [`ApiKeyHelperCache`] + [`fetch_api_key`] — binary `ise` cache + `Bqt`/`W_c`:
//!   reuse the cached value while `Date.now() - timestamp < ttl`, otherwise
//!   re-invoke, and on failure log `apiKeyHelper failed: {o}` +
//!   `Error getting API key from apiKeyHelper: {o}`.
//!
//! The env var keeps its `CLAUDE_CODE_` name verbatim (this repo preserves the
//! `CLAUDE_CODE_*` protocol/env names — cf. `CLAUDE_CODE_EXTRA_BODY`,
//! `CLAUDE_CODE_OAUTH_TOKEN`), matching the binary's process-env read exactly.
//!
//! The binary's single-flight in-flight-promise dedup (`a3e`) and the
//! workspace-trust gate (`nbc()`/`bd()` → `tengu_apiKeyHelper_missing_trust11`)
//! are not ported here.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// `DTh` — the default helper TTL: 5 minutes, in milliseconds.
pub const DEFAULT_API_KEY_HELPER_TTL_MS: u64 = 300_000;

/// `{timeout:600000}` — the helper execution timeout: 10 minutes.
pub const API_KEY_HELPER_TIMEOUT: Duration = Duration::from_secs(600);

/// The `CLAUDE_CODE_API_KEY_HELPER_TTL_MS` env var name (kept verbatim — this
/// repo preserves `CLAUDE_CODE_*` env names).
pub const API_KEY_HELPER_TTL_ENV: &str = "CLAUDE_CODE_API_KEY_HELPER_TTL_MS";

/// `obc()` — resolve the helper-value TTL (milliseconds).
///
/// ```js
/// function obc(){
///   let e=process.env.CLAUDE_CODE_API_KEY_HELPER_TTL_MS;
///   if(e){
///     let t=parseInt(e,10);
///     if(!Number.isNaN(t)&&t>=0)return t;
///     C(`Found CLAUDE_CODE_API_KEY_HELPER_TTL_MS env var, but it was not a valid number. Got ${e}`,{level:"error"})
///   }
///   return DTh   // 300000
/// }
/// ```
///
/// A present-but-invalid value (`NaN` or negative) logs the byte-exact error at
/// `error` level and falls back to [`DEFAULT_API_KEY_HELPER_TTL_MS`].
#[must_use]
pub fn api_key_helper_ttl_ms() -> u64 {
    match std::env::var(API_KEY_HELPER_TTL_ENV) {
        Ok(raw) if !raw.is_empty() => resolve_ttl_ms(&raw),
        _ => DEFAULT_API_KEY_HELPER_TTL_MS,
    }
}

/// The pure `obc()` core over a captured env value (testable without touching
/// the process environment).
#[must_use]
pub fn resolve_ttl_ms(raw: &str) -> u64 {
    // `Z.CLAUDE_CODE_API_KEY_HELPER_TTL_MS` is coerced by the shared `hp` helper
    // (2.1.211+: scientific notation + digit separators), then `jMc` keeps it
    // when `!Number.isNaN(e) && e >= 0`.
    let t = platform_api::env::parse_int_env(raw);
    if !t.is_nan() && t >= 0.0 {
        return t as u64;
    }
    tracing::error!(
        "Found {API_KEY_HELPER_TTL_ENV} env var, but it was not a valid number. Got {raw}"
    );
    DEFAULT_API_KEY_HELPER_TTL_MS
}

/// `LTh()`'s exec core with the default 600s timeout.
///
/// # Errors
///
/// Returns the byte-exact failure message on non-zero exit (`exited {code}` or
/// `exited {code}: {stderr}`), timeout (`timed out`), empty output
/// (`did not return a value`), or spawn failure (the OS error string).
pub async fn run_api_key_helper(command: &str) -> Result<String, String> {
    run_api_key_helper_with_timeout(command, API_KEY_HELPER_TIMEOUT).await
}

/// `LTh()`'s exec core, timeout injectable for tests.
///
/// ```js
/// let r=await yI(t,{timeout:600000,reject:!1});
/// if(r.failed){
///   let o=r.timedOut?"timed out":`exited ${r.exitCode}`,i=r.stderr?.trim();
///   throw Error(i?`${o}: ${i}`:o)
/// }
/// let n=r.stdout?.trim();
/// if(!n)throw Error("did not return a value");
/// return n
/// ```
///
/// The helper is run through the platform shell (`sh -c <command>`), matching
/// the binary's `child_process`-through-a-shell execution of the configured
/// command line (a bare script path or a full command both work).
///
/// # Errors
///
/// See [`run_api_key_helper`].
pub async fn run_api_key_helper_with_timeout(
    command: &str,
    timeout: Duration,
) -> Result<String, String> {
    let child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // `{timeout}` SIGTERMs the child; killing on drop of the timed-out
        // future mirrors that.
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;

    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        // `r.timedOut` branch: `o = "timed out"`. (Best-effort: the binary also
        // appends a trimmed stderr; on timeout the pipe is not drained here.)
        Err(_elapsed) => Err("timed out".to_string()),
        Ok(Err(e)) => Err(e.to_string()),
        Ok(Ok(output)) => {
            if !output.status.success() {
                // `exited ${r.exitCode}` — signal-terminated (no code) → JS `null`.
                let code = output
                    .status
                    .code()
                    .map_or_else(|| "null".to_string(), |c| c.to_string());
                let o = format!("exited {code}");
                let stderr = String::from_utf8_lossy(&output.stderr);
                let i = stderr.trim();
                return Err(if i.is_empty() { o } else { format!("{o}: {i}") });
            }
            let stdout = String::from_utf8_lossy(&output.stdout);
            let n = stdout.trim();
            if n.is_empty() {
                return Err("did not return a value".to_string());
            }
            Ok(n.to_string())
        }
    }
}

/// `ise` — the process-level cached helper value with its capture instant.
///
/// `Bqt` reuses the cached value while `Date.now() - ise.timestamp < ttl`
/// (here monotonic [`Instant`] elapsed vs the TTL), otherwise re-invokes.
#[derive(Debug, Default)]
pub struct ApiKeyHelperCache {
    inner: Mutex<Option<Cached>>,
}

#[derive(Debug, Clone)]
struct Cached {
    value: String,
    at: Instant,
}

impl ApiKeyHelperCache {
    /// A fresh, empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    /// Return the cached value iff it is still within `ttl_ms`
    /// (`Date.now() - ise.timestamp < t`).
    #[must_use]
    pub fn get_fresh(&self, ttl_ms: u64) -> Option<String> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cached = guard.as_ref()?;
        if cached.at.elapsed() < Duration::from_millis(ttl_ms) {
            Some(cached.value.clone())
        } else {
            None
        }
    }

    /// Store a freshly-fetched value with the current instant
    /// (`ise = {value, timestamp: Date.now()}`).
    pub fn store(&self, value: String) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some(Cached {
            value,
            at: Instant::now(),
        });
    }

    /// `fjt()` — clear the cache (`ise = null`).
    pub fn clear(&self) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = None;
    }
}

/// `Bqt`/`W_c` observable behavior: return the cached value while fresh, else
/// re-invoke the helper, caching + returning the new value on success, or
/// logging both byte-exact failure lines and returning `None` on error.
///
/// (The binary's single-flight in-flight-promise dedup `a3e` is a concurrency
/// optimization, not an observable difference for a single caller; it is not
/// ported here.)
pub async fn fetch_api_key(
    command: &str,
    cache: &ApiKeyHelperCache,
    ttl_ms: u64,
) -> Option<String> {
    fetch_api_key_result(command, cache, ttl_ms).await.ok()
}

/// Same as [`fetch_api_key`], but preserves the helper failure string for live
/// auth resolution paths that need to surface the exact helper error instead of
/// collapsing it into a generic authentication failure.
pub async fn fetch_api_key_result(
    command: &str,
    cache: &ApiKeyHelperCache,
    ttl_ms: u64,
) -> Result<String, String> {
    if let Some(v) = cache.get_fresh(ttl_ms) {
        return Ok(v);
    }
    match run_api_key_helper(command).await {
        Ok(key) => {
            cache.store(key.clone());
            Ok(key)
        }
        Err(o) => {
            // `console.error(mt.red(\`apiKeyHelper failed: ${o}\`))` (color dropped).
            eprintln!("apiKeyHelper failed: {o}");
            // `C(\`Error getting API key from apiKeyHelper: ${o}\`,{level:"error"})`.
            tracing::error!("Error getting API key from apiKeyHelper: {o}");
            Err(o)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── obc() / TTL ─────────────────────────────────────────────────────────

    #[test]
    fn ttl_default_when_absent_or_empty() {
        assert_eq!(resolve_ttl_ms(""), DEFAULT_API_KEY_HELPER_TTL_MS);
    }

    #[test]
    fn ttl_parses_valid_non_negative() {
        assert_eq!(resolve_ttl_ms("0"), 0);
        assert_eq!(resolve_ttl_ms("60000"), 60_000);
        // parseInt semantics: leading digits, trailing junk ignored.
        assert_eq!(resolve_ttl_ms("90000ms"), 90_000);
    }

    #[test]
    fn ttl_invalid_or_negative_falls_back_to_default() {
        // NaN (no leading digits) → default.
        assert_eq!(resolve_ttl_ms("abc"), DEFAULT_API_KEY_HELPER_TTL_MS);
        // Negative (`t >= 0` fails) → default.
        assert_eq!(resolve_ttl_ms("-5"), DEFAULT_API_KEY_HELPER_TTL_MS);
    }

    #[test]
    fn default_ttl_is_five_minutes() {
        assert_eq!(DEFAULT_API_KEY_HELPER_TTL_MS, 300_000);
    }

    // ── LTh() executor ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn helper_stdout_is_trimmed_and_returned() {
        let key = run_api_key_helper("printf '  sk-test-123  \\n'")
            .await
            .expect("helper should succeed");
        assert_eq!(key, "sk-test-123");
    }

    #[tokio::test]
    async fn empty_output_is_did_not_return_a_value() {
        let err = run_api_key_helper("printf ''")
            .await
            .expect_err("empty output must error");
        assert_eq!(err, "did not return a value");
    }

    #[tokio::test]
    async fn whitespace_only_output_is_did_not_return_a_value() {
        let err = run_api_key_helper("printf '   \\n'")
            .await
            .expect_err("whitespace-only output must error");
        assert_eq!(err, "did not return a value");
    }

    #[tokio::test]
    async fn nonzero_exit_reports_code_and_trimmed_stderr() {
        let err = run_api_key_helper("echo boom 1>&2; exit 3")
            .await
            .expect_err("non-zero exit must error");
        assert_eq!(err, "exited 3: boom");
    }

    #[tokio::test]
    async fn nonzero_exit_without_stderr_reports_bare_code() {
        let err = run_api_key_helper("exit 7")
            .await
            .expect_err("non-zero exit must error");
        assert_eq!(err, "exited 7");
    }

    #[tokio::test]
    async fn timeout_reports_timed_out() {
        let err = run_api_key_helper_with_timeout("sleep 5", Duration::from_millis(50))
            .await
            .expect_err("a slow helper must time out");
        assert_eq!(err, "timed out");
    }

    // ── ise cache / Bqt ─────────────────────────────────────────────────────

    #[test]
    fn cache_returns_value_within_ttl_and_expires_after() {
        let cache = ApiKeyHelperCache::new();
        assert_eq!(cache.get_fresh(1_000), None);
        cache.store("sk-abc".to_string());
        // Fresh within a generous TTL.
        assert_eq!(cache.get_fresh(60_000), Some("sk-abc".to_string()));
        // TTL of 0 ms → immediately stale (`Date.now()-ts < 0` is false).
        assert_eq!(cache.get_fresh(0), None);
        cache.clear();
        assert_eq!(cache.get_fresh(60_000), None);
    }

    #[tokio::test]
    async fn fetch_api_key_caches_and_reuses_within_ttl() {
        let cache = ApiKeyHelperCache::new();
        // First fetch runs the helper and caches.
        let first = fetch_api_key("printf 'sk-one'", &cache, 60_000).await;
        assert_eq!(first, Some("sk-one".to_string()));
        // Second fetch within TTL returns the CACHED value even though the
        // command now yields a different key.
        let second = fetch_api_key("printf 'sk-two'", &cache, 60_000).await;
        assert_eq!(second, Some("sk-one".to_string()));
        // With a stale TTL the helper is re-invoked and the new value cached.
        let third = fetch_api_key("printf 'sk-two'", &cache, 0).await;
        assert_eq!(third, Some("sk-two".to_string()));
    }

    #[tokio::test]
    async fn fetch_api_key_returns_none_on_helper_failure() {
        let cache = ApiKeyHelperCache::new();
        let got = fetch_api_key("exit 1", &cache, 60_000).await;
        assert_eq!(got, None);
        // A failed fetch does not populate the cache.
        assert_eq!(cache.get_fresh(60_000), None);
    }
}
