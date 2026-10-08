//! Exponential-backoff retry driver over llm-runtime's error taxonomy.
//!
//! This module ports the cadence constants and jitter helper from
//! `api-client/src/retry.rs` and adds a new pure decision function
//! [`next_step`] that maps `(RetryState, RetryControl, LlmError)` to a
//! [`DriveStep`] — what the caller should do next after one failed attempt.
//!
//! The function is **synchronous and pure**: the caller is responsible for
//! actually sleeping (`tokio::time::sleep`) and re-executing the request.
//! This separation keeps the driver unit-testable without a Tokio runtime and
//! leaves all transport / UI emission to Plan 3 wiring.
//!
//! ## 429 subscriber gate (parity `withRetry.ts:767`)
//!
//! `RateLimited` (HTTP 429) is retried **only when** `!is_subscriber ||
//! is_enterprise`. A plain Claude.ai subscriber (non-enterprise) hits their
//! rate limit and must wait; retrying would just burn the budget and hammer
//! the server. Enterprise subscribers and API-key users do retry (same as
//! claude-code).
//!
//! ## `resolve_retry_control` (parity `api-client/src/anthropic.rs:1187-1211`)
//!
//! Computes the per-request [`RetryControl`] from env + subscriber state.
//! Injectable via [`ResolveRetryEnv`] for deterministic tests.
//! `CLAUDE_CODE_UNATTENDED_RETRY` / persistent-mode is Anthropic-internal-only —
//! not ported.

#![forbid(unsafe_code)]

use rand::Rng;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Cadence constants — byte-locked against the v2.1.183 binary's `sle`
// retry-delay helper (the main API request retry loop `Tzn`).
// ---------------------------------------------------------------------------
//
// v2.1.183 binary (`bin/claude.exe`, offsets confirmed against the build):
//
// ```js
// function sle(e, t, n = 32000) {                       // e=attempt(1-indexed), t=retry-after header, n=cap
//   let r = Math.min(Cbm * Math.pow(2, e - 1), n),      // Cbm = 500 → base = min(500 * 2^(attempt-1), 32000)
//       o = r + Math.random() * 0.25 * r;               // additive jitter: r + rand(0, 0.25) * r
//   if (t) { let s = parseInt(t, 10); if (!isNaN(s)) return Math.max(s * 1000, o) }  // honor retry-after header
//   return o
// }
// ```
//
// Constants block @ binary offset 206068108:
//   `_bm=10, JIo=3000, ybm=3, ..., Cbm=500, vbm=60000, ..., pFl=300000, XIo=21600000`
// `sle`'s default cap param `n=32000`.
//
// This is the **exponential** backoff (`500 * 2^(attempt-1)`, capped at 32000ms)
// with **additive** jitter (`+ rand(0, 0.25) * base`) — NOT the old `[500, 1000,
// 2000]` table with multiplicative `× uniform(0.8, 1.2)` jitter.

/// Base delay in milliseconds: the first retry waits ~`500ms` (before jitter).
/// Binary `Cbm = 500`. Each subsequent attempt doubles up to [`MAX_BACKOFF_MS`].
pub const BASE_DELAY_MS: u64 = 500;

/// Maximum (pre-jitter) backoff in milliseconds. Binary `sle`'s default cap
/// param `n = 32000`. The exponential `500 * 2^(attempt-1)` is clamped here.
pub const MAX_BACKOFF_MS: u64 = 32_000;

/// Default maximum number of retries. Byte-locked to claude-code
/// `withRetry.ts:52` (`const DEFAULT_MAX_RETRIES = 10`). Combined with the
/// loop bound `attempt <= maxRetries + 1` (`withRetry.ts:189`) this permits up
/// to 11 executions / 10 sleeps at the default. Overridable via
/// `LINGXI_MAX_RETRIES` (see [`max_retries_from_env`]).
pub const DEFAULT_MAX_RETRIES: u32 = 10;

/// Resolve the configured max-retries from a raw `LINGXI_MAX_RETRIES`
/// value. Mirrors claude-code `getDefaultMaxRetries` (`withRetry.ts:789-793`):
/// when the env var is present its `parseInt` is used, otherwise (absent or
/// unparseable) the default of [`DEFAULT_MAX_RETRIES`] applies.
#[must_use]
pub fn max_retries_from_env_value(v: Option<&str>) -> u32 {
    match v {
        Some(s) => s.trim().parse::<u32>().unwrap_or(DEFAULT_MAX_RETRIES),
        None => DEFAULT_MAX_RETRIES,
    }
}

/// Read `LINGXI_MAX_RETRIES` from the process environment and resolve the
/// effective max-retries. Mirrors claude-code `getDefaultMaxRetries`
/// (`withRetry.ts:789-796`).
#[must_use]
pub fn max_retries_from_env() -> u32 {
    max_retries_from_env_value(std::env::var("LINGXI_MAX_RETRIES").ok().as_deref())
}

/// Retry-watchdog default max-retries. Binary `_j_ = 300` — when the
/// `CLAUDE_CODE_RETRY_WATCHDOG` watchdog is ON and no explicit override is set,
/// the default retry budget jumps from [`DEFAULT_MAX_RETRIES`] (10) to 300
/// (`pDs()`'s `return e ? _j_ : yj_`).
pub const WATCHDOG_MAX_RETRIES: u32 = 300;

/// Hard clamp applied to an explicit `LINGXI_MAX_RETRIES` when the retry
/// watchdog is OFF. Binary `ufa = 15`: `pDs()` clamps any value `> 15` down to
/// 15 (with a one-time `warn`) unless the watchdog lifts the cap.
pub const MAX_RETRIES_CLAMP: u32 = 15;

/// Backoff cap (ms) the retry watchdog raises the capacity-retry ladder to.
/// Binary `TLp = 21_600_000` (6 hours) — under the watchdog a sustained 529/429
/// capacity outage keeps retrying with the exponential ladder capped at 6h
/// instead of the normal [`MAX_BACKOFF_MS`] (32s). `Math.min(n, TLp)`.
pub const WATCHDOG_MAX_BACKOFF_MS: u64 = 21_600_000;

/// One-time latch for the `LINGXI_MAX_RETRIES … clamped to 15` warning. Binary
/// `bLp` (`bLp=!1` initially, set `!0` on first clamp) so the warning fires at
/// most once per process.
static MAX_RETRIES_CLAMP_WARNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Current native typed boolean parsing for the branded retry watchdog.
#[must_use]
pub fn retry_watchdog_from_value(value: Option<&str>) -> bool {
    value.is_some_and(crate::structured_output::bool_value)
}

/// Read the canonical branded retry-watchdog flag from the environment.
#[must_use]
pub fn retry_watchdog_from_env() -> bool {
    retry_watchdog_from_value(std::env::var(branding::RETRY_WATCHDOG_ENV).ok().as_deref())
}

/// Byte-faithful port of the binary's `pDs()` max-retries resolver.
///
/// ```js
/// function pDs(){
///   let e = oMe();                                   // watchdog on?
///   if (process.env.CLAUDE_CODE_MAX_RETRIES) {
///     let t = parseInt(process.env.CLAUDE_CODE_MAX_RETRIES, 10);
///     if (Number.isFinite(t) && t >= 0) {
///       if (t > ufa && !e) {                         // ufa = 15
///         if (!bLp) { bLp = true; warn(`…=${t} clamped to ${ufa}`) }
///         return ufa;                                // 15
///       }
///       return t;
///     }
///   }
///   return e ? _j_ : yj_;                            // 300 : 10
/// }
/// ```
///
/// `env_raw` is the raw `LINGXI_MAX_RETRIES` value (lingxi's rename of
/// `CLAUDE_CODE_MAX_RETRIES`). An empty / unparseable / negative value falls
/// through to the `watchdog ? 300 : 10` default exactly as the JS `if
/// (Number.isFinite(t) && t >= 0)` guard does. When the watchdog is OFF an
/// explicit value `> 15` is clamped to 15 and warned once.
#[must_use]
pub fn resolve_max_retries(watchdog: bool, env_raw: Option<&str>) -> u32 {
    if let Some(s) = env_raw {
        // `parseInt` accepts leading digits; a `u32` parse is stricter but
        // already covers `finite && >= 0`. A non-empty, unparseable, or
        // negative value falls through to the default below (JS guard false).
        if let Ok(t) = s.trim().parse::<u32>() {
            if t > MAX_RETRIES_CLAMP && !watchdog {
                if !MAX_RETRIES_CLAMP_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    tracing::warn!("LINGXI_MAX_RETRIES={t} clamped to {MAX_RETRIES_CLAMP}");
                }
                return MAX_RETRIES_CLAMP;
            }
            return t;
        }
    }
    if watchdog {
        WATCHDOG_MAX_RETRIES
    } else {
        DEFAULT_MAX_RETRIES
    }
}

/// Consecutive-529 threshold before the fallback / repeated-overload decision
/// fires. Byte-locked to claude-code `withRetry.ts:54`
/// (`const MAX_529_RETRIES = 3`).
pub const MAX_529_RETRIES: u8 = 3;

/// Additive jitter fraction. Binary `sle`: `o = r + Math.random() * 0.25 * r`,
/// so the delay is `base + uniform(0, 0.25) * base`, i.e. uniform over
/// `[base, 1.25 * base)`.
pub const JITTER_FRACTION: f64 = 0.25;

/// Compute a jittered delay: `base_ms + uniform(0, 0.25) * base_ms`.
///
/// Byte-faithful to the v2.1.183 binary's `sle` jitter
/// (`r + Math.random() * 0.25 * r`). The result is uniform over
/// `[base_ms, 1.25 * base_ms)`. Uses `rand::thread_rng()` so independent retry
/// loops do not share state across tokio tasks.
///
/// **Precision note**: the spec works in milliseconds where sub-ms accuracy
/// is irrelevant to network timing. `base_ms as f64` and the round-trip back
/// to `u64` are intentional — `clippy::cast_*` lints are silenced locally
/// with justification.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "ms-scale timing; the jitter fraction is in [0, 0.25) so base + frac*base stays in u64 range and is non-negative"
)]
pub fn jittered_delay(base_ms: u64) -> Duration {
    // Binary `sle`: o = r + Math.random() * 0.25 * r. `gen_range(0.0..0.25)`
    // mirrors `Math.random() * 0.25` (half-open `[0, 0.25)`).
    let frac: f64 = rand::rng().random_range(0.0..JITTER_FRACTION);
    // Add in f64, cast back to u64 ms. Rounding direction does not matter
    // (we're in milliseconds; sub-ms accuracy is irrelevant to network timing).
    let ms = ((base_ms as f64) + (base_ms as f64) * frac) as u64;
    Duration::from_millis(ms)
}

// ---------------------------------------------------------------------------
// Driver types
// ---------------------------------------------------------------------------

/// What the driver decided after one failed attempt.
#[derive(Debug, Clone, PartialEq)]
pub enum DriveStep {
    /// Sleep `delay` then retry the same request.
    RetryAfter(Duration),
    /// Re-encode with this `max_tokens` then retry (529-independent).
    AdjustMaxTokens(u32),
    /// Strip thinking / redacted_thinking blocks and retry the same request
    /// (2.1.259 thinking-signature 400). Does **not** consume a budget attempt.
    /// Once per request; a second hit is [`Terminal`]. Applies to any provider
    /// whose 400 copy matches the classifier.
    StripThinkingSignature,
    /// Switch to the fallback model then retry.
    Fallback {
        /// The model identifier to use for the fallback attempt.
        fallback_model: String,
    },
    /// Surface the error to the caller.
    Terminal,
    /// Surface a "Repeated 529 Overloaded errors" error to the caller.
    ///
    /// Emitted when `allow_fallback && consecutive_overloaded >= max_529_retries
    /// && fallback_model.is_none() && is_external && !is_sandbox` — the external
    /// non-sandbox terminal branch (TS `withRetry.ts:354-362`). Distinct from
    /// [`Terminal`] so callers can produce the byte-locked
    /// `REPEATED_529_ERROR_MESSAGE` copy (`errors.ts:166`) without needing to
    /// re-inspect the [`RetryState`].
    RepeatedOverloaded,
}

/// Per-call mutable retry state (attempts, consecutive overloads).
#[derive(Debug, Default)]
pub struct RetryState {
    /// Number of budget-consuming retry attempts taken so far.
    ///
    /// `u32` to match [`RetryControl::max_retries`]: under the retry watchdog
    /// the budget defaults to [`WATCHDOG_MAX_RETRIES`] (300), which a `u8`
    /// (max 255, and `saturating_add`-clamped) could never reach — the
    /// terminal check `attempt >= max_retries` would then be `255 >= 300`
    /// forever, retrying persistent non-capacity errors indefinitely.
    pub attempt: u32,
    /// Capacity waits under the persistent retry watchdog. These advance the
    /// backoff ladder without spending an ordinary retry (`Ese`'s `B`, while
    /// `$t` is decremented after each persistent 429/529 wait).
    pub watchdog_capacity_waits: u32,
    /// Count of consecutive `Overloaded` errors without an intervening
    /// non-overloaded outcome.
    pub consecutive_overloaded: u8,
    /// `true` when the configured credential is a Claude.ai OAuth subscriber.
    ///
    /// Gates the 429 retry: `!is_subscriber || is_enterprise` mirrors
    /// `withRetry.ts:767`. A subscriber-non-enterprise 429 is **terminal**;
    /// enterprise subscribers and API-key users retry.
    pub is_subscriber: bool,
    /// `true` when the subscriber is an enterprise account.
    ///
    /// Re-enables 429 retry for subscribers on enterprise plans
    /// (`withRetry.ts:767`).
    pub is_enterprise: bool,
    /// `true` ⇒ a `RateLimited` (429) is TERMINAL (surfaced immediately, no
    /// retry). Set for OpenRouter FREE-tier models (`…:free`): their 429 is
    /// shared-quota exhaustion that does not clear within the retry-backoff
    /// window, so burning the ~160s ladder just delays the inevitable error.
    /// `false` (the default) keeps Claude Code's parity 429-retry for Anthropic
    /// and every PAID model.
    pub rate_limit_terminal: bool,
    /// `true` once this request already applied
    /// [`DriveStep::StripThinkingSignature`]. A second thinking-signature 400
    /// is terminal (the strip is once-per-request, like overflow adjustment).
    pub thinking_signature_stripped: bool,
}

/// Consecutive-529 / Opus-fallback policy threaded into [`next_step`].
///
/// Ports the consecutive-529 block of claude-code `withRetry.ts:326-365`. The
/// driver maintains a `consecutive_overloaded` counter inside [`RetryState`]
/// that increments on each `Overloaded` response and **resets to 0 on any
/// non-overloaded error**. When the counter reaches [`MAX_529_RETRIES`] *and*
/// [`Self::allow_fallback`] is set and a fallback model is configured, the
/// driver returns [`DriveStep::Fallback`].
///
/// `allow_fallback` is the **pre-computed** TS guard
/// `FALLBACK_FOR_ALL_PRIMARY_MODELS || (!isClaudeAISubscriber() &&
/// isNonCustomOpusModel(model))` (`withRetry.ts:331-332`) — the caller resolves
/// the env flag / subscriber state / Opus check once and hands the result in.
#[derive(Debug, Clone)]
pub struct RetryControl {
    /// Consecutive-529 threshold. Defaults to [`MAX_529_RETRIES`] (3).
    pub max_529_retries: u8,
    /// Maximum number of retries. Defaults to [`DEFAULT_MAX_RETRIES`] (10).
    ///
    /// Overridable via `LINGXI_MAX_RETRIES` — wired by
    /// [`resolve_retry_control`] reading [`ResolveRetryEnv::max_retries`].
    /// Mirrors `withRetry.ts:789-796` (`getMaxRetries`).
    pub max_retries: u32,
    /// Fallback model to signal via [`DriveStep::Fallback`] once the
    /// threshold trips; `None` disables the fallback signal.
    pub fallback_model: Option<String>,
    /// The primary model in flight — carried for caller context / logging.
    pub primary_model: String,
    /// Pre-computed TS guard (`FALLBACK_FOR_ALL_PRIMARY_MODELS ||
    /// (!is_subscriber && is_non_custom_opus(primary_model))`). When `false`,
    /// the consecutive-529 gate never trips and the loop behaves as budget-only.
    pub allow_fallback: bool,
    /// `USER_TYPE === 'external'` (claude-code `withRetry.ts:354`). Gates the
    /// no-fallback `Overloaded { repeated: true }` terminal branch.
    pub is_external: bool,
    /// `!!process.env.IS_SANDBOX` (claude-code `withRetry.ts:355`). When set,
    /// the no-fallback terminal branch is skipped (sandbox keeps retrying).
    pub is_sandbox: bool,
    /// `oMe()` — the `CLAUDE_CODE_RETRY_WATCHDOG` retry watchdog is ON. When set,
    /// capacity errors (529 Overloaded / 429 RateLimited) are EXEMPT from
    /// retry-budget exhaustion: [`next_step`] keeps retrying them past
    /// `max_retries` with the exponential ladder capped at
    /// [`WATCHDOG_MAX_BACKOFF_MS`] (6h) instead of [`MAX_BACKOFF_MS`] (32s),
    /// mirroring the binary's separate watchdog counter. Non-capacity errors
    /// (transport / provider-internal / invalid-request) still honour the
    /// budget. Default `false`.
    pub watchdog: bool,
}

impl Default for RetryControl {
    /// Behaviour-neutral default: no fallback configured and the gate disabled,
    /// so [`next_step`] reduces to budget-only retry. Used by callers that
    /// have not wired the consecutive-529 policy yet.
    fn default() -> Self {
        Self {
            max_529_retries: MAX_529_RETRIES,
            max_retries: DEFAULT_MAX_RETRIES,
            fallback_model: None,
            primary_model: String::new(),
            allow_fallback: false,
            is_external: false,
            is_sandbox: false,
            watchdog: false,
        }
    }
}

// ---------------------------------------------------------------------------
// resolve_retry_control — ported from api-client/src/anthropic.rs:1187-1211
// ---------------------------------------------------------------------------

/// Injectable environment for [`resolve_retry_control`].
///
/// Mirrors the same `UserAgentEnv` injectable pattern (Task 3) so tests can
/// supply deterministic values without mutating `std::env`.  The
/// `from_process_env()` constructor reads the real process environment for
/// production callers.
///
/// **Not ported:** `CLAUDE_CODE_UNATTENDED_RETRY` / persistent-mode is
/// Anthropic-internal-only and has no effect in external builds.
#[derive(Debug, Clone, Default)]
pub struct ResolveRetryEnv {
    /// Raw value of `FALLBACK_FOR_ALL_PRIMARY_MODELS`. A non-empty string is
    /// truthy (mirrors JS `process.env.FALLBACK_FOR_ALL_PRIMARY_MODELS ||`).
    pub fallback_for_all: Option<String>,
    /// Raw value of `USER_TYPE`. `"external"` means `is_external = true`.
    pub user_type: Option<String>,
    /// Whether `IS_SANDBOX` is defined at all. Any defined value (including
    /// empty string) counts as sandboxed — mirrors TS `!!process.env.IS_SANDBOX`.
    pub is_sandbox_defined: bool,
    /// Raw value of `LINGXI_MAX_RETRIES`. When `Some`, parsed as `u32`;
    /// absent or unparseable → the watchdog-aware default via
    /// [`resolve_max_retries`]. Mirrors `pDs()` (`CLAUDE_CODE_MAX_RETRIES`).
    pub max_retries: Option<String>,
    /// `oMe()` — the `CLAUDE_CODE_RETRY_WATCHDOG` watchdog flag
    /// (`LINGXI_RETRY_WATCHDOG` else the `CLAUDE_CODE_` alias). Drives the
    /// max-retries default (`watchdog ? 300 : 10`), lifts the `>15` clamp, and
    /// exempts capacity errors from retry-budget exhaustion in [`next_step`].
    pub retry_watchdog: bool,
}

impl ResolveRetryEnv {
    /// Read the live process environment. For production callers.
    #[must_use]
    pub fn from_process_env() -> Self {
        Self {
            fallback_for_all: std::env::var("FALLBACK_FOR_ALL_PRIMARY_MODELS").ok(),
            user_type: std::env::var("USER_TYPE").ok(),
            is_sandbox_defined: std::env::var_os("IS_SANDBOX").is_some(),
            max_retries: std::env::var("LINGXI_MAX_RETRIES").ok(),
            retry_watchdog: retry_watchdog_from_env(),
        }
    }
}

/// Compute the per-request [`RetryControl`] from env + subscriber state.
///
/// Ports `api-client/src/anthropic.rs::resolve_retry_control` (`:1187-1211`)
/// re-typed off api-client. Accepts an injectable [`ResolveRetryEnv`] so
/// tests can supply deterministic values.
///
/// **Policy:**
/// - `allow_fallback` = `FALLBACK_FOR_ALL_PRIMARY_MODELS` truthy **or**
///   (`!is_subscriber` and `is_non_custom_opus(model)`)
/// - `is_external` = `USER_TYPE == "external"`
/// - `is_sandbox` = `IS_SANDBOX` env var is defined (any value)
/// - `max_retries` = `LINGXI_MAX_RETRIES` parsed as `u32`, or
///   [`DEFAULT_MAX_RETRIES`] when absent/unparseable (mirrors
///   `withRetry.ts:789-796`)
///
/// **Not ported:** `CLAUDE_CODE_UNATTENDED_RETRY` / persistent-mode is
/// ant-only.
#[must_use]
pub fn resolve_retry_control(
    model: &str,
    fallback_model: Option<String>,
    is_subscriber: bool,
    env: &ResolveRetryEnv,
) -> RetryControl {
    resolve_retry_control_with_settings(model, fallback_model, is_subscriber, env, None)
}

/// Extended form of [`resolve_retry_control`] that additionally accepts a
/// `settings_max_retries` value from `routing.retry.maxAttempts`.
///
/// ## Precedence
///
/// `LINGXI_MAX_RETRIES` env **>** `settings_max_retries` **>** [`DEFAULT_MAX_RETRIES`].
///
/// When `env.max_retries` is present and parseable, it wins regardless of
/// `settings_max_retries`.  When the env var is absent, `settings_max_retries`
/// is used as the effective default before falling back to [`DEFAULT_MAX_RETRIES`].
#[must_use]
pub fn resolve_retry_control_with_settings(
    model: &str,
    fallback_model: Option<String>,
    is_subscriber: bool,
    env: &ResolveRetryEnv,
    settings_max_retries: Option<u32>,
) -> RetryControl {
    // TS raw `process.env.FALLBACK_FOR_ALL_PRIMARY_MODELS ||` — truthy means a
    // non-empty string; an empty string is falsy in JS.
    let fallback_for_all = env
        .fallback_for_all
        .as_deref()
        .is_some_and(|v| !v.is_empty());
    let allow_fallback =
        fallback_for_all || (!is_subscriber && crate::model::fallback::is_non_custom_opus(model));
    let is_external = env.user_type.as_deref() == Some("external");
    // TS `!!process.env.IS_SANDBOX` — present (defined) is sandboxed.
    let is_sandbox = env.is_sandbox_defined;
    let watchdog = env.retry_watchdog;
    // Precedence: env > settings > watchdog-aware default. When the
    // `LINGXI_MAX_RETRIES` env is set it wins via the `pDs()` port
    // (`resolve_max_retries`: parse, clamp `>15`→15 unless watchdog). When it is
    // absent a lingxi `routing.retry.maxAttempts` setting wins, else the
    // `pDs()` default `watchdog ? 300 : 10`.
    let max_retries = if env.max_retries.is_some() {
        resolve_max_retries(watchdog, env.max_retries.as_deref())
    } else {
        settings_max_retries.unwrap_or(if watchdog {
            WATCHDOG_MAX_RETRIES
        } else {
            DEFAULT_MAX_RETRIES
        })
    };
    RetryControl {
        fallback_model,
        primary_model: model.to_string(),
        allow_fallback,
        is_external,
        is_sandbox,
        max_retries,
        watchdog,
        ..RetryControl::default()
    }
}

/// Pure retry decision: given the current state, control policy, error, and
/// thinking budget, decide what the caller should do next.
///
/// ## Mutation contract
///
/// `next_step` mutates `state` **before** returning for budget-consuming
/// retries: `state.attempt` is incremented when a `RetryAfter` is returned
/// (so the caller's sleep + re-execute path sees the updated count on the
/// next call). `AdjustMaxTokens` does **not** consume an attempt (mirrors
/// api-client `AdjustAndRetry`). `consecutive_overloaded` is incremented for
/// `Overloaded` errors and reset to 0 for any non-overloaded error class.
///
/// ## Decision table
///
/// 1. [`LlmError::Overloaded`] — check consecutive gate first (evaluated in order,
///    first match wins):
///    - `allow_fallback && consecutive >= max_529_retries && fallback_model.is_some()`
///      → [`DriveStep::Fallback`] (does not consume an attempt).
///    - `allow_fallback && consecutive >= max_529_retries && fallback_model.is_none()
///      && is_external && !is_sandbox`
///      → [`DriveStep::Terminal`] immediately — external non-sandbox callers with
///      no fallback configured are terminated at the threshold rather than
///      exhausting the full budget (mirrors api-client `withRetry.ts:354-359`).
///    - Budget remains → [`DriveStep::RetryAfter`] (jittered, consumes an attempt,
///      increments consecutive counter).
///    - Budget exhausted → [`DriveStep::Terminal`].
/// 2. [`LlmError::RateLimited`] — server wins:
///    - `retry_after: Some(d)` → [`DriveStep::RetryAfter`] `max(d, jittered backoff)` (binary `sle` floor; consumes an attempt).
///    - `retry_after: None` → [`DriveStep::RetryAfter`] (jittered, consumes an attempt).
/// 3. [`LlmError::ProviderInternal`] / [`LlmError::Transport`] — budget check:
///    - Budget remains → [`DriveStep::RetryAfter`] (jittered, consumes an attempt).
///    - Exhausted → [`DriveStep::Terminal`].
/// 4. [`LlmError::InvalidRequest`] — overflow check:
///    - Message parses as overflow AND `adjusted_max_tokens` yields `Some(n)`
///      → [`DriveStep::AdjustMaxTokens(n)`] (does **not** consume an attempt).
///    - Message matches Anthropic thinking-signature 400 copy AND this request
///      has not already stripped → [`DriveStep::StripThinkingSignature`]
///      (does **not** consume an attempt). DeepSeek / OpenAI / Gemini 400 copy
///      does not match and stays [`DriveStep::Terminal`].
///    - Otherwise → [`DriveStep::Terminal`].
/// 5. All other errors → [`DriveStep::Terminal`].
/// 6. Budget exhaustion: once `state.attempt >= ctl.max_retries`, every
///    otherwise-retryable class → [`DriveStep::Terminal`].
///    `ctl.max_retries` defaults to [`DEFAULT_MAX_RETRIES`] and is overridden
///    by `LINGXI_MAX_RETRIES` via [`resolve_retry_control`].
pub fn next_step(
    state: &mut RetryState,
    ctl: &RetryControl,
    error: &crate::LlmError,
    thinking_budget: u32,
) -> DriveStep {
    next_step_with_backoff(state, ctl, error, thinking_budget, None)
}

/// Extended form of [`next_step`] with an optional `backoff_ms` override for
/// the exponential ladder's first rung.
///
/// When `backoff_ms` is `Some(b)`, the ladder's base is set to `b` (instead of
/// [`BASE_DELAY_MS`] = 500), preserving the binary's exponential growth +
/// [`MAX_BACKOFF_MS`] cap: `min(b * 2^attempt, 32000)`.  E.g. `backoff_ms =
/// 1000` → `[1000, 2000, 4000, 8000, 16000, 32000, …]`.  A server-sent
/// `retry_after` acts as a **floor**: the delay is `max(retry_after, jittered
/// backoff)` (binary `sle`), so a small server value never undercuts the backoff.
pub fn next_step_with_backoff(
    state: &mut RetryState,
    ctl: &RetryControl,
    error: &crate::LlmError,
    thinking_budget: u32,
    backoff_ms: Option<u64>,
) -> DriveStep {
    use crate::LlmError;

    match error {
        LlmError::Overloaded { .. } => {
            // Increment consecutive counter (always, regardless of allow_fallback,
            // so the count is accurate when allow_fallback later becomes true).
            state.consecutive_overloaded = state.consecutive_overloaded.saturating_add(1);

            // Check the fallback / external-terminal gates BEFORE budget exhaustion
            // (mirrors api-client withRetry.ts:335-359).
            if ctl.allow_fallback && state.consecutive_overloaded >= ctl.max_529_retries {
                if let Some(fallback) = &ctl.fallback_model {
                    // api-client withRetry.ts:347 — signal the caller to re-issue
                    // against the fallback model.
                    return DriveStep::Fallback {
                        fallback_model: fallback.clone(),
                    };
                }
                // api-client withRetry.ts:354-362 — external, non-sandbox, no fallback
                // configured → terminate immediately with the byte-locked
                // "Repeated 529 Overloaded errors" copy (errors.ts:166).
                if ctl.is_external && !ctl.is_sandbox {
                    return DriveStep::RepeatedOverloaded;
                }
                // Neither branch applies (internal or sandbox) → fall through to the
                // normal budget-driven retry path.
            }

            // Retry-watchdog (`oMe()`): a 529 capacity outage is EXEMPT from
            // budget exhaustion — the watchdog keeps retrying via its separate
            // counter with the ladder capped at 6h (`TLp`). Without the watchdog
            // the normal `attempt >= max_retries` terminal applies.
            if !ctl.watchdog && state.attempt >= ctl.max_retries {
                return DriveStep::Terminal;
            }

            let base = next_capacity_backoff(state, ctl, backoff_ms);
            DriveStep::RetryAfter(jittered_delay(base))
        }

        LlmError::RateLimited { retry_after, .. } => {
            // Reset the consecutive-overloaded counter: a rate-limit is not a 529.
            state.consecutive_overloaded = 0;

            // Non-first-party rate limits (e.g. an OpenRouter free-tier quota)
            // do not clear inside the retry-backoff window — surface them
            // IMMEDIATELY rather than burning the ~160s ladder on a request that
            // cannot succeed. Anthropic keeps the parity 429-retry below.
            if state.rate_limit_terminal {
                return DriveStep::Terminal;
            }

            // 429 subscriber gate (parity withRetry.ts:767):
            //   retry_429_allowed = !is_subscriber || is_enterprise
            // A plain Claude.ai subscriber (non-enterprise) must NOT retry 429s
            // — they've hit their usage limit and the server won't honour more
            // requests until the window resets. Enterprise + API-key users retry.
            if state.is_subscriber && !state.is_enterprise {
                return DriveStep::Terminal;
            }

            // A wait the SERVER named that outlasts our whole remaining ladder
            // cannot be retried into success: the loop gives up before the limit
            // clears, so every sleep in between is spent to reach the same
            // failure. Surface it now, with the provider's own message.
            //
            // Only fires on an explicit server value — a 429 with no hint keeps
            // the ladder, because burst throttling clears in seconds and is
            // exactly what the ladder is for.
            if let Some(wait) = retry_after {
                if *wait > remaining_backoff_window(state, ctl, backoff_ms) {
                    return DriveStep::Terminal;
                }
            }

            // Retry-watchdog (`oMe()`, `SLp(e)=nMe(e)||status===429`): a 429
            // capacity outage is EXEMPT from budget exhaustion, same as 529.
            if !ctl.watchdog && state.attempt >= ctl.max_retries {
                return DriveStep::Terminal;
            }

            let base = next_capacity_backoff(state, ctl, backoff_ms);
            let delay = match retry_after {
                // Binary `sle` treats the retry-after header as a FLOOR, not a
                // verbatim value: `return Math.max(header*1000, jittered_backoff)`.
                // So a small server delay never undercuts our own exponential
                // backoff during sustained rate-limiting (and a large one still wins).
                Some(d) => (*d).max(jittered_delay(base)),
                None => jittered_delay(base),
            };
            DriveStep::RetryAfter(delay)
        }

        LlmError::ProviderInternal
        | LlmError::ProviderTimeout { .. }
        | LlmError::Transport { .. }
        | LlmError::TransportTimeout { .. } => {
            // Reset the consecutive-overloaded counter.
            state.consecutive_overloaded = 0;

            if state.attempt >= ctl.max_retries {
                return DriveStep::Terminal;
            }

            let base = scaled_base_delay_ms(
                state.attempt.saturating_add(state.watchdog_capacity_waits),
                backoff_ms,
            );
            state.attempt = state.attempt.saturating_add(1);
            DriveStep::RetryAfter(jittered_delay(base))
        }

        LlmError::InvalidRequest { message } => {
            // Reset the consecutive-overloaded counter.
            state.consecutive_overloaded = 0;

            // Overflow check: parse the message and compute adjusted max_tokens.
            if let Some(overflow) = crate::model::overflow::parse_overflow_message(message) {
                if let Some(new_max) = crate::model::overflow::adjusted_max_tokens(
                    overflow,
                    u64::from(thinking_budget),
                ) {
                    // AdjustMaxTokens does NOT consume a budget attempt.
                    return DriveStep::AdjustMaxTokens(new_max);
                }
            }

            // 2.1.259 thinking-signature 400: strip once, then retry. Classifier
            // is the 400 copy, not the protocol family — OpenAI / Gemini /
            // OpenAI-compat thinking models heal the same way. DeepSeek
            // reasoning_content 400s do not match and stay terminal.
            if !state.thinking_signature_stripped
                && crate::model::thinking_signature::is_thinking_signature_rejection(message)
            {
                state.thinking_signature_stripped = true;
                return DriveStep::StripThinkingSignature;
            }

            DriveStep::Terminal
        }

        // All other LlmError variants are unconditionally terminal — including
        // SSL/cert failures (parity 2.1.201 `isSSLError` short-circuit: a cert
        // error is terminal immediately, never consuming the retry budget).
        LlmError::TlsCert { .. }
        | LlmError::Authentication { .. }
        // A rejected refresh token cannot be retried into validity; the user
        // has to sign in again.
        | LlmError::OAuthRefreshDead
        | LlmError::PermissionDenied { .. }
        | LlmError::RequestDispatchRejected { .. }
        | LlmError::ContextOverflow { .. }
        | LlmError::RequestTooLarge
        | LlmError::FileUploadOutcomeUnknown { .. }
        | LlmError::QuotaExceeded
        | LlmError::ModelUnavailable
        | LlmError::StreamInterrupted { .. }
        | LlmError::MalformedToolInput { .. }
        | LlmError::CostUnavailable { .. }
        | LlmError::UnsupportedCapability { .. }
        | LlmError::MediaDelegationUnavailable { .. }
        | LlmError::MediaDelegationPartial { .. } => {
            state.consecutive_overloaded = 0;
            DriveStep::Terminal
        }
    }
}

/// Select the (pre-jitter) base delay for attempt index `attempt`.
///
/// Byte-faithful to the v2.1.183 binary's `sle`: `min(500 * 2^(A-1), 32000)`
/// where `A` is the binary's 1-indexed attempt. LingXi's `attempt` field is
/// 0-indexed (it starts at 0 for the first retry and increments afterwards),
/// so `A - 1 == attempt` and the formula here is `min(500 * 2^attempt, 32000)`.
///
/// The `2^attempt` term is computed with saturating arithmetic so a large
/// `attempt` (which would otherwise overflow `u64`) simply clamps to the
/// [`MAX_BACKOFF_MS`] cap rather than wrapping.
pub(crate) fn base_delay_ms(attempt: u32) -> u64 {
    // 2^attempt via checked shift; any attempt >= 64 (or whose product would
    // exceed u64) saturates, then the `.min` clamps to MAX_BACKOFF_MS anyway.
    let factor = 1u64.checked_shl(attempt).unwrap_or(u64::MAX);
    BASE_DELAY_MS.saturating_mul(factor).min(MAX_BACKOFF_MS)
}

/// Select the base delay scaled by an optional custom `backoff_ms` value.
///
/// When `backoff_ms` is `None`, delegates to [`base_delay_ms`] for the
/// default exponential ladder `min(500 * 2^attempt, 32000)`.
///
/// When `backoff_ms` is `Some(b)`, the ladder's first rung is set to `b`
/// instead of [`BASE_DELAY_MS`] (= 500), keeping the same exponential growth
/// and [`MAX_BACKOFF_MS`] cap: `min(b * 2^attempt, 32000)`.  E.g.
/// `backoff_ms = 1000` → `[1000, 2000, 4000, 8000, 16000, 32000, …]`.
/// Additive jitter still applies via the caller.
///
/// Computed in `u64` arithmetic (no float) with saturating multiply so extreme
/// values clamp to the cap rather than wrapping.  Floored at 1ms as the last
/// line of defense against a zero-delay tight retry loop (settings parsing
/// already rejects `backoffMs = 0`).
#[must_use]
pub fn scaled_base_delay_ms(attempt: u32, backoff_ms: Option<u64>) -> u64 {
    match backoff_ms {
        None => base_delay_ms(attempt),
        Some(b) => {
            let factor = 1u64.checked_shl(attempt).unwrap_or(u64::MAX);
            b.saturating_mul(factor).min(MAX_BACKOFF_MS).max(1)
        }
    }
}

/// Base (pre-jitter) delay for a CAPACITY error (529/429) attempt.
///
/// Without the watchdog this is exactly [`scaled_base_delay_ms`] (the normal
/// ladder capped at [`MAX_BACKOFF_MS`] = 32s). With the watchdog ON the same
/// exponential growth is re-capped at [`WATCHDOG_MAX_BACKOFF_MS`] (6h, binary
/// `TLp`), so a sustained outage backs off far longer while the watchdog keeps
/// retrying past the normal budget. Saturating arithmetic clamps extreme
/// `attempt` values to the cap rather than wrapping.
#[must_use]
pub fn capacity_base_delay_ms(attempt: u32, backoff_ms: Option<u64>, watchdog: bool) -> u64 {
    if !watchdog {
        return scaled_base_delay_ms(attempt, backoff_ms);
    }
    let base = backoff_ms.unwrap_or(BASE_DELAY_MS);
    let factor = 1u64.checked_shl(attempt).unwrap_or(u64::MAX);
    base.saturating_mul(factor)
        .min(WATCHDOG_MAX_BACKOFF_MS)
        .max(1)
}

/// Spend the appropriate counter only after the capacity error passed its
/// terminal/fallback gates. Watchdog waits never consume ordinary retries.
fn next_capacity_backoff(
    state: &mut RetryState,
    ctl: &RetryControl,
    backoff_ms: Option<u64>,
) -> u64 {
    let counter = if ctl.watchdog {
        &mut state.watchdog_capacity_waits
    } else {
        &mut state.attempt
    };
    let base = capacity_base_delay_ms(*counter, backoff_ms, ctl.watchdog);
    *counter = counter.saturating_add(1);
    base
}

/// Total time the retry ladder would still spend sleeping, from `state.attempt`
/// to the budget's end.
///
/// The comparison point for a server-named rate-limit wait: a `Retry-After`
/// longer than this cannot be retried into success, because the loop gives up
/// before the limit clears.
///
/// The watchdog is excluded on purpose — its ladder runs to 6h, so under it
/// there is effectively always more window and the caller should keep waiting.
#[must_use]
pub fn remaining_backoff_window(
    state: &RetryState,
    ctl: &RetryControl,
    backoff_ms: Option<u64>,
) -> std::time::Duration {
    if ctl.watchdog {
        return std::time::Duration::MAX;
    }
    let total: u64 = (state.attempt..ctl.max_retries)
        .map(|attempt| capacity_base_delay_ms(attempt, backoff_ms, false))
        .sum();
    std::time::Duration::from_millis(total)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "retry_test.rs"]
mod retry_test;
