//! API-success telemetry emission. The per-request response-completed success
//! path fires `tengu_api_success` with the field subset claude-code 2.1.195
//! emits on its main-query success path.
//!
//! # Parity
//!
//! claude-code 2.1.195 fires `tengu_api_success` on every completed API
//! response (`j("tengu_api_success", {...})`). The port-only
//! `tengu_cost_recorded` event (0 hits in 2.1.195) was dropped under strict
//! parity; cost ACCOUNTING is untouched — only the telemetry emit changed.
//!
//! ## Field set
//!
//! claude's main-path payload is ~40 fields. The bulk are conditional-spread
//! (`...x&&{...}`) or `??void 0` (omitted when absent) — those stay omitted
//! here because the port has no source for them, which is byte-faithful (claude
//! also omits them when their input is absent). But claude's main-query path
//! ALWAYS emits a ~22-key UNCONDITIONAL minimum (verified `od` at binary byte
//! 212532751 — bare `key:value`, NOT `...&&` / `??void 0`). This emitter now
//! supplies that full unconditional set. The inserted keys (claude camelCase
//! names):
//!
//! - `model` — resolved model id (`model:e`).
//! - `messageCount` — count of messages in the call's input snapshot (`messageCount:n`).
//! - `messageTokens` — pre-call ESTIMATE of the input snapshot's tokens
//!   (`messageTokens:r`); claude's `r` binding is the estimate, distinct from
//!   `inputTokens` which is the API's real count.
//! - `inputTokens` / `outputTokens` — Anthropic `usage.{input,output}_tokens`.
//! - `cachedInputTokens` / `uncachedInputTokens` — prompt-caching counters
//!   (`cache_read_input_tokens??0` / `cache_creation_input_tokens??0`).
//! - `durationMs` — this call's wall-clock (`durationMs:s`).
//! - `durationMsIncludingRetries` — elapsed across retries (`durationMsIncludingRetries:i`).
//! - `attempt` — 1-based attempt count = `retries + 1` (`attempt:a`).
//! - `ttftMs` — time-to-first-token; OMITTED when unknown (`ttftMs:l??void 0`).
//! - `buildAgeMins` — minutes since the build timestamp (`buildAgeMins:THl()`).
//! - `costUSD` — **dollars float** (`costUSD:p`). The port stores nano-USD
//!   `u64`; this emitter divides by `1e9` to match claude's dollar shape.
//!   This is the one value-shape conversion vs the port's internal storage.
//! - `provider` — resolved provider tag (`provider:y9()`).
//! - `requestId` — `last_request_id()`; OMITTED when `None` (`??void 0`).
//! - `stop_reason` — `response.stop_reason`; OMITTED when `None` (`??void 0`).
//! - `didFallBackToNonStreaming` — `true` on the 529 non-streaming fallback arm,
//!   `false` on the plain streaming/batched success path (`didFallBackToNonStreaming:m`).
//! - `isNonInteractiveSession` — `xr()` (`isNonInteractiveSession:L`).
//! - `print` — the `-p`/`--print` flag (`print:B`). Distinct from
//!   `isNonInteractiveSession` (SDK/transport is non-interactive but not print).
//! - `isTTY` — `process.stdout.isTTY??!1` (`isTTY:...`). Hosts thread the real
//!   stdout TTY; SDK / tests default `false`.
//! - `querySource` — Claude Code 2.1.245 main-query allowlist
//!   (`repl_main_thread` / `sdk`, sanitized via `E_`).
//! - `permissionMode` — `"plan"` when plan-mode else `"default"` (`permissionMode:No(T)`).
//!   The port `PermissionGate` exposes only plan/default, not acceptEdits/bypass.
//!
//! The port sink stores metadata in a `HashMap`, so wire field ORDER is not
//! byte-locked — only the field-name SET + value types/values are observable.
//! Insertion order below follows claude's object literal for readability only.

#![forbid(unsafe_code)]

use std::sync::Arc;
use telemetry::{AnalyticsBus, AnalyticsValue, LogEventMetadata, Verified};

/// Event name — preserved-identifier `tengu_*` carve-out; matches claude-code
/// 2.1.195 verbatim (7 hits in the 2.1.195 binary strings).
pub const EVENT_NAME_API_SUCCESS: &str = "tengu_api_success";

/// Inputs for one `tengu_api_success` emission.
///
/// Assembled by the orchestrator success path (request id / stop reason live
/// there, not in the cost crate) and passed to [`emit_api_success`].
#[derive(Debug, Clone)]
pub struct ApiSuccessFields {
    /// Resolved model id (`model:e`).
    pub model: String,
    /// Anthropic `usage.input_tokens`.
    pub input_tokens: u64,
    /// Anthropic `usage.output_tokens`.
    pub output_tokens: u64,
    /// Prompt-caching: tokens read from cache (`cache_read_input_tokens??0`).
    pub cached_input_tokens: u64,
    /// Prompt-caching: tokens written to a new cache block (`cache_creation_input_tokens??0`).
    pub uncached_input_tokens: u64,
    /// This call's wall-clock in ms (`durationMs:s`).
    pub duration_ms: u64,
    /// Elapsed across retries in ms (`durationMsIncludingRetries:i`).
    pub duration_ms_including_retries: u64,
    /// 1-based attempt count = `retries + 1` (`attempt:a`).
    pub attempt: u32,
    /// Recorded cost for this single call, in nano-USD. Emitted as dollars float.
    pub cost_nano_usd: u64,
    /// Resolved provider tag (`provider:y9()`).
    pub provider: String,
    /// `response.stop_reason`; OMITTED when `None` (`stop_reason:...??void 0`).
    pub stop_reason: Option<String>,
    /// Most recent `request-id` header; OMITTED when `None` (`requestId:...??void 0`).
    pub request_id: Option<String>,
    /// Count of messages in the call's input snapshot (`messageCount:n`).
    pub message_count: u32,
    /// Pre-call ESTIMATE of the input snapshot's tokens (`messageTokens:r`).
    pub message_tokens: u64,
    /// `true` on the 529 non-streaming fallback arm, else `false`
    /// (`didFallBackToNonStreaming:m`).
    pub did_fall_back_to_non_streaming: bool,
    /// `xr()` non-interactive-session flag (`isNonInteractiveSession:L`).
    pub is_non_interactive_session: bool,
    /// `-p`/`--print` flag (`print:B`). Distinct from
    /// [`Self::is_non_interactive_session`].
    pub print: bool,
    /// `process.stdout.isTTY??!1` (`isTTY:...`). Host-supplied; default `false`.
    pub is_tty: bool,
    /// Main-query source (`querySource:hl(m)` / `E_(querySource)`). CLI is
    /// `"repl_main_thread"`; SDK/bridge is `"sdk"`.
    pub query_source: String,
    /// `"plan"` when plan-mode else `"default"` (`permissionMode:No(T)`).
    pub permission_mode: String,
    /// Time-to-first-token in ms; OMITTED when `None` (`ttftMs:l??void 0`).
    pub ttft_ms: Option<u64>,
    /// `/fast`-mode flag for this query (`fastMode:H`) — a BARE UNCONDITIONAL
    /// key in claude's object; the port derives it from the request/response
    /// `speed == "fast"` tier.
    pub fast_mode: bool,
    /// Ms since the previous API call's timestamp; OMITTED only on the very
    /// first call (`timeSinceLastApiCallMs:W`,
    /// `W=G!==null?Math.max(0,Math.round(M-G)):void 0`).
    pub time_since_last_api_call_ms: Option<u64>,
}

/// Minutes since the build timestamp baked in by `cost/build.rs`, the faithful
/// analog of claude-code 2.1.195's `THl()` (`buildAgeMins:THl()`).
///
/// `THl()` returns `undefined` when the build time is missing/unparseable; this
/// returns `None` in the matching cases so `buildAgeMins` is OMITTED from the
/// payload (no placeholder), byte-faithful with claude. `option_env!` resolves
/// at COMPILE time of THIS crate (`cost`), so the value reflects cost's own
/// build epoch regardless of which caller invokes the emitter.
#[must_use]
pub fn build_age_mins() -> Option<i64> {
    let epoch_secs: i64 = option_env!("LINGXI_COST_BUILD_EPOCH_SECS")?.parse().ok()?;
    let now = chrono::Utc::now().timestamp();
    Some((now - epoch_secs) / 60)
}

/// Emit a `tengu_api_success` event for one completed API call.
///
/// Field-name set + value types follow claude-code 2.1.195's main-query
/// success path. `??void 0` fields (`stop_reason`, `requestId`) are OMITTED
/// when `None` (no `AnalyticsValue::None` placeholder), matching claude.
pub async fn emit_api_success(bus: &Arc<AnalyticsBus>, f: &ApiSuccessFields) {
    let mut metadata = LogEventMetadata::new();
    metadata.insert(
        "model".into(),
        AnalyticsValue::String(Verified::assert_safe(f.model.clone()).into_inner()),
    );
    metadata.insert(
        "inputTokens".into(),
        AnalyticsValue::Int(i64_from_u64_saturating(f.input_tokens)),
    );
    metadata.insert(
        "outputTokens".into(),
        AnalyticsValue::Int(i64_from_u64_saturating(f.output_tokens)),
    );
    metadata.insert(
        "cachedInputTokens".into(),
        AnalyticsValue::Int(i64_from_u64_saturating(f.cached_input_tokens)),
    );
    metadata.insert(
        "uncachedInputTokens".into(),
        AnalyticsValue::Int(i64_from_u64_saturating(f.uncached_input_tokens)),
    );
    metadata.insert(
        "durationMs".into(),
        AnalyticsValue::Int(i64_from_u64_saturating(f.duration_ms)),
    );
    metadata.insert(
        "durationMsIncludingRetries".into(),
        AnalyticsValue::Int(i64_from_u64_saturating(f.duration_ms_including_retries)),
    );
    metadata.insert("attempt".into(), AnalyticsValue::Int(i64::from(f.attempt)));
    // claude `costUSD:p` is DOLLARS as a float; the port stores nano-USD u64,
    // so divide by 1e9 to match the dollar value-shape (the one conversion).
    #[allow(clippy::cast_precision_loss)]
    metadata.insert(
        "costUSD".into(),
        AnalyticsValue::Float(f.cost_nano_usd as f64 / 1e9),
    );
    metadata.insert(
        "provider".into(),
        AnalyticsValue::String(Verified::assert_safe(f.provider.clone()).into_inner()),
    );
    // `??void 0` → field OMITTED when absent (no None placeholder).
    if let Some(sr) = &f.stop_reason {
        metadata.insert(
            "stop_reason".into(),
            AnalyticsValue::String(Verified::assert_safe(sr.clone()).into_inner()),
        );
    }
    if let Some(rid) = &f.request_id {
        metadata.insert(
            "requestId".into(),
            AnalyticsValue::String(Verified::assert_safe(rid.clone()).into_inner()),
        );
    }
    metadata.insert(
        "messageCount".into(),
        AnalyticsValue::Int(i64::from(f.message_count)),
    );
    metadata.insert(
        "messageTokens".into(),
        AnalyticsValue::Int(i64_from_u64_saturating(f.message_tokens)),
    );
    metadata.insert(
        "didFallBackToNonStreaming".into(),
        AnalyticsValue::Bool(f.did_fall_back_to_non_streaming),
    );
    metadata.insert(
        "isNonInteractiveSession".into(),
        AnalyticsValue::Bool(f.is_non_interactive_session),
    );
    metadata.insert("print".into(), AnalyticsValue::Bool(f.print));
    metadata.insert("isTTY".into(), AnalyticsValue::Bool(f.is_tty));
    metadata.insert(
        "querySource".into(),
        AnalyticsValue::String(Verified::assert_safe(f.query_source.clone()).into_inner()),
    );
    metadata.insert(
        "permissionMode".into(),
        AnalyticsValue::String(Verified::assert_safe(f.permission_mode.clone()).into_inner()),
    );
    // `buildAgeMins:THl()` — OMITTED when the build epoch is missing/unparseable.
    if let Some(mins) = build_age_mins() {
        metadata.insert("buildAgeMins".into(), AnalyticsValue::Int(mins));
    }
    // `ttftMs:l??void 0` — OMITTED when unknown.
    if let Some(t) = f.ttft_ms {
        metadata.insert(
            "ttftMs".into(),
            AnalyticsValue::Int(i64_from_u64_saturating(t)),
        );
    }
    // `fastMode:H` — bare unconditional bool, always serialized.
    metadata.insert("fastMode".into(), AnalyticsValue::Bool(f.fast_mode));
    // `timeSinceLastApiCallMs:W` — OMITTED on the very first call (no prior).
    if let Some(ms) = f.time_since_last_api_call_ms {
        metadata.insert(
            "timeSinceLastApiCallMs".into(),
            AnalyticsValue::Int(i64_from_u64_saturating(ms)),
        );
    }

    bus.log_event(EVENT_NAME_API_SUCCESS, metadata).await;
}

/// Saturate a `u64` into the `i64` value range used by `AnalyticsValue::Int`.
///
/// In practice token counts and nano-USD totals never exceed `i64::MAX`
/// (~9.2e18), but the saturation guards against the pathological `u64::MAX`
/// case so `as i64` would not produce a confusing negative number.
#[inline]
#[allow(clippy::cast_possible_wrap)]
const fn i64_from_u64_saturating(v: u64) -> i64 {
    if v > i64::MAX as u64 {
        i64::MAX
    } else {
        v as i64
    }
}
