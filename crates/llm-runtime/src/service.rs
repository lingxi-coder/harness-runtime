//! Provider-neutral API service: drive the LLM client with full
//! retry/rate-limit/betas.
//!
//! `ApiService` owns the inherent drive loop relocated from the orchestrator's
//! `ProviderApiAdapter` (`orchestrator/src/provider_adapter.rs`). It speaks
//! `protocol::ConversationMessage` + llm-runtime types and references only
//! `crate::*` + `protocol`/`traits`/`telemetry` — never any orchestrator-internal
//! path. The orchestrator's consumer-trait impls delegate to it 1:1.

use crate::agent_cache_ttl_1h_override;
use crate::convert::{
    ensure_tool_result_pairing, normalize_messages_for_api_with_tool_search, to_llm_messages,
    to_tool_declarations,
};
use crate::model::betas::{
    apply_beta_header_with_auth_and_custom, bedrock_extra_body_betas, BetaContext, Endpoint,
    Provider, FAST_MODE,
};
use crate::model::rate_limit::{
    formatted_reset_times_from_headers, parse_retry_after, parse_unified_reset,
    rate_limit_error_message, RateLimitInfo, RawUtilization, SubscriptionContext,
};
use crate::model::retry::{
    next_step_with_backoff, resolve_retry_control_with_settings, DriveStep, ResolveRetryEnv,
    RetryControl, RetryState,
};
use crate::model::telemetry;
use crate::model::user_agent::{user_agent, UserAgentEnv};
use crate::{
    CacheControl, CostEstimator, DefaultLlmClient, LlmError, LlmEvent, LlmRequest, LlmResponse,
    MediaRoute, ResponsesWebSocketSession, StreamDecoder, Transport,
};
use futures::stream::BoxStream;
use protocol::{is_nested_media_value, ContentBlock, ConversationMessage};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// Mirror claude-code `getPromptCachingEnabled` (services/api/claude.ts:333).
///
/// Prompt caching is on by default; `DISABLE_PROMPT_CACHING` turns it off
/// globally, and the per-family `DISABLE_PROMPT_CACHING_{HAIKU,SONNET,OPUS}`
/// vars turn it off for a matching model. Truthiness follows TS `isEnvTruthy`
/// (utils/envUtils.ts): only `1`/`true`/`yes`/`on` (case-insensitive) count.
///
/// PARITY-NOTE: TS compares `model` for exact equality with the *configured*
/// small-fast / default-sonnet / default-opus IDs; here we match the family by
/// substring, a close (slightly more lenient) approximation.
/// Shared truthy-env reader (`1`/`true`/`yes`/`on`, case/space-insensitive) —
/// used by the prompt-cache gates.
fn cache_env_truthy(name: &str) -> bool {
    std::env::var(name).ok().is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn prompt_caching_enabled(model: &str) -> bool {
    fn env_truthy(name: &str) -> bool {
        cache_env_truthy(name)
    }
    if env_truthy("DISABLE_PROMPT_CACHING") {
        return false;
    }
    let m = model.to_ascii_lowercase();
    if m.contains("haiku") && env_truthy("DISABLE_PROMPT_CACHING_HAIKU") {
        return false;
    }
    if m.contains("sonnet") && env_truthy("DISABLE_PROMPT_CACHING_SONNET") {
        return false;
    }
    if m.contains("opus") && env_truthy("DISABLE_PROMPT_CACHING_OPUS") {
        return false;
    }
    true
}

/// Collapse a [`ReasoningConfig`] to a numeric budget for telemetry labels.
/// `Adaptive` → 0, `Enabled{b}` → b.
fn reasoning_budget(reasoning: Option<crate::ReasoningConfig>) -> u32 {
    match reasoning {
        Some(crate::ReasoningConfig::Enabled { budget_tokens }) => budget_tokens,
        Some(crate::ReasoningConfig::Adaptive) | None => 0,
    }
}

/// Remove provider-authenticated assistant blocks before retrying on a different
/// model. Their signatures are scoped to the model that produced them and must
/// never be replayed to a fallback provider/model.
fn strip_signature_blocks(messages: &mut [crate::Message]) {
    for message in messages
        .iter_mut()
        .filter(|message| message.role == "assistant")
    {
        message.content.retain(|block| {
            !matches!(
                block,
                crate::ContentBlock::Reasoning { .. }
                    | crate::ContentBlock::RedactedThinking { .. }
                    | crate::ContentBlock::ConnectorText { .. }
            )
        });
    }
}

/// Claude Code gates cross-model signature stripping on its internal account
/// class `T2o()`. In the shipped 2.1.218 binary that predicate is a compile-time
/// constant `"external"` (there is NO `process.env.USER_TYPE` read in the CLI
/// bundle for this) — so the strip never runs in the released build. Modeled
/// here as the same inert constant rather than reading `USER_TYPE`, which the
/// oracle does not do for this path. The pure [`strip_signature_blocks`]
/// transform stays separately tested and ready for when the internal account
/// class is actually plumbed.
/// Re-point `req` at the next CONNECTION of the same provider group.
///
/// Returns the connection's profile name when the request was moved and the
/// caller should retry it, `None` when this error does not trigger failover or
/// the group is exhausted.
///
/// This is NOT model fallback. The model is identical — only the endpoint and
/// credential change — so thinking signatures stay valid, nothing is stripped,
/// and no `tengu_model_fallback_triggered` is emitted: from the caller's point
/// of view the same model simply answered.
///
/// The retry budget is reset per connection: attempts burned against an
/// endpoint that is rate-limited or down say nothing about the next one.
fn advance_connection(
    req: &mut crate::LlmRequest,
    state: &mut RetryState,
    chain: &[crate::ConnectionHop],
    index: &mut usize,
    triggers: crate::FailoverTriggers,
    error: &LlmError,
) -> Option<String> {
    if !triggers.matches(error) {
        return None;
    }
    let hop = chain.get(*index)?;
    *index += 1;
    req.profile = Some(hop.profile_name.clone());
    req.model.clone_from(&hop.request_model);
    state.attempt = 0;
    state.consecutive_overloaded = 0;
    Some(hop.profile_name.clone())
}

fn strip_signature_blocks_for_fallback(messages: &mut [crate::Message]) {
    if is_internal_account_class() {
        strip_signature_blocks(messages);
    }
}

/// claude-code `T2o()` — the account class, a compile-time `"external"` constant
/// in the shipped binary. `false` until an internal account class is plumbed.
fn is_internal_account_class() -> bool {
    false
}

/// Bound `max_tokens` so `input_tokens + output` fit `context_window`: reserve
/// the estimated input plus provider-formatting headroom. Fixes models whose
/// advertised max-output equals their context window (models.dev has no distinct
/// output cap — 64 OpenRouter models + gpt-4) from requesting the ENTIRE window
/// as output, which the endpoint rejects once any input is present. Never raises
/// `max_tokens`; leaves it unchanged when it already fits.
fn bound_output_to_context(max_tokens: u32, context_window: u64, input_tokens: u64) -> u32 {
    /// Keep bounded headroom unused because OpenAI-compatible routers may add
    /// model-specific chat/tool templates.
    const OUTPUT_FIT_MARGIN_MIN: u64 = 1_024;
    const OUTPUT_FIT_MARGIN_MAX: u64 = 20_000;
    let raw_fit = context_window.saturating_sub(input_tokens);
    let margin = (context_window / 20).clamp(OUTPUT_FIT_MARGIN_MIN, OUTPUT_FIT_MARGIN_MAX);
    let conservative_fit = raw_fit.saturating_sub(margin);
    // If only the safety margin (rather than the actual input) consumes the
    // remaining window, allow one token so the wire request remains valid.
    let fit = if conservative_fit == 0 {
        raw_fit.min(1)
    } else {
        conservative_fit
    };
    max_tokens.min(u32::try_from(fit).unwrap_or(u32::MAX))
}

/// Allow one overflow-driven output reduction per request drive, and only when
/// it strictly lowers the value already sent on the wire.
fn guard_max_tokens_adjustment(
    step: DriveStep,
    current_max_tokens: Option<u32>,
    already_adjusted: bool,
) -> DriveStep {
    match step {
        DriveStep::AdjustMaxTokens(proposed)
            if already_adjusted
                || !current_max_tokens.is_some_and(|current| proposed < current) =>
        {
            DriveStep::Terminal
        }
        other => other,
    }
}

// ── Subscriber state ─────────────────────────────────────────────────────────

/// Subscription flags — gates the 429 retry policy.
///
/// Task 8 wires real values from auth; default is both false (conservative:
/// over-retries 429s slightly, but never breaks).
#[derive(Debug, Clone, Copy, Default)]
pub struct SubscriberState {
    /// `true` when the configured credential is a Claude.ai OAuth subscriber.
    pub is_subscriber: bool,
    /// `true` when the subscriber is an enterprise account.
    pub is_enterprise: bool,
}

// ── Stream state (used in drive_stream unfold) ────────────────────────────────

/// State threaded through the `futures::stream::unfold` loop in `drive_stream`.
struct StreamState {
    attempt: crate::model_attempt::WireAttempt,
    decoder: crate::upstream::Decoder,
    frames: lingxi_llm_client::ModelStream,
    pricing: Option<lingxi_llm_client::FrozenPricing>,
    pricing_model: crate::PricingModelRef,
    /// First frame already pulled by the drive loop's dispatch body-phase
    /// lookahead (see [`ApiService::note_dispatch_body_phase_failure`]).
    /// Consumed in place of the first `next_frame()` so decoding, watchdog and
    /// error handling stay identical to the un-seeded path. `None` on every
    /// stream that did not carry `anthropic-dispatch-id`.
    seed: Option<Result<Option<lingxi_llm_client::StreamBatch>, LlmError>>,
    queue: VecDeque<LlmEvent>,
    finished: bool,
    /// Guard against double-emit: once we have fired succeed/fail we never fire again.
    done: bool,
    /// Optional analytics bus for stream telemetry twins (`emit_succeeded` / `emit_failed`).
    analytics: Option<Arc<::telemetry::AnalyticsBus>>,
    /// Request model string for telemetry labels.
    model: String,
    /// Client-side request id for telemetry correlation.
    request_id: String,
    /// Wall-clock start of the stream for `duration_ms`.
    started: Instant,
    /// Streaming idle watchdog (cc 2.1.196 default-on). `Some(timeout)` when
    /// the watchdog is enabled: each blocking frame read is bounded by this
    /// duration and, on elapse, the stream yields a watchdog
    /// [`LlmError::StreamInterrupted`] (detectable via
    /// [`crate::model::stream_watchdog::is_stream_idle_timeout`]). `None`
    /// disables it (`LINGXI_ENABLE_STREAM_WATCHDOG=0`). The deadline resets on
    /// every received event because a fresh timeout wraps each frame fetch.
    idle_timeout: Option<Duration>,
}

fn frozen_stream_quote(
    pricing: Option<&lingxi_llm_client::FrozenPricing>,
    pricing_model: &crate::PricingModelRef,
    usage: &lingxi_llm_client::protocol::UsageReport,
    inference: &lingxi_llm_client::protocol::InferenceReport,
) -> Option<crate::CostEstimate> {
    let estimate = pricing?
        .estimate(
            usage,
            inference,
            lingxi_llm_client::protocol::Submission::default(),
        )
        .ok()?;
    crate::cost::project_estimate(estimate, pricing_model.clone()).ok()
}

fn attach_frozen_stream_quote(events: &mut [LlmEvent], quote: Option<&crate::CostEstimate>) {
    for event in events {
        if let LlmEvent::MessageDelta {
            usage: Some(usage), ..
        } = event
        {
            usage.cost_estimate = quote.cloned();
        }
    }
}

// ── Adapter state ─────────────────────────────────────────────────────────────

/// Origin of the request id recorded by [`ApiService::last_request_id`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestIdOrigin {
    /// The provider returned its own id in a known response header
    /// (authoritative — valid for provider-side log/support lookups).
    Server,
    /// No server id header was present, so the client-generated `x-request-id`
    /// we sent is used as a fallback. Correlation-only: a provider will NOT find
    /// this id in its logs.
    Client,
}

/// Whether `model` is an OpenRouter FREE-tier variant (`…:free`), whose 429 is a
/// shared-quota exhaustion that does NOT clear within the retry-backoff window —
/// so it fails fast (surfaces the rate limit immediately) instead of burning the
/// ~160s ladder. PAID models (incl. the user's main provider) and Anthropic keep
/// Claude Code's parity 429-retry. `:free` is OpenRouter's free-variant suffix
/// and is unused by other providers, so it targets exactly the flaky free tier.
fn is_free_tier_model(model: &str) -> bool {
    model.ends_with(":free")
}

/// Profiles whose credential is a SUBSCRIPTION rather than an API key.
///
/// The distinction is the whole point: an API key's 429 is burst throttling and
/// clears in seconds, while a plan's quota resets on the plan's own clock —
/// minutes to hours. Retrying the second kind spends the entire ladder to reach
/// the same failure, which is what made a rate-limited ChatGPT-login turn sit on
/// "Thinking…" for minutes before reporting "api call failed: rate limited".
///
/// Anthropic's Claude.ai subscription is NOT listed: it is already covered by
/// the parity subscriber gate (`RetryState::is_subscriber`, fed from the live
/// subscription snapshot). That gate speaks Claude.ai's vocabulary
/// (`subscription_type == "enterprise"`), so an OpenAI plan can never set it —
/// which is exactly why the ChatGPT profile has to be named here.
fn is_subscription_profile(profile: Option<&str>) -> bool {
    matches!(profile, Some("openai-chatgpt"))
}

/// Whether a 429 on this route is known not to clear inside the retry-backoff
/// window, and so must surface immediately rather than burn the ~160s ladder.
///
/// Both arms are the same criterion — a quota that resets on someone else's
/// clock — reached by the two identities we can see before the error arrives.
/// A server-named `Retry-After` that outlasts the window is handled separately,
/// per-error, in `next_step_with_backoff`.
fn rate_limit_cannot_clear(profile: Option<&str>, model: &str) -> bool {
    is_free_tier_model(model) || is_subscription_profile(profile)
}

fn openrouter_free_rate_limit_message(body: Option<&serde_json::Value>) -> String {
    let detail = body
        .and_then(|body| {
            body.get("error")
                .and_then(|error| error.get("message"))
                .or_else(|| body.get("message"))
        })
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .map(|message| message.trim_end_matches(['.', '!', '?']));

    match detail {
        Some(detail) => format!(
            "OpenRouter free-model rate limit reached: {detail}. Try another free model or retry later."
        ),
        None => "OpenRouter free-model rate limit reached. Try another free model or retry later."
            .to_string(),
    }
}

const NEAR_LIMIT_WRAP_UP_DEFAULT_THRESHOLD: f64 = 0.95;
const NEAR_LIMIT_WRAP_UP_MAX5X_THRESHOLD: f64 = 0.99;
const NEAR_LIMIT_WRAP_UP_MAX20X_THRESHOLD: f64 = 0.9975;

/// A retry-worthy API failure the [`ApiService`] retry loop is about to back off
/// on, surfaced to the UI so it can show a Claude-Code-style
/// "Retrying in Ns… (attempt X/Y)" status during the wait (mirrors
/// `SystemAPIErrorMessage.tsx`). Emitted once per backoff, before sleeping.
#[derive(Debug, Clone)]
pub struct RetryInfo {
    /// The user-facing error text (e.g. `"provider internal error"`).
    pub message: String,
    /// 1-based attempt number about to be retried.
    pub attempt: u32,
    /// The configured retry cap (`DEFAULT_MAX_RETRIES` = 10 unless overridden).
    pub max_retries: u32,
    /// Backoff before the next attempt, in milliseconds (the countdown seed).
    pub delay_ms: u64,
}

/// Sink for retry-status updates emitted by the [`ApiService`] retry loop. The
/// composition root wires this to the UI output stream so the TUI can render
/// the retry/backoff status during an otherwise-silent backoff. `report` is
/// synchronous (fire-and-forget); the wiring bridges to the async UI channel.
pub trait RetryReporter: Send + Sync {
    /// Called once per backoff, immediately before the retry sleep.
    fn report(&self, info: RetryInfo);
}

/// Production service: drives `DefaultLlmClient` with full retry/rate-limit/betas.
pub struct ApiService {
    model_attempt_hooks: RwLock<Option<Arc<dyn crate::ModelAttemptHooks>>>,
    client: Arc<DefaultLlmClient>,
    transport: Arc<dyn Transport>,
    /// Subscriber state for the 429 gate (Task 8 wires real value).
    ///
    /// Build-time seed/fallback: when [`Self::subscription`] is attached and
    /// resolved, [`Self::effective_subscriber`] prefers the live snapshot.
    subscriber: SubscriberState,
    /// Live shared subscription slot (batch-5 Task 3). Filled asynchronously
    /// by the composition root's background profile/roles fetch (batch 4);
    /// `None` when the host has no OAuth profile fetch (mobile) or predates
    /// the wiring. Read via [`Self::effective_subscriber`].
    subscription: Option<platform_api::subscription::SharedSubscription>,
    /// Forced `tool_choice` for every request this adapter drives, set by
    /// [`Self::with_forced_tool_choice`]. Used by `--json-schema` structured
    /// output to COMPEL the `StructuredOutput` tool (1:1 with claude-code forcing
    /// `tool_choice` to that tool). `None` (the default for every normal turn)
    /// leaves the request's `tool_choice` unset so the model chooses freely.
    forced_tool_choice: Option<crate::ToolChoice>,
    /// Session thinking configuration (claude-code `thinking` intent).
    ///
    /// Default [`ThinkingConfig::Adaptive`] — claude-code sends adaptive thinking
    /// by default for adaptive-capable models. `build_request` resolves this
    /// against the model's thinking predicates + the `CLAUDE_CODE_DISABLE_*`
    /// env gates to produce the `reasoning` field and the coupled `temperature`.
    /// Set via [`Self::with_thinking`].
    thinking: RwLock<crate::model::thinking::ThinkingConfig>,
    /// Identity for the Anthropic `metadata.user_id` field (claude-code
    /// `claude.ts:503-525`). `None` (the default) omits `metadata` entirely.
    /// Set via [`Self::with_request_metadata`]; the composition root supplies
    /// the composed identity string.
    request_metadata: Option<crate::RequestMetadata>,
    /// 1P experimental cache-editing inputs (claude.ts `addCacheBreakpoints`
    /// `newCacheEdits`/`pinnedEdits`, claude.ts:3068-3069). LingXi has no
    /// cached-microcompact scheduler to produce these, so the default is
    /// `None`/empty — the gate-armed `cache_reference`-on-tool_results pass
    /// (the directly-exercised behavior) still runs from `req.messages`. Set
    /// only by [`Self::with_cache_editing_inputs`] (test-only today); wiring a
    /// real producer is residual. See module note.
    cache_editing_inputs: CacheEditingInputs,
    /// User-agent environment snapshot (Task 3).
    ua: UserAgentEnv,
    /// Build version string for the User-Agent header.
    version: String,
    /// Optional analytics bus for telemetry events.
    analytics: Option<Arc<::telemetry::AnalyticsBus>>,
    /// Optional UI retry-status sink. Set via [`Self::with_retry_reporter`];
    /// `None` (the default) makes the retry loop silent as before. When set, the
    /// loop reports each backoff so the TUI can show "Retrying in Ns… (attempt
    /// X/Y)".
    retry_reporter: Option<Arc<dyn RetryReporter>>,
    /// Ordered global fallback chain. A scalar legacy value becomes one entry;
    /// the CLI's comma-separated form is normalized into this vector once at
    /// construction so every new user turn starts from the primary model and
    /// walks the same immutable order.
    fallback_models: Vec<String>,
    /// Host-validated CLI beta additions for first-party Anthropic API-key
    /// message requests. Kept as explicit session state instead of a process
    /// environment variable so concurrent embedded runtimes cannot leak flags
    /// into one another.
    custom_cli_betas: Vec<String>,
    /// Session-local interactivity for request beta assembly. Keeping this on
    /// the service prevents concurrently embedded mobile runtimes (foreground
    /// chat plus scheduled headless work) from overwriting one process-global
    /// flag. Defaults to the legacy global when no host supplies a value.
    interactive_session: Option<bool>,
    /// Per-model fallback chains from `routing.fallback`.
    ///
    /// Key is the request's resolved display model; value is the ordered chain
    /// of fallback target display models.  A per-model entry **wins** over
    /// `fallback_model` (global).  The adapter walks the chain in order on
    /// consecutive overload events: chain[0] fires first, chain[1] next, etc.
    fallback_overrides: std::collections::BTreeMap<String, Vec<String>>,
    /// Alias → display-model map built at construction from
    /// `client.available_models()`. Used by `messages_create_with_fallback`
    /// to normalize an alias request string to the display model before
    /// probing `fallback_overrides` (whose keys are display-normalized at
    /// parse time).
    alias_to_display: std::collections::BTreeMap<String, String>,
    /// `routing.retry.maxAttempts` override.
    ///
    /// Precedence: `LINGXI_MAX_RETRIES` env > this > `DEFAULT_MAX_RETRIES`.
    settings_max_retries: Option<u32>,
    /// `routing.retry.backoffMs` override.
    ///
    /// When `Some(b)`, the jitter ladder's first rung is `b` ms (default 500).
    /// Subsequent rungs are scaled proportionally (`DEFAULT[i] * b/500`).
    /// Jitter ±20% still applies.
    settings_backoff_ms: Option<u64>,
    /// Available model ids from the client registry (for `available_models`).
    available_model_ids: Vec<String>,
    /// Full provider-specific model listings for rich picker surfaces.
    model_listings: Vec<crate::ModelListing>,
    /// Optional cost estimator for populating `LlmResponse.cost`.
    ///
    /// When `Some`, a successful `decode_response` triggers a cost estimate using
    /// the model's resolved `PricingModelRef` and usage counters.  Unpriced or
    /// unknown models leave `response.cost = None` (never an error).  The
    /// `CostTracker` budget authority is UNTOUCHED by this path.
    estimator: Option<Arc<CostEstimator>>,
    /// Most recently observed 2xx rate-limit header snapshot.
    ///
    /// Parsed via [`RateLimitInfo::from_headers`] on every successful
    /// `drive_non_stream` and `drive_stream` connect-success response.
    /// Exposed via [`Self::last_rate_limit_info`].  `None` until the first
    /// successful response is received.  Interior-mutable so non-`&mut self`
    /// callers (the `OrchestratorApiClient` impls) can update it.
    ///
    /// TUI wiring: no existing `OrchestratorHandle` surface maps naturally to
    /// per-request rate-limit metadata (all status APIs are session-wide
    /// snapshots). Callers that need this should call `last_rate_limit_info()`
    /// on the adapter directly. A future task can thread it into the handle if
    /// needed.
    last_rate_limit: Mutex<Option<RateLimitInfo>>,
    /// The request id of the most recently recorded response, with its origin,
    /// captured in [`Self::record_rate_limit_from_headers`] (the stream
    /// connect-success + non-stream header pass). Read via the
    /// `last_request_id()` trait method to stamp the persisted assistant line's
    /// top-level `requestId`. The value is the provider's server-side id when a
    /// known id header is present ([`RequestIdOrigin::Server`]); otherwise it
    /// falls back to the client-generated `x-request-id` we sent
    /// ([`RequestIdOrigin::Client`]) so the field is never blank — but that
    /// fallback is correlation-only and is NOT valid for provider-side log
    /// lookups. `None` until the first recorded response (or when both are
    /// absent).
    last_request_id: Mutex<Option<(String, RequestIdOrigin)>>,
    /// Number of budget-consuming retry attempts the most recent drive
    /// performed before its terminal outcome (`RetryState::attempt`). Recorded
    /// on the non-stream success path and at stream connect-success; read via
    /// the `last_retry_count()` trait method (both `OrchestratorApiClient` and
    /// `StreamingApiClient`) so the orchestrator's cost-recording call sites can
    /// pass the real retry count to `CostTracker::record_api_response_v2`
    /// instead of the previous hardcoded `0` (#5 main-loop parity). For the
    /// stream this reflects connect-phase retries only (the value the adapter
    /// knows when it returns the `BoxStream`). `0` until the first drive.
    last_retry_count: Mutex<u32>,
    /// Recovery status and rejected historical identities. New message IDs
    /// remain unaffected, even when their thinking bytes match an older turn.
    thinking_recovery: crate::thinking_scope::ThinkingRecoveryScope,
    /// Most recently observed RAW per-window utilization snapshot.
    ///
    /// Task 2 (llm-runtime future-work batch 5): parsed via
    /// [`RawUtilization::from_headers`] alongside the [`RateLimitInfo`]
    /// parse in `record_rate_limit_from_headers` — claude-code assigns
    /// `rawUtilization = extractRawUtilization(headers)` on the same passes
    /// that compute the limits (`claudeAiLimits.ts:476`). Assigned
    /// UNCONDITIONALLY on every recorded response (unlike `last_rate_limit`,
    /// which is gated on `has_unified_headers()`), so a later response
    /// without the per-window quartet resets it to the empty snapshot
    /// exactly like the TS module state. `None` until the first recorded
    /// response. Recorded on success passes here AND, as of B6-T1, on a
    /// TERMINAL 429 — TS extracts raw utilization from error headers too
    /// (`extractRawUtilization`, `claudeAiLimits.ts:500`). The 429 path stages
    /// the raw snapshot in [`Self::pending_429`] and promotes it into this
    /// cache only when the turn dies on the 429 (via
    /// [`Self::promote_pending_429`]), never on a retried-then-recovered
    /// attempt. EMPTY snapshots are preserved too so a later headerless
    /// success or terminal 429 can clear stale raw-window state exactly like
    /// the TS module assignment. Exposed via the
    /// `OrchestratorApiClient::last_raw_utilization` override.
    last_raw_utilization: Mutex<Option<RawUtilization>>,
    /// User-facing copy composed from the most recent 429 **error** response.
    ///
    /// Task 6 (llm-runtime future-work batch 5): claude-code builds the
    /// rejected-limits view from the terminal 429's own headers and renders
    /// `getRateLimitErrorMessage` as the user-visible error content
    /// (`errors.ts:480-524`). Set on EVERY decoded 429 by
    /// [`Self::record_rate_limit_from_429`] — Anthropic's composed limits copy
    /// when unified headers are present, or an actionable OpenRouter free-tier
    /// message (including `error.message`) for `…:free` models. Other
    /// headerless 429s leave this as `None`. Cleared on every successful
    /// response, so it always reflects the most recent response seen. Exposed
    /// via the `OrchestratorApiClient::last_rate_limit_error_message` override.
    last_429_message: Mutex<Option<String>>,
    /// 429-attempt state staged until the retry loop declares the error
    /// TERMINAL.
    ///
    /// B6-T1: claude-code updates the limits/raw module state ONLY in the
    /// terminal catch handler `extractQuotaStatusFromError`
    /// (claudeAiLimits.ts:487), never on a retried attempt that later
    /// recovers. [`Self::record_rate_limit_from_429`] writes this slot on
    /// every decoded 429; [`Self::promote_pending_429`] promotes it into
    /// `last_rate_limit` / `last_raw_utilization` only at the decode-terminal
    /// returns of both drive fns. Discarded at drive-entry and on any
    /// subsequent success — so a retried-then-recovered 429 never plants a
    /// rejected snapshot (the prior per-attempt-write divergence, CLOSED).
    pending_429: Mutex<Option<Pending429>>,
    /// One-shot subagent wrap-up hint, armed from a near-limit 2xx snapshot and
    /// consumed by the query loop exactly once for that five-hour window.
    pending_near_limit_wrap_up_hint: Mutex<bool>,
    /// Five-hour reset epoch that already armed or consumed the near-limit
    /// wrap-up hint. This is the per-window dedupe key: the same reset must not
    /// re-arm after consumption; a later reset opens a new window.
    near_limit_wrap_up_window_key: Mutex<Option<u64>>,
    /// Optional AWS auth-refresh driver (2.1.198 `ZBd`, `awsAuthRefresh`).
    ///
    /// When set, an AWS-auth failure (401/403) on the Bedrock provider runs
    /// the client-side refresh flow and retries the request, bounded at
    /// [`crate::aws_auth::AWS_AUTH_MAX_ATTEMPTS`] (`Ygf = 2`). `None` (the
    /// default) keeps every error path unchanged. Provider-gated inside
    /// [`crate::aws_auth::is_aws_auth_error`] — non-AWS providers never
    /// reach the refresh.
    aws_auth: Option<Arc<dyn crate::aws_auth::AwsAuthRefresh>>,
    /// Monotonic guard timestamp (ms) for the rate-limit record path — the
    /// binary's `Nha` (@210953364). A record whose timestamp is OLDER than
    /// this is dropped so an out-of-order (parallel) response cannot overwrite
    /// a newer rate-limit snapshot (2.1.196 flicker fix). `None` until the
    /// first record.
    last_rate_limit_record_ts_ms: Mutex<Option<u128>>,
    /// Test-only override for the streaming idle-watchdog timeout. `Some(d)`
    /// forces `d` (bypassing the env resolver whose floor is 5 min, which is
    /// otherwise untestable); `None` (production) uses
    /// [`crate::model::stream_watchdog::resolve_stream_idle_timeout`].
    stream_idle_timeout_override: Option<Duration>,
    /// Test-only override for the connect-phase first-byte watchdog. `None`
    /// resolves the provider/body-aware timeout from the process environment.
    stream_first_byte_timeout_override: Option<Duration>,
    /// Conversation-session scoped OpenAI Responses WebSocket connection/cache.
    ///
    /// The adapter is used by one conversation runtime; mobile already enforces
    /// one in-flight turn. The underlying `llm-runtime` session still only sends
    /// `previous_response_id` when the new request is a strict compatible
    /// extension of the previous completed request.
    responses_ws_session: tokio::sync::Mutex<ResponsesWebSocketSession>,
}

/// (cc 2.1.219) Per-QUERY `anthropic-dispatch-id` state — the oracle's `Kt`/
/// `no` pair.
///
/// Both live in the `let` list of the per-query generator `erp` (2.1.220
/// @237543834: `…,_o=0,Kt=!1,no=!1,Dn=!1,…`), NOT at module scope: the module
/// scope in that file holds only the `var` constants `S8s`/`Vtp` (@237594040).
/// A fallback therefore lasts for the rest of the CURRENT query and the next
/// query re-sends the header — the deliberate contrast is the afk-beta arm a
/// few bytes earlier, which calls the global setters `F2(!1),gnn(!0)` and says
/// "for this session".
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct DispatchHeaderState {
    /// `fB(i.querySource)==="auxiliary"` — utility queries (compaction, recap,
    /// title generation, …) never carry the header. `false` also covers the
    /// oracle's `fB(void 0) === undefined` case, which passes the gate.
    auxiliary: bool,
    /// `Kt` — an attempt of THIS query that carried the header failed with a
    /// 5xx / connection error, so every later attempt of this query omits it.
    fallen_back: bool,
}

impl DispatchHeaderState {
    /// `fB(querySource) === "auxiliary"` — the side-query driver
    /// ([`ApiService::messages_create_side_query`]), which serves the oracle's
    /// compaction / recap / title-generation / memory utility queries.
    const AUXILIARY: Self = Self {
        auxiliary: true,
        fallen_back: false,
    };
}

/// 429-attempt state held until the retry loop declares the error TERMINAL —
/// TS updates module state only in the terminal catch handler
/// (`extractQuotaStatusFromError`, claudeAiLimits.ts:487), never on retried
/// attempts. Promoted by [`ApiService::promote_pending_429`]; discarded
/// on drive-entry and on any subsequent success.
struct Pending429 {
    /// Forced-rejected limits snapshot (`from_429_error_headers`); `None`
    /// when the unified-header limits gate did not pass but raw windows did.
    info: Option<RateLimitInfo>,
    /// Raw per-window utilization from the SAME error headers, computed
    /// UNCONDITIONALLY (`extractRawUtilization`, ts:500 — independent of the
    /// limits gate).
    raw: RawUtilization,
}

/// A previously-pinned cache_edits block plus the user-message index it must be
/// re-inserted at. Mirrors claude-code's `CachedMCPinnedEdits`
/// (`services/api/claude.ts:3057-3060`).
#[derive(Debug, Clone, Default)]
struct PinnedCacheEdits {
    /// Index into the request `messages` (the user message to splice into).
    user_message_index: usize,
    /// The cache_edits delete operations to re-insert at that position.
    edits: Vec<crate::CacheEdit>,
}

/// Cache-editing builder inputs — the `newCacheEdits` + `pinnedEdits` args of
/// claude-code's `addCacheBreakpoints`. Default is empty (no producer wired):
/// only the gate-armed `cache_reference`-on-tool_results pass runs by default.
#[derive(Debug, Clone, Default)]
struct CacheEditingInputs {
    /// New cache_edits delete ops to insert into the last user message and pin.
    new_edits: Vec<crate::CacheEdit>,
    /// Previously-pinned cache_edits to re-insert at their original positions.
    pinned: Vec<PinnedCacheEdits>,
}

fn parse_fallback_chain(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    for model in raw.split(',').map(str::trim).filter(|m| !m.is_empty()) {
        if !out.iter().any(|existing| existing == model) {
            out.push(model.to_string());
        }
    }
    out
}

fn extra_body_object() -> Option<serde_json::Map<String, serde_json::Value>> {
    extra_body_object_uncached()
}

fn extra_body_object_uncached() -> Option<serde_json::Map<String, serde_json::Value>> {
    let Ok(t) = std::env::var("CLAUDE_CODE_EXTRA_BODY") else {
        return None;
    };
    if t.is_empty() {
        return None;
    }
    match serde_json::from_str::<serde_json::Value>(&t) {
        Ok(serde_json::Value::Object(map)) => Some(map),
        Ok(_) => {
            tracing::error!(
                "CLAUDE_CODE_EXTRA_BODY env var must be a JSON object, but was given {t}"
            );
            None
        }
        Err(err) => {
            tracing::error!("Error parsing CLAUDE_CODE_EXTRA_BODY: {err}");
            None
        }
    }
}

fn extra_metadata_object() -> Option<serde_json::Map<String, serde_json::Value>> {
    extra_metadata_object_uncached()
}

fn extra_metadata_object_uncached() -> Option<serde_json::Map<String, serde_json::Value>> {
    let Ok(extra_str) = std::env::var("CLAUDE_CODE_EXTRA_METADATA") else {
        return None;
    };
    if extra_str.is_empty() {
        return None;
    }
    match serde_json::from_str::<serde_json::Value>(&extra_str) {
        Ok(serde_json::Value::Object(extra)) => Some(extra),
        _ => {
            tracing::error!(
                "CLAUDE_CODE_EXTRA_METADATA env var must be a JSON object, but was given {extra_str}"
            );
            None
        }
    }
}

impl ApiService {
    /// Execute a canonical side-query request through the same host hooks and
    /// retry driver as the high-level side-query entry points.
    pub async fn execute_side_query_request(
        &self,
        mut request: LlmRequest,
    ) -> Result<LlmResponse, LlmError> {
        request.stream = false;
        let control = resolve_retry_control_with_settings(
            &request.model,
            None,
            self.effective_subscriber().is_subscriber,
            &ResolveRetryEnv::from_process_env(),
            self.settings_max_retries,
        );
        self.drive_non_stream(request, control, DispatchHeaderState::AUXILIARY)
            .await
    }

    /// Classifier side queries own a retry budget independent of main turns.
    pub async fn execute_classifier_request(
        &self,
        mut request: LlmRequest,
        max_retries: u32,
    ) -> Result<LlmResponse, LlmError> {
        request.stream = false;
        let mut control = resolve_retry_control_with_settings(
            &request.model,
            None,
            self.effective_subscriber().is_subscriber,
            &ResolveRetryEnv::from_process_env(),
            self.settings_max_retries,
        );
        control.max_retries = max_retries;
        self.drive_non_stream(request, control, DispatchHeaderState::AUXILIARY)
            .await
    }

    /// Stream a canonical request through the accounting-aware physical driver.
    pub async fn stream_request(
        &self,
        mut request: LlmRequest,
    ) -> Result<BoxStream<'static, Result<LlmEvent, LlmError>>, LlmError> {
        request.stream = true;
        self.drive_stream(request).await
    }

    /// Opt-aware panel stream. Context remains typed and never enters the body.
    #[allow(clippy::too_many_arguments)]
    pub async fn stream_with_attempt_opts(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
        forced_tool: Option<&str>,
        effort: Option<serde_json::Value>,
        max_tokens: Option<u32>,
        query_source: Option<&str>,
        model_attempt: Option<platform_api::ModelAttemptContext>,
    ) -> Result<BoxStream<'static, Result<LlmEvent, LlmError>>, LlmError> {
        let mut request =
            self.build_request(model, profile, system, messages, tools, true, max_tokens)?;
        request.effort = effort;
        request.query_source = query_source.map(str::to_string);
        request.model_attempt = model_attempt;
        if let Some(name) = forced_tool {
            request.tool_choice = Some(crate::ToolChoice::Tool { name: name.into() });
        }
        self.drive_stream(request).await
    }
    /// Install the host's registered-attempt authority after composition.
    /// Ordinary requests without context never invoke this hook.
    pub fn set_model_attempt_hooks(&self, hooks: Arc<dyn crate::ModelAttemptHooks>) {
        let retired = self
            .model_attempt_hooks
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .replace(hooks);
        drop(retired);
    }

    /// Retire host authority after the host has drained all producers and
    /// receipts. Registered requests then fail closed; ordinary requests are
    /// unchanged. Other service owners must not extend a closed session lease.
    pub fn clear_model_attempt_hooks(&self) {
        let retired = self
            .model_attempt_hooks
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        // Host destructors can release their own graphs or reenter the service.
        // Never run them under the service's hook lock.
        drop(retired);
    }

    async fn begin_model_attempt(
        &self,
        request: &LlmRequest,
        prepared: &crate::PreparedLlmCall,
    ) -> Result<crate::model_attempt::WireAttempt, LlmError> {
        let Some(context) = request.model_attempt.as_ref() else {
            return Ok(crate::model_attempt::WireAttempt::new(None));
        };
        let hooks = self
            .model_attempt_hooks
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(crate::model_attempt::missing_hooks_error)?;
        let lease = hooks
            .begin(context, request, prepared)
            .await
            .map_err(crate::model_attempt::accounting_error)?;
        Ok(crate::model_attempt::WireAttempt::new(Some(lease)))
    }
    /// Resolve the selected main route plus an optional same-profile vision delegate.
    pub fn resolve_media_route(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<MediaRoute, LlmError> {
        self.client.resolve_media_route(model, profile)
    }

    fn apply_side_query_thinking(
        &self,
        req: &mut LlmRequest,
        model: &str,
        thinking: Option<crate::model::thinking::ThinkingConfig>,
        temperature: Option<f32>,
    ) {
        use crate::model::thinking::{model_sends_temperature, session_thinking_active};

        let has_thinking = thinking.is_some_and(session_thinking_active);
        req.reasoning = thinking.and_then(|thinking| {
            crate::model::thinking::reasoning_for_request(thinking, model, req.max_tokens)
        });
        req.temperature = temperature.map(f64::from).or_else(|| {
            if thinking.is_some()
                && !has_thinking
                && model_sends_temperature(model)
                && !matches!(
                    thinking,
                    Some(crate::model::thinking::ThinkingConfig::Automatic)
                )
            {
                Some(1.0)
            } else {
                None
            }
        });
    }

    /// Construct the service.  Called by Task 10 host constructors.
    ///
    /// `version` is the build version string embedded in the User-Agent header.
    ///
    /// `estimator` — when `Some`, a successful response decode populates
    /// `LlmResponse.cost` via the llm-runtime `CostEstimator`.  Pass
    /// `None` to leave ordinary cost estimation disabled. Registered attempts
    /// always retain a frozen quote for their host accounting hooks.
    #[must_use]
    pub fn new(
        client: Arc<DefaultLlmClient>,
        transport: Arc<dyn Transport>,
        subscriber: SubscriberState,
        ua: UserAgentEnv,
        version: impl Into<String>,
        analytics: Option<Arc<::telemetry::AnalyticsBus>>,
        fallback_model: Option<String>,
    ) -> Self {
        Self::new_with_estimator(
            client,
            transport,
            subscriber,
            ua,
            version,
            analytics,
            fallback_model,
            None,
        )
    }

    /// Construct the service with an explicit cost estimator.
    ///
    /// Hosts that have the `cost::PricingCatalog` available (desktop + mobile)
    /// call this instead of [`Self::new`] to get live `LlmResponse.cost` values.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_estimator(
        client: Arc<DefaultLlmClient>,
        transport: Arc<dyn Transport>,
        subscriber: SubscriberState,
        ua: UserAgentEnv,
        version: impl Into<String>,
        analytics: Option<Arc<::telemetry::AnalyticsBus>>,
        fallback_model: Option<String>,
        estimator: Option<Arc<CostEstimator>>,
    ) -> Self {
        Self::new_with_routing(
            client,
            transport,
            subscriber,
            ua,
            version,
            analytics,
            fallback_model,
            estimator,
            std::collections::BTreeMap::new(),
            None,
            None,
        )
    }

    /// Construct the service with routing overrides from `routing.fallback` /
    /// `routing.retry` settings.
    ///
    /// ## Constructor choice
    ///
    /// Hosts that parse `routing` settings call this after
    /// `parse_routing_overrides`; the older [`Self::new`] and
    /// [`Self::new_with_estimator`] paths delegate here with empty overrides so
    /// they continue to compile unchanged.
    ///
    /// ## Fallback precedence (per-request)
    ///
    /// Per-model `fallback_overrides` entry for the request's display model **wins**
    /// over the global `fallback_model` field.  When neither is set, no fallback
    /// is configured.
    ///
    /// ## Retry precedence
    ///
    /// `LINGXI_MAX_RETRIES` env > `settings_max_retries` > `DEFAULT_MAX_RETRIES` (10).
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_routing(
        client: Arc<DefaultLlmClient>,
        transport: Arc<dyn Transport>,
        subscriber: SubscriberState,
        ua: UserAgentEnv,
        version: impl Into<String>,
        analytics: Option<Arc<::telemetry::AnalyticsBus>>,
        fallback_model: Option<String>,
        estimator: Option<Arc<CostEstimator>>,
        fallback_overrides: std::collections::BTreeMap<String, Vec<String>>,
        settings_max_retries: Option<u32>,
        settings_backoff_ms: Option<u64>,
    ) -> Self {
        let models = client.available_models();
        let available_model_ids = models.iter().map(|m| m.display_model.clone()).collect();
        // Build alias→display map once so `messages_create_with_fallback` can
        // normalize an alias request to the display model before looking up
        // per-model fallback overrides (whose keys are display-normalized).
        let mut alias_to_display = std::collections::BTreeMap::new();
        for m in &models {
            for alias in &m.aliases {
                alias_to_display.insert(alias.clone(), m.display_model.clone());
            }
            // Map display_model → itself so the lookup is always correct
            // whether the caller used the canonical name or an alias.
            alias_to_display.insert(m.display_model.clone(), m.display_model.clone());
        }
        let fallback_models = fallback_model
            .as_deref()
            .map(parse_fallback_chain)
            .unwrap_or_default();
        Self {
            client,
            model_attempt_hooks: RwLock::new(None),
            transport,
            subscriber,
            subscription: None,
            retry_reporter: None,
            forced_tool_choice: None,
            thinking: RwLock::new(crate::model::thinking::ThinkingConfig::default()),
            request_metadata: None,
            cache_editing_inputs: CacheEditingInputs::default(),
            ua,
            version: version.into(),
            analytics,
            fallback_models,
            custom_cli_betas: Vec::new(),
            interactive_session: None,
            fallback_overrides,
            alias_to_display,
            settings_max_retries,
            settings_backoff_ms,
            available_model_ids,
            model_listings: models,
            estimator,
            last_rate_limit: Mutex::new(None),
            last_request_id: Mutex::new(None),
            last_retry_count: Mutex::new(0),
            thinking_recovery: crate::thinking_scope::ThinkingRecoveryScope::default(),
            last_raw_utilization: Mutex::new(None),
            last_429_message: Mutex::new(None),
            pending_429: Mutex::new(None),
            pending_near_limit_wrap_up_hint: Mutex::new(false),
            near_limit_wrap_up_window_key: Mutex::new(None),
            aws_auth: None,
            last_rate_limit_record_ts_ms: Mutex::new(None),
            stream_idle_timeout_override: None,
            stream_first_byte_timeout_override: None,
            responses_ws_session: tokio::sync::Mutex::new(ResponsesWebSocketSession::new()),
        }
    }

    /// Attach the AWS auth-refresh driver (2.1.198 `awsAuthRefresh` flow).
    /// Builder-style; the default is `None` (no refresh, errors stay terminal).
    #[must_use]
    pub fn with_aws_auth(mut self, aws_auth: Arc<dyn crate::aws_auth::AwsAuthRefresh>) -> Self {
        self.aws_auth = Some(aws_auth);
        self
    }

    /// Test-only: force the streaming idle-watchdog timeout (the env floor of
    /// 5 min is otherwise untestable). Builder-style; default `None`.
    #[cfg(test)]
    #[must_use]
    pub fn with_stream_idle_timeout_override(mut self, timeout: Option<Duration>) -> Self {
        self.stream_idle_timeout_override = timeout;
        self
    }

    /// Test-only: force the streaming first-byte timeout. Builder-style;
    /// default `None` uses the provider/body-aware environment resolver.
    #[cfg(test)]
    #[must_use]
    pub fn with_stream_first_byte_timeout_override(mut self, timeout: Duration) -> Self {
        self.stream_first_byte_timeout_override = Some(timeout);
        self
    }

    /// Attach the live subscription slot (batch-5 Task 3). When present and
    /// resolved, the drive loops read subscriber/enterprise state from it at
    /// call time instead of the build-time [`SubscriberState`] copy.
    #[must_use]
    pub fn with_subscription(
        mut self,
        slot: platform_api::subscription::SharedSubscription,
    ) -> Self {
        self.subscription = Some(slot);
        self
    }

    /// Attach host-validated, stable-deduplicated CLI beta additions.
    #[must_use]
    pub fn with_custom_cli_betas(mut self, betas: Vec<String>) -> Self {
        self.custom_cli_betas = betas;
        self
    }

    /// Attach the session's interaction mode for beta-header decisions.
    #[must_use]
    pub fn with_interactive_session(mut self, interactive: bool) -> Self {
        self.interactive_session = Some(interactive);
        self
    }

    /// Host-validated beta additions active for this service. Orchestrator
    /// context-window and compaction math must use the same list as request
    /// assembly (notably for the 1M-context beta).
    #[must_use]
    pub fn active_custom_betas(&self) -> &[String] {
        &self.custom_cli_betas
    }

    /// Attach a UI retry-status sink. The retry loop then reports each backoff
    /// (error text + attempt/max + delay) so the TUI can surface it, matching
    /// Claude Code's `SystemAPIErrorMessage` retry display.
    #[must_use]
    pub fn with_retry_reporter(mut self, reporter: Arc<dyn RetryReporter>) -> Self {
        self.retry_reporter = Some(reporter);
        self
    }

    /// Report a retry backoff to the attached [`RetryReporter`] (no-op if none).
    /// Called immediately before each retry sleep. `state.attempt` is the
    /// upcoming attempt number; `ctl.max_retries` the cap.
    fn report_retry(
        &self,
        error: &LlmError,
        delay: Duration,
        state: &RetryState,
        ctl: &RetryControl,
    ) {
        if let Some(reporter) = &self.retry_reporter {
            reporter.report(RetryInfo {
                // Oracle `OYr(e).formatted` = `sir(e)`, NOT the taxonomy's own
                // `Display`. This is the text the retry banner shows.
                message: crate::error::error_display_text(error),
                attempt: state.attempt,
                max_retries: ctl.max_retries,
                delay_ms: u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
            });
        }
    }

    /// Surface the retry status (if a reporter is attached) then sleep the
    /// backoff — the single choke point every retry-loop sleep routes through so
    /// the UI can show "Retrying in Ns… (attempt X/Y)" during the wait.
    async fn report_and_sleep_retry(
        &self,
        error: &LlmError,
        delay: Duration,
        state: &RetryState,
        ctl: &RetryControl,
    ) {
        self.report_retry(error, delay, state, ctl);
        tokio::time::sleep(delay).await;
    }

    pub fn with_forced_tool_choice(mut self, choice: crate::ToolChoice) -> Self {
        self.forced_tool_choice = Some(choice);
        self
    }

    /// Set the session thinking configuration. Builder-style; the default is
    /// [`ThinkingConfig::Adaptive`](crate::model::thinking::ThinkingConfig::Adaptive).
    #[must_use]
    pub fn with_thinking(mut self, thinking: crate::model::thinking::ThinkingConfig) -> Self {
        self.thinking = RwLock::new(thinking);
        self
    }

    /// Replace the live session thinking policy. The next request observes the
    /// new value; an already-open response stream is intentionally unaffected.
    pub fn set_thinking(&self, thinking: crate::model::thinking::ThinkingConfig) {
        *self
            .thinking
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = thinking;
    }

    fn thinking(&self) -> crate::model::thinking::ThinkingConfig {
        *self
            .thinking
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Set the identity for the Anthropic `metadata.user_id` field. Builder-style;
    /// the default is `None` (no `metadata` object emitted).
    #[must_use]
    pub fn with_request_metadata(mut self, metadata: crate::RequestMetadata) -> Self {
        self.request_metadata = Some(metadata);
        self
    }

    /// `getAPIMetadata()` (`services/api/claude.ts:503-528`): build the Anthropic
    /// request `metadata.user_id` value, which claude-code packs as a JSON STRING
    /// `JSON.stringify({...extra, device_id, account_uuid, session_id})`.
    ///
    /// * `extra` = the `CLAUDE_CODE_EXTRA_METADATA` env var when it parses to a
    ///   JSON object (any other value is ignored, mirroring the TS
    ///   debug-log-and-skip — we have no debug log).
    /// * Key order is `extra…, device_id, account_uuid, session_id` (workspace
    ///   `serde_json` `preserve_order`), and a colliding `extra` key keeps its
    ///   first position but takes the canonical value — byte-identical to the JS
    ///   object spread.
    ///
    /// The composition root supplies `device_id` ([`migrations::global_config::
    /// get_or_create_user_id`]), `account_uuid` (the OAuth account UUID, or `""`
    /// — the TS `getOauthAccountInfo()?.accountUuid ?? ''`), and `session_id`
    /// (the main session id, claude-code's `getSessionId()`).
    #[must_use]
    pub fn build_api_metadata_user_id(
        device_id: &str,
        account_uuid: &str,
        session_id: &str,
        parent_session_id: Option<&str>,
    ) -> String {
        let mut obj = serde_json::Map::new();
        if let Some(extra) = extra_metadata_object() {
            for (k, v) in extra {
                obj.insert(k, v);
            }
        }
        obj.insert(
            "device_id".to_string(),
            serde_json::Value::String(device_id.to_string()),
        );
        obj.insert(
            "account_uuid".to_string(),
            serde_json::Value::String(account_uuid.to_string()),
        );
        obj.insert(
            "session_id".to_string(),
            serde_json::Value::String(session_id.to_string()),
        );
        if let Some(parent_session_id) = parent_session_id.filter(|id| !id.is_empty()) {
            // The conditional spread is deliberately last: metadata.user_id is
            // a JSON string and key order is externally observable.
            obj.insert(
                "parent_session_id".to_string(),
                serde_json::Value::String(parent_session_id.to_string()),
            );
        }
        serde_json::to_string(&serde_json::Value::Object(obj)).unwrap_or_default()
    }

    /// Inject 1P cache-editing inputs (`newCacheEdits` / `pinnedEdits`). Test-only
    /// today — no production producer (cached-microcompact scheduler) is wired, so
    /// the default is empty. Builder-style.
    #[cfg(test)]
    #[must_use]
    fn with_cache_editing_inputs(mut self, inputs: CacheEditingInputs) -> Self {
        self.cache_editing_inputs = inputs;
        self
    }

    /// Effective subscriber state: the live shared snapshot when provided and
    /// resolved, else the static build-time state. Since M13 the build-time
    /// state (and the seed in the shared slot) already carries the enterprise
    /// tier PERSISTED in the stored credential (claude-code keeps
    /// `subscriptionType` inside `claudeAiOauth`), so the static fallback is
    /// correct from request #1; the slot exists to FRESHEN it once the
    /// background profile fetch lands. Poisoned/empty slot → static fallback
    /// (conservative, pre-batch-5 behavior).
    ///
    /// Granularity: each drive fn hoists this ONCE before its retry loop, so
    /// `RetryState`'s 429/enterprise gate is stable across a request's retry
    /// attempts — the TS-faithful behaviour (`getSubscriptionType()` reads per
    /// attempt-ish but the gate effectively stabilizes per request).
    fn effective_subscriber(&self) -> SubscriberState {
        let Some(slot) = &self.subscription else {
            return self.subscriber;
        };
        let Ok(guard) = slot.read() else {
            return self.subscriber;
        };
        let Some(snap) = guard.as_ref() else {
            return self.subscriber;
        };
        SubscriberState {
            is_subscriber: snap.is_subscriber,
            is_enterprise: snap.subscription_type.as_deref() == Some("enterprise"),
        }
    }

    /// 1P global-cache-scope gate — parity `shouldUseGlobalCacheScope`
    /// (`utils/betas.ts:227-232`): `getAPIProvider() === 'firstParty' &&
    /// !CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS`.
    ///
    /// LingXi resolves the concrete provider downstream of this provider-agnostic
    /// request builder and has no GrowthBook rollout bucketing, so the feature is
    /// kept **dormant**: it requires an explicit opt-in env
    /// (`LINGXI_GLOBAL_CACHE_SCOPE`) — mirroring the experimental-beta gating
    /// pattern used elsewhere — AND the subscriber (firstParty) signal, AND the
    /// shared experimental-betas kill switch must not be set. Default: off.
    fn should_use_global_cache_scope(&self) -> bool {
        if cache_env_truthy("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS") {
            return false;
        }
        // `firstParty` approximation at this layer: a Claude.ai subscriber (the
        // OAuth/first-party path). Opt-in env arms the otherwise-dormant feature.
        cache_env_truthy("LINGXI_GLOBAL_CACHE_SCOPE") && self.effective_subscriber().is_subscriber
    }

    /// 1h-TTL gate — parity `should1hCacheTTL` (`services/api/claude.ts:393-434`).
    ///
    /// The TS path is GrowthBook-allowlist + querySource gated (machinery LingXi
    /// lacks) plus a Bedrock env opt-in (`ENABLE_PROMPT_CACHING_1H_BEDROCK`).
    /// Kept **dormant**: honored only via the Bedrock-style explicit opt-in
    /// env `ENABLE_PROMPT_CACHING_1H` (default off), since LingXi has no
    /// querySource allowlist to consult. Folded into the emitted cache_control.
    fn should_1h_cache_ttl(&self) -> bool {
        if cache_env_truthy("ENABLE_PROMPT_CACHING_1H") {
            return true;
        }
        // claude-code `agentCacheTtlOverride`: an agent's
        // `experimental.cacheTtl` applies only when no setting/env asked for a
        // TTL, so the env check above wins. `"5m"` resolves to `false` here —
        // it is Anthropic's default lifetime and emits no `ttl` key.
        agent_cache_ttl_1h_override()
    }

    /// 1P experimental cache-EDITING gate — parity `useCachedMC`
    /// (`services/api/claude.ts:3067`, passed down from the caller at
    /// claude.ts:1531-1709, where it additionally requires
    /// `getAPIProvider()==='firstParty' && querySource==='repl_main_thread'`).
    ///
    /// LingXi resolves the concrete provider downstream of this provider-agnostic
    /// request builder and still lacks the rest of the Claude Code protocol:
    /// the once-per-session `CACHE_EDITING_BETA_HEADER` latch and the cross-call
    /// pinned-edits store. Because this partial path can mutate requests without
    /// the required session/header contract, it is kept FAIL-CLOSED here even
    /// when `LINGXI_CACHE_EDITING=1`. Default: off → no `cache_edits` /
    /// `cache_reference` ever emitted, so 3P traffic is byte-unchanged.
    fn should_use_cache_editing(&self) -> bool {
        false
    }

    // ── Shared request build ─────────────────────────────────────────────────

    /// Convert orchestrator-layer inputs into an `LlmRequest`.
    // An internal request-assembler: model + profile + system + msgs + tools +
    // stream + max_tokens are all genuinely distinct inputs (8/7).
    #[allow(clippy::too_many_arguments)]
    fn build_request(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        msgs: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
        stream: bool,
        max_tokens: Option<u32>,
    ) -> Result<LlmRequest, LlmError> {
        // Pre-wire pipeline (claude-code order): strip_excess_media →
        // normalizeMessagesForAPI (consecutive-role merge) → ensureToolResultPairing
        // (SEND-time repair of orphaned/missing/duplicate tool_use↔tool_result on
        // resumed/interrupted transcripts; strict no-op on a clean turn).
        // Session-scoped tool-search gate (Claude Code `$U()`), published by the
        // orchestrator. NOT inferred from whether THIS request's toolset carries
        // a `ToolSearch` declaration: `$U()` reads only the session mode +
        // provider, and the branch site `if(!$U())W=j6s(W);else W=xPy(W,a)` runs
        // for main-loop AND side-query requests alike. A side query assembled
        // with an empty toolset (compaction summarizer, recap) in a
        // tool-search-enabled session must therefore still take the ENABLED
        // branch — emitting "[…tools no longer available]" rather than the
        // disabled branch's "[…tool search not enabled]". The request's `tools`
        // remain the availability set (`a`) below.
        let mut msgs = msgs;
        let thinking_source_message_ids: Vec<_> = msgs
            .iter()
            .filter_map(|message| {
                if let ConversationMessage::Assistant { id, content, .. } = message {
                    content
                        .iter()
                        .any(|block| {
                            matches!(
                                block,
                                protocol::ContentBlock::Thinking { .. }
                                    | protocol::ContentBlock::RedactedThinking { .. }
                            )
                        })
                        .then_some(*id)
                } else {
                    None
                }
            })
            .collect();
        let thinking_recovery_scope = self.thinking_recovery_scope();
        thinking_recovery_scope.capture(&thinking_source_message_ids);
        if !crate::model::thinking_signature::thinking_must_round_trip(model, profile) {
            crate::model::thinking_signature::strip_marked_conversation_thinking(
                &mut msgs,
                &thinking_recovery_scope.messages(),
            );
        }
        let tool_search_enabled = platform_api::session_flags::tool_search_enabled();
        let available_tool_names: std::collections::HashSet<String> = tools
            .iter()
            .filter_map(|tool| tool.get("name").and_then(serde_json::Value::as_str))
            .map(str::to_string)
            .collect();
        let normalize = |messages| {
            to_llm_messages(ensure_tool_result_pairing(
                normalize_messages_for_api_with_tool_search(
                    strip_excess_media(messages, MAX_MEDIA_PER_REQUEST),
                    tool_search_enabled,
                    Some(&available_tool_names),
                ),
            ))
        };
        let messages = normalize(msgs)?;
        let tool_decls = to_tool_declarations(tools)?;

        let mut req = LlmRequest::new(model);
        if let Some(p) = profile {
            req = req.with_profile(p);
        }

        // Prompt-cache breakpoints (parity: claude-code getPromptCachingEnabled +
        // buildSystemPromptBlocks + addCacheBreakpoints). Anthropic permits at most
        // 4 ephemeral breakpoints per request; we place at most 3 — up to two on
        // the system blocks (prefix + rest, per splitSysPromptPrefix /
        // buildSystemPromptBlocks) and one on the last content block of the last
        // message — matching the TS baseline (the tools array gets none on the
        // non-global-cache path). The Anthropic codec serializes
        // Some(CacheControl::Ephemeral) as {"type":"ephemeral"}; non-Anthropic
        // codecs ignore the field, so this is a no-op for them.
        let enable_caching = prompt_caching_enabled(model);

        if let Some(s) = system {
            // Split the assembled system string into up-to-3 cache blocks
            // (here ≤2 — the attribution bucket is always empty for LingXi),
            // marking only the org-scoped buckets, per
            // `prompt::split_system_blocks`. Replaces the previous single
            // collapsed block.
            //
            // 1P global-cache path (dormant): when the experimental gate is on
            // we take splitSysPromptPrefix's global mode. LingXi does not yet
            // assemble SYSTEM_PROMPT_DYNAMIC_BOUNDARY into the prompt, so this
            // degenerates to the org default (boundaryIndex===-1 fallthrough) —
            // the scope:'global'/ttl:'1h' serialization is wired and ready but
            // inert until a boundary marker is assembled. See module residual.
            let split_opts = crate::prompt_format::SplitOptions {
                global_scope: self.should_use_global_cache_scope(),
                ttl_1h: self.should_1h_cache_ttl(),
            };
            req.system =
                crate::prompt_format::split_system_blocks_with(s, enable_caching, split_opts);
        }
        req.messages = messages;
        req.thinking_source_message_ids = thinking_source_message_ids;
        req.thinking_recovery_scope = Some(thinking_recovery_scope);

        // Exactly one message-level breakpoint, on the last cache-eligible content
        // block of the last message (claude.ts addCacheBreakpoints markerIndex =
        // len-1). Skip reasoning/redacted blocks (assistantMessageToMessageParam).
        if enable_caching {
            use crate::ContentBlock as LlmContentBlock;
            if let Some(last) = req.messages.last_mut() {
                if let Some(block) = last.content.iter_mut().rev().find(|b| {
                    !matches!(
                        b,
                        LlmContentBlock::Reasoning { .. }
                            | LlmContentBlock::RedactedThinking { .. }
                    )
                }) {
                    match block {
                        LlmContentBlock::Text { cache_control, .. }
                        | LlmContentBlock::ToolResult { cache_control, .. } => {
                            *cache_control = Some(CacheControl::Ephemeral);
                        }
                        // Image / ToolCall / etc.: no cache_control slot — skip.
                        _ => {}
                    }
                }
            }
        }

        // 1P experimental cache-editing pass (claude.ts addCacheBreakpoints,
        // 3108-3208). Gated behind `useCachedMC` (`should_use_cache_editing`):
        // when OFF (the default), this is a no-op and the request is
        // byte-identical to the pre-feature path. When ARMED it (a) re-inserts
        // previously-pinned cache_edits at their original positions, (b) inserts
        // the new cache_edits into the last user message, and (c) stamps
        // `cache_reference` onto every tool_result strictly before the last
        // cache_control marker — all with cross-block delete-ref dedup.
        if self.should_use_cache_editing() {
            apply_cache_editing(
                &mut req.messages,
                enable_caching,
                &self.cache_editing_inputs.new_edits,
                &self.cache_editing_inputs.pinned,
            );
        }

        req.tools = tool_decls; // No tool-array breakpoint (matches TS baseline).
                                // Forced tool choice (e.g. `--json-schema` → `StructuredOutput`). Unset
                                // for every normal turn, so the request carries no `tool_choice` and the
                                // model chooses freely — byte-identical to the pre-feature request.
        if let Some(choice) = &self.forced_tool_choice {
            req.tool_choice = Some(choice.clone());
        }
        if req.model.contains("deepseek") || req.profile.as_deref() == Some("deepseek") {
            tracing::debug!(
                event = "build_request",
                model = %req.model,
                profile = req.profile.as_deref().unwrap_or("<none>"),
                messages = req.messages.len(),
                tools = req.tools.len(),
                forced_tool_choice = self.forced_tool_choice.is_some(),
                active_tool_choice = ?req.tool_choice,
                stream = req.stream,
            );
        }
        req.stream = stream;

        // max_tokens (DIV-3): an explicit escalation wins; ordinary turns use a
        // model-aware request default. Catalog `limit.output` is a hard ceiling,
        // not a request default (notably OpenRouter GLM Free advertises 230.4k
        // output inside a 256k total context window).
        let requested_max_tokens = max_tokens.unwrap_or_else(|| {
            u32::try_from(crate::model::context_window::default_output_tokens_for_model(model))
                .unwrap_or(u32::MAX)
        });
        req.max_tokens = Some(
            crate::model::context_window::known_output_token_limit_for_model(model)
                .map(|limit| u32::try_from(limit).unwrap_or(u32::MAX))
                .map_or(requested_max_tokens, |limit| {
                    requested_max_tokens.min(limit)
                }),
        );

        // Bound max_tokens so input + output fit the model's context window.
        // Even a safe ordinary output default may not fit beside a long prompt;
        // reserve the structured input estimate (system + messages + tools)
        // plus provider-formatting headroom. Claude models (output << context)
        // are unaffected unless the input is near-full.
        let context_window =
            crate::model::context_window::context_window_for_model(model, &self.custom_cli_betas);
        let input_est = crate::model::count_tokens::approximate_tokens(&req);
        if let Some(mt) = req.max_tokens {
            let bounded = bound_output_to_context(mt, context_window, input_est);
            if bounded == 0 {
                return Err(LlmError::ContextOverflow {
                    token_gap: input_est.saturating_sub(context_window),
                });
            }
            req.max_tokens = Some(bounded);
        }

        // thinking (DIV-1) + temperature (DIV-4), mirroring claude.ts:1596-1630
        // and claude.ts:1693. Computed AFTER max_tokens is known (the fixed-
        // budget cap clamps to max_tokens-1).
        {
            use crate::model::thinking::{model_sends_temperature, session_thinking_active};

            let thinking = self.thinking();
            let has_thinking = session_thinking_active(thinking);

            // The claude/non-claude branch, the env kill switches and the
            // budget clamp live in `model::thinking::reasoning_for_request` —
            // the SAME session-config resolution the compaction side-query
            // path inherits (cc 2.1.198). Behavior is byte-identical to the
            // previous inline block.
            req.reasoning =
                crate::model::thinking::reasoning_for_request(thinking, model, req.max_tokens);

            // temperature:1 ONLY when thinking is disabled AND the model is in the
            // `rhn` temperature-gate set (binary @205866168:
            // `!xs && rhn(u) ? temperatureOverride ?? 1 : void 0`). The default
            // opus-4-8 (and 4-7/fable-5/mythos-5/unknowns) are NOT in `rhn` → the
            // field is omitted. The Anthropic codec emits temperature on Some only.
            req.temperature = if !has_thinking
                && !matches!(thinking, crate::model::thinking::ThinkingConfig::Automatic)
                && model_sends_temperature(model)
            {
                Some(1.0)
            } else {
                None
            };
        }

        // metadata.user_id (DIV-2): claude-code always sends it. `None` (no
        // identity wired) omits the object — byte-identical to the prior request.
        req.metadata = self.request_metadata.clone();

        Ok(req)
    }

    fn log_deepseek_prepared_request(model: &str, prepared: &crate::PreparedLlmCall, stream: bool) {
        let body = &prepared.provider_request.body_json;
        let is_deepseek = model.contains("deepseek")
            || body
                .get("model")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|model| model.contains("deepseek"))
            || prepared.provider_request.url.contains("api.deepseek.com");
        if !is_deepseek {
            return;
        }

        let (message_count, messages_with_reasoning_content, last_assistant_reasoning_len) = body
            .get("messages")
            .and_then(serde_json::Value::as_array)
            .map_or((0usize, 0usize, 0usize), |messages| {
                let with_reasoning = messages
                    .iter()
                    .filter(|message| message.get("reasoning_content").is_some())
                    .count();
                let last_assistant_reasoning_len = messages
                    .iter()
                    .rev()
                    .find(|message| {
                        message.get("role").and_then(serde_json::Value::as_str) == Some("assistant")
                    })
                    .and_then(|message| message.get("reasoning_content"))
                    .and_then(serde_json::Value::as_str)
                    .map_or(0, str::len);
                (messages.len(), with_reasoning, last_assistant_reasoning_len)
            });
        tracing::debug!(
            target = "llm_runtime::service",
            event = "deepseek_prepared_request",
            stream = stream,
            model = %model,
            provider_url = %prepared.provider_request.url,
            message_count = message_count,
            messages_with_reasoning_content = messages_with_reasoning_content,
            last_assistant_reasoning_len = last_assistant_reasoning_len,
            tool_choice = ?body.get("tool_choice"),
            tools = body
                .get("tools")
                .and_then(serde_json::Value::as_array)
                .map_or(0, |tools| tools.len()),
            thinking = ?body.get("thinking"),
        );
    }

    /// Inject betas + User-Agent + request-id headers onto a prepared request.
    ///
    /// **Header name `x-request-id`** — sourced from `api-client/src/anthropic.rs`
    /// where it is written as `("x-request-id".into(), new_request_id())`.
    ///
    /// Reads [`Self::effective_subscriber`] directly (one resolver call per
    /// attempt — these injectors run once per prepare/execute attempt, so the
    /// live-slot read here is per-attempt, the lighter diff vs. threading the
    /// hoisted value through as a parameter).
    /// Build the per-request [`BetaContext`] (the binary's `xLr(model)` inputs)
    /// from the prepared request body: the resolved model id and `speed: "fast"`.
    /// Interactivity and `showThinkingSummaries` come from the process session
    /// flags published by the composition root, matching Claude's module-level
    /// `getIsNonInteractiveSession()` / initial-settings reads.
    fn beta_context(&self, prepared: &crate::PreparedLlmCall) -> BetaContext {
        let model = prepared
            .provider_request
            .body_json
            .get("model")
            .and_then(serde_json::Value::as_str)
            // Vertex/Bedrock codecs move the model into the URL; keep beta
            // capability checks tied to the resolved request model.
            .unwrap_or(&prepared.route.resolved_route.request_model);
        let fast_mode = prepared
            .provider_request
            .body_json
            .get("speed")
            .and_then(serde_json::Value::as_str)
            == Some("fast");
        // The `effort-2025-11-24` beta gates on the body carrying
        // `output_config.effort`.
        let has_effort = prepared
            .provider_request
            .body_json
            .get("output_config")
            .and_then(|oc| oc.get("effort"))
            .is_some();
        let has_tool_search = prepared
            .provider_request
            .body_json
            .get("tools")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|tools| {
                tools.iter().any(|tool| {
                    tool.get("name").and_then(serde_json::Value::as_str) == Some("ToolSearch")
                        || tool
                            .get("defer_loading")
                            .and_then(serde_json::Value::as_bool)
                            == Some(true)
                })
            });
        let interactive = self
            .interactive_session
            .unwrap_or_else(|| !platform_api::session_flags::effective_non_interactive_session());
        BetaContext::for_model(model)
            .with_interactive(interactive)
            .with_show_thinking_summaries(platform_api::session_flags::show_thinking_summaries())
            .with_fast_mode(fast_mode)
            .with_effort(has_effort)
            .with_tool_search(has_tool_search)
            .with_context_hint(
                prepared
                    .provider_request
                    .body_json
                    .get("context_hint")
                    .is_some(),
            )
    }

    #[cfg(test)]
    fn interactive_session_for_test(&self) -> bool {
        self.interactive_session
            .unwrap_or_else(|| !platform_api::session_flags::effective_non_interactive_session())
    }

    /// Return host-validated CLI betas only for the first-party Anthropic
    /// route. Custom providers may share the Anthropic wire protocol, but must
    /// never inherit a first-party experimental header by accident.
    fn custom_cli_betas(&self, prepared: &crate::PreparedLlmCall) -> Vec<String> {
        if !Self::direct_anthropic_api_route(prepared) {
            return Vec::new();
        }
        self.custom_cli_betas.clone()
    }

    /// A direct first-party Anthropic API route, resolved after profile/model
    /// selection. Custom Anthropic-wire gateways are deliberately excluded:
    /// they may understand the stable Messages schema, but must not inherit
    /// Claude Code's private first-party betas or fast tier.
    fn direct_anthropic_api_route(prepared: &crate::PreparedLlmCall) -> bool {
        if prepared.route.resolved_route.provider_id != crate::ProviderId::AnthropicFirstParty
            || prepared.route.protocol != crate::ProtocolFamily::AnthropicMessages
        {
            return false;
        }
        url::Url::parse(&prepared.provider_request.url).is_ok_and(|url| {
            url.scheme() == "https"
                && url.host_str() == Some(FIRST_PARTY_API_HOST)
                && url.port().is_none()
        })
    }

    fn enforce_fast_route(prepared: &mut crate::PreparedLlmCall) {
        let model = prepared
            .provider_request
            .body_json
            .get("model")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(&prepared.route.resolved_route.request_model);
        let allowed = Self::direct_anthropic_api_route(prepared)
            && platform_api::model_capabilities::has_capability(
                model,
                platform_api::model_capabilities::ModelCapability::FastMode,
            );
        if allowed {
            return;
        }
        if let Some(body) = prepared.provider_request.body_json.as_object_mut() {
            body.remove("speed");
        }
        let Some(header) = prepared
            .provider_request
            .headers
            .get("anthropic-beta")
            .cloned()
        else {
            return;
        };
        let retained = header
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty() && *part != FAST_MODE)
            .collect::<Vec<_>>()
            .join(",");
        if retained.is_empty() {
            prepared.provider_request.headers.remove("anthropic-beta");
        } else {
            prepared
                .provider_request
                .headers
                .insert("anthropic-beta".to_string(), retained);
        }
    }

    /// `true` for protocols that speak to Anthropic models (first-party or via
    /// Bedrock/Vertex). Only these get the `claude-cli/<ver>` User-Agent;
    /// OpenAI / Gemini / Azure / Copilot routes get a neutral UA so we don't
    /// announce ourselves as Anthropic's official CLI to third-party providers.
    fn is_anthropic_family_protocol(protocol: &crate::ProtocolFamily) -> bool {
        matches!(
            protocol,
            crate::ProtocolFamily::AnthropicMessages
                | crate::ProtocolFamily::BedrockClaude
                | crate::ProtocolFamily::VertexClaude
                | crate::ProtocolFamily::FoundryClaude
        )
    }

    /// Provider-aware User-Agent. Anthropic-family routes keep the byte-faithful
    /// `claude-cli/...` UA. Other routes get a neutral `LingXi-Code/<ver>` UA —
    /// but only when an authenticator hasn't already set one (e.g. Copilot's
    /// `User-Agent: LingXi-Code`), which avoids shipping two conflicting UA
    /// headers on a case-sensitive header map.
    fn apply_user_agent(&self, prepared: &mut crate::PreparedLlmCall) {
        if Self::is_anthropic_family_protocol(&prepared.route.protocol) {
            prepared.provider_request.headers.insert(
                "user-agent".to_string(),
                user_agent(&self.ua, &self.version),
            );
        } else if !prepared.provider_request.headers.contains_key("user-agent")
            && !prepared.provider_request.headers.contains_key("User-Agent")
        {
            prepared.provider_request.headers.insert(
                "user-agent".to_string(),
                format!("LingXi-Code/{}", self.version),
            );
        }
    }

    /// Port of claude-code's `B0t` (2.1.207): parse `CLAUDE_CODE_EXTRA_BODY` into a
    /// JSON object to be spread into the outgoing Anthropic-family request body.
    ///
    /// * Non-object env value → the object is ignored and an error is logged with
    ///   the byte-exact claude-code string
    ///   `CLAUDE_CODE_EXTRA_BODY env var must be a JSON object, but was given {t}`.
    /// * A parse failure logs `Error parsing CLAUDE_CODE_EXTRA_BODY: {err}`.
    /// * `betas` (claude-code's `ol` arg — the bedrock/body beta list, empty on the
    ///   first-party path where betas ride the `anthropic-beta` header) is folded
    ///   into `anthropic_beta`: append-dedupe when the extra body already carries
    ///   that array, else set it.
    ///
    /// Kept under the original `CLAUDE_CODE_` env name (like the sibling
    /// `CLAUDE_CODE_EXTRA_METADATA` at [`ApiService::build_api_metadata_user_id`])
    /// — these are wire-parity vars preserved verbatim through the `LINGXI_` rename.
    fn parse_extra_body(betas: &[String]) -> serde_json::Map<String, serde_json::Value> {
        let mut r = serde_json::Map::new();
        // claude-code enters the parse branch only when the env var is truthy; an
        // empty string is falsy in JS, so an empty value is a silent no-op.
        if let Some(map) = extra_body_object() {
            r = map;
        }
        if !betas.is_empty() {
            match r.get_mut("anthropic_beta") {
                // Extra body already carries the array → append only the missing
                // entries, preserving the extra body's order (claude-code's
                // `[...o, ...n.filter((s)=>!o.includes(s))]`).
                Some(serde_json::Value::Array(existing)) => {
                    for b in betas {
                        if !existing.iter().any(|v| v.as_str() == Some(b.as_str())) {
                            existing.push(serde_json::Value::String(b.clone()));
                        }
                    }
                }
                _ => {
                    r.insert(
                        "anthropic_beta".to_string(),
                        serde_json::Value::Array(
                            betas
                                .iter()
                                .cloned()
                                .map(serde_json::Value::String)
                                .collect(),
                        ),
                    );
                }
            }
        }
        r
    }

    /// Merge `CLAUDE_CODE_EXTRA_BODY` into a prepared Anthropic-family request body
    /// (claude-code `B0t` spread — 2.1.207). No-op for non-Anthropic routes and
    /// when the env var is unset/empty, so those bodies stay byte-identical.
    ///
    /// `output_config` is peeled from the extra body and the computed
    /// `output_config` is layered on top so computed keys win (claude-code
    /// `Ii={...extra.output_config}; <compute mutates Ii>`); the merged object is
    /// emitted only when non-empty. The computed top-level `speed` is likewise
    /// re-applied after the spread so it wins over an extra-body `speed`
    /// (claude-code spreads `...Vs` BEFORE `...ze!==void 0&&{speed:ze}`).
    /// Remaining keys follow JS object-spread collision semantics — a colliding
    /// key keeps its position but takes the extra value, a new key appends at the
    /// tail (`serde_json` `preserve_order`).
    ///
    /// Runs after the beta/User-Agent header injectors so a user-supplied
    /// `speed`/`output_config` in the extra body never leaks into the computed
    /// `anthropic-beta` header ([`ApiService::beta_context`] reads the pre-merge
    /// body).
    fn merge_extra_body(&self, prepared: &mut crate::PreparedLlmCall) {
        if !Self::is_anthropic_family_protocol(&prepared.route.protocol) {
            return;
        }
        // Bedrock carries a narrow beta subset in `anthropic_beta` inside the
        // body. Other Anthropic-family routes carry their betas in headers.
        let body_betas = if matches!(
            prepared.route.protocol,
            crate::ProtocolFamily::BedrockClaude
        ) {
            bedrock_extra_body_betas(&self.beta_context(prepared))
        } else {
            Vec::new()
        };
        let mut extra = Self::parse_extra_body(&body_betas);
        if extra.is_empty() {
            return;
        }
        let Some(body) = prepared.provider_request.body_json.as_object_mut() else {
            return;
        };
        // Peel the extra body's output_config (claude-code `delete _i.output_config`).
        let extra_output_config = extra.remove("output_config");
        // Capture the codec-computed top-level `speed` so the generic extra spread
        // can't clobber it: claude-code spreads the extra body (`...Vs`) BEFORE the
        // computed `...ze!==void 0&&{speed:ze}`, so a computed speed wins over an
        // extra one. When no speed was computed (`ze` undefined) the spread is
        // skipped and an extra-body `speed` survives.
        let computed_speed = body.get("speed").cloned();
        // Spread the remaining keys first (claude-code `...va, ..._i`).
        for (k, v) in extra {
            body.insert(k, v);
        }
        // Re-apply the computed speed on top (computed wins, position preserved).
        if let Some(speed) = computed_speed {
            body.insert("speed".to_string(), speed);
        }
        // Then merge/emit output_config last (claude-code `...{output_config:Ii}`):
        // start from the extra body's copy, overlay the computed one (computed wins).
        if let Some(serde_json::Value::Object(extra_oc)) = extra_output_config {
            let mut merged = extra_oc;
            if let Some(serde_json::Value::Object(computed)) = body.get("output_config") {
                for (k, v) in computed {
                    merged.insert(k.clone(), v.clone());
                }
            }
            if merged.is_empty() {
                body.remove("output_config");
            } else {
                body.insert(
                    "output_config".to_string(),
                    serde_json::Value::Object(merged),
                );
            }
        }
    }

    /// (cc 2.1.219) Opt-in `anthropic-dispatch-id: v2s` resolver —
    /// `Mg.CLAUDE_CODE_DISPATCH_V2S ?? Ke("tengu_cedar_lattice", !1)`.
    ///
    /// `Mg` is `oMl(cfh, null)` with `cfh = {}` (2.1.220 @226176806): the
    /// getter loop over `Object.entries({})` never runs and the prototype is
    /// `null`, so `Mg.<ANY>` reads `undefined` and the nullish `??` ALWAYS
    /// falls through to the flag. `CLAUDE_CODE_DISPATCH_V2S` is therefore not
    /// a live switch in the shipped binary — the flag is the sole gate, and
    /// reading the env here would let the port enable (or disable) a header
    /// the oracle cannot.
    fn dispatch_v2s_opt_in() -> bool {
        // `::telemetry` = the flags crate (the unqualified name is the
        // local `crate::model::telemetry` emit module).
        ::telemetry::flag_bool("tengu_cedar_lattice", false)
    }

    /// (cc 2.1.219) `Ooe()` = `xn()==="firstParty" && Yd()` (2.1.220
    /// @227683488) — the header's provider gate has TWO halves.
    ///
    /// `xn()` is env-only (bedrock/foundry/…/else `"firstParty"`), which the
    /// port models as [`crate::ProviderId::AnthropicFirstParty`]. `Yd()` is the
    /// BASE-URL half and is independent of it: `_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL`
    /// (`Pe.bool()`, i.e. `1|true|yes|on`) short-circuits to true, else
    /// `d6r()` requires the configured base to be unset or to parse to host
    /// `api.anthropic.com` (`T1e`; an unparseable URL is false). A first-party
    /// route pointed at an enterprise gateway must get NO header.
    fn dispatch_first_party(prepared: &crate::PreparedLlmCall) -> bool {
        if prepared.route.resolved_route.provider_id != crate::ProviderId::AnthropicFirstParty {
            return false;
        }
        if cache_env_truthy("_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL") {
            return true;
        }
        // The port always resolves a concrete base (default
        // `https://api.anthropic.com`), so the oracle's "unset ⇒ true" arm is
        // the default host itself. `URL.host` keeps a non-default port, which
        // is what `url::Url::port()` reports.
        url::Url::parse(&prepared.provider_request.url).is_ok_and(|u| {
            u.host_str().is_some_and(|h| match u.port() {
                Some(p) => format!("{h}:{p}") == FIRST_PARTY_API_HOST,
                None => h == FIRST_PARTY_API_HOST,
            })
        })
    }

    /// (cc 2.1.219) Add `anthropic-dispatch-id: v2s` (`pi[S8s]=Vtp`) to an
    /// attempt: `!Kt && fB(i.querySource)!=="auxiliary" && Ooe() && (flag)`
    /// (2.1.220 @237553955), in that order.
    fn apply_dispatch_header(prepared: &mut crate::PreparedLlmCall, state: DispatchHeaderState) {
        if state.fallen_back
            || state.auxiliary
            || !Self::dispatch_first_party(prepared)
            || !Self::dispatch_v2s_opt_in()
        {
            return;
        }
        prepared
            .provider_request
            .headers
            .insert(DISPATCH_ID_HEADER.to_string(), DISPATCH_ID_V2S.to_string());
        // `w(`[dispatch] sent ${S8s}=${Vtp}`)` — default (debug) log level.
        tracing::debug!("[dispatch] sent {DISPATCH_ID_HEADER}={DISPATCH_ID_V2S}");
    }

    /// (cc 2.1.219) Dispatch-header degradation check: when THIS attempt
    /// carried the header and the failure is an HTTP 5xx or a connection
    /// error, latch the per-query fallback (`Kt=!0`) and report
    /// `(reason, status)` for the `tengu_dispatch_header_fallback` telemetry —
    /// the caller then retries immediately WITHOUT consuming retry budget
    /// (the oracle's `"retry:dispatch-header-strip"` step). `None` ⇒ not a
    /// dispatch-header failure; normal retry classification applies.
    fn note_dispatch_header_failure(
        state: &mut DispatchHeaderState,
        prepared: &crate::PreparedLlmCall,
        err: &LlmError,
    ) -> Option<(&'static str, Option<u16>)> {
        let carried = prepared
            .provider_request
            .headers
            .contains_key(DISPATCH_ID_HEADER);
        Self::note_dispatch_header_failure_carried(state, carried, err)
    }

    /// [`Self::note_dispatch_header_failure`] twin for callers whose prepared
    /// call was already moved (the stream open path) — `carried` is captured
    /// before the move.
    fn note_dispatch_header_failure_carried(
        state: &mut DispatchHeaderState,
        carried: bool,
        err: &LlmError,
    ) -> Option<(&'static str, Option<u16>)> {
        // `no && !Kt`.
        if !carried || state.fallen_back {
            return None;
        }
        let http_5xx = Self::status_of(err).filter(|s| *s >= 500);
        if http_5xx.is_none() && !Self::is_dispatch_conn_err(err) {
            return None;
        }
        state.fallen_back = true;
        // Byte template: `[dispatch] ${Nu?`HTTP ${ss}`:"connection error"}
        // with ${S8s}; retrying without it` at level warn.
        let what = http_5xx.map_or_else(|| "connection error".to_string(), |s| format!("HTTP {s}"));
        tracing::warn!("[dispatch] {what} with {DISPATCH_ID_HEADER}; retrying without it");
        Some((
            if http_5xx.is_some() {
                "5xx"
            } else {
                "conn_err"
            },
            http_5xx,
        ))
    }

    /// (cc 2.1.219) The oracle's SECOND dispatch-fallback arm (2.1.220
    /// @237577972): `if(no&&!Kt&&Bs!==null&&!oc)` — a CONNECTION error raised
    /// while reading the stream body, before the first stream event was
    /// yielded. It latches `Kt`, emits `tengu_dispatch_header_fallback` with
    /// `reason:"body_phase"`/`status:"none"` and `continue e`s, so unlike the
    /// stale-connection arm right below it (`ko<jt`) it costs no retry budget.
    ///
    /// `!oc` ("nothing yielded yet") is why the port can only take this arm on
    /// a ONE-FRAME lookahead in [`Self::drive_stream`]: once the stream is
    /// handed to the caller there is no attempt loop left to `continue`.
    /// `x2(Qo)` classifies node connection errors, so the watchdog's
    /// [`LlmError::StreamInterrupted`] is correctly excluded.
    fn note_dispatch_body_phase_failure(
        state: &mut DispatchHeaderState,
        carried: bool,
        err: &LlmError,
    ) -> bool {
        if !carried || state.fallen_back || !Self::is_dispatch_conn_err(err) {
            return false;
        }
        state.fallen_back = true;
        // Byte template: `[dispatch] Stream connection error (${Bs.code}) with
        // anthropic-dispatch-id before first event; retrying without it`. The
        // port has no node `errno` string, so the transport message stands in
        // for `Bs.code`.
        tracing::warn!(
            "[dispatch] Stream connection error ({err}) with {DISPATCH_ID_HEADER} before first event; retrying without it"
        );
        true
    }

    /// `x2()` — the connection-error classification both dispatch-fallback arms
    /// key on (a TLS failure is a connect failure, not a provider response).
    fn is_dispatch_conn_err(err: &LlmError) -> bool {
        matches!(err, LlmError::Transport { .. } | LlmError::TlsCert { .. })
    }

    fn inject_headers(
        &self,
        prepared: &mut crate::PreparedLlmCall,
        request_id: &str,
        dispatch: DispatchHeaderState,
    ) {
        // Provider-specific tool-search beta: first-party/Foundry use
        // advanced-tool-use, Vertex uses tool-search-tool, and Bedrock carries
        // tool-search-tool in the request body's anthropic_beta array.
        let beta_provider = match prepared.route.protocol {
            crate::ProtocolFamily::AnthropicMessages
                if Self::direct_anthropic_api_route(prepared) =>
            {
                Some(Provider::Anthropic)
            }
            crate::ProtocolFamily::FoundryClaude => Some(Provider::Anthropic),
            crate::ProtocolFamily::VertexClaude => Some(Provider::Vertex),
            _ => None,
        };
        if let Some(provider) = beta_provider {
            let ctx = self.beta_context(prepared);
            let custom_betas = self.custom_cli_betas(prepared);
            apply_beta_header_with_auth_and_custom(
                &mut prepared.provider_request,
                provider,
                Endpoint::MessagesCreate,
                &ctx,
                self.effective_subscriber().is_subscriber,
                &custom_betas,
            );
        }
        // User-Agent (Task 3) — provider-aware (see apply_user_agent).
        self.apply_user_agent(prepared);
        // Client-traceable request id (matches api-client header name).
        prepared
            .provider_request
            .headers
            .insert("x-request-id".to_string(), request_id.to_string());
        // (cc 2.1.219) opt-in dispatch-routing header (see apply_dispatch_header).
        Self::apply_dispatch_header(prepared, dispatch);
        // CLAUDE_CODE_EXTRA_BODY merge — after the beta header is computed from the
        // pre-merge body (claude-code `B0t` spread; 2.1.207).
        self.merge_extra_body(prepared);
        // Final resolved-route guard. `CLAUDE_CODE_EXTRA_BODY` is merged above,
        // so this must run last to prevent it from reintroducing first-party
        // speed/beta fields on custom or unsupported routes.
        Self::enforce_fast_route(prepared);
    }

    /// Same as [`inject_headers`] but for the streaming endpoint.
    fn inject_stream_headers(
        &self,
        prepared: &mut crate::PreparedLlmCall,
        request_id: &str,
        dispatch: DispatchHeaderState,
    ) {
        let beta_provider = match prepared.route.protocol {
            crate::ProtocolFamily::AnthropicMessages
                if Self::direct_anthropic_api_route(prepared) =>
            {
                Some(Provider::Anthropic)
            }
            crate::ProtocolFamily::FoundryClaude => Some(Provider::Anthropic),
            crate::ProtocolFamily::VertexClaude => Some(Provider::Vertex),
            _ => None,
        };
        if let Some(provider) = beta_provider {
            let ctx = self.beta_context(prepared);
            let custom_betas = self.custom_cli_betas(prepared);
            apply_beta_header_with_auth_and_custom(
                &mut prepared.provider_request,
                provider,
                Endpoint::MessagesCreateStream,
                &ctx,
                self.effective_subscriber().is_subscriber,
                &custom_betas,
            );
        }
        self.apply_user_agent(prepared);
        prepared
            .provider_request
            .headers
            .insert("x-request-id".to_string(), request_id.to_string());
        // (cc 2.1.219) opt-in dispatch-routing header (see apply_dispatch_header).
        Self::apply_dispatch_header(prepared, dispatch);
        // CLAUDE_CODE_EXTRA_BODY merge — after the beta header is computed from the
        // pre-merge body (claude-code `B0t` spread; 2.1.207).
        self.merge_extra_body(prepared);
        Self::enforce_fast_route(prepared);
    }

    // ── 429 retry-after resolution (reset ladder) ─────────────────────────────

    /// Resolve the 429 retry delay using the server-sent reset ladder:
    /// `retry-after` → `anthropic-ratelimit-unified-reset` → `anthropic-ratelimit-requests-reset` → 1 s.
    fn resolve_retry_after(
        headers: &std::collections::BTreeMap<String, String>,
    ) -> std::time::Duration {
        // Convert BTreeMap to vec for parse helpers.
        let hvec: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let now = std::time::SystemTime::now();

        // 1. Retry-After (RFC 7231 delta-seconds).
        if let Some(d) = parse_retry_after(&hvec) {
            return d;
        }
        // 2. anthropic-ratelimit-unified-reset (epoch seconds).
        if let Some(d) = parse_unified_reset(&hvec, now) {
            return d;
        }
        // 3. anthropic-ratelimit-requests-reset (ISO8601 UTC).
        if let Some(d) = crate::model::rate_limit::parse_anthropic_ratelimit_reset(&hvec, now) {
            return d;
        }
        // 4. OpenAI x-ratelimit-reset-requests / -tokens (Go-duration). Without
        // this a non-Anthropic 429 falls to the 1s blind wait and hammers the
        // still-exhausted window. Provider-neutral: only matches when present.
        if let Some(d) = crate::model::rate_limit::parse_openai_reset(&hvec) {
            return d;
        }
        // 5. Fallback: 1 s.
        std::time::Duration::from_secs(1)
    }

    // ── error_kind label (for telemetry) ─────────────────────────────────────

    /// Stable `error_kind` label for `emit_failed`.
    ///
    /// Strings are **spec-locked** to the originals from
    /// `api-client/src/anthropic.rs::error_kind` (`:1144`) to keep telemetry
    /// dashboards consistent across the api-client and llm-runtime codepaths.
    ///
    /// Mapping table (api-client variant → llm-runtime variant → label):
    ///
    /// | api-client             | LlmError                           | label            |
    /// |------------------------|------------------------------------|------------------|
    /// | `Unauthorized`         | `Authentication \| PermissionDenied` | `"unauthorized"` |
    /// | `Server`               | `ProviderInternal`                 | `"server"`       |
    /// | `Http`                 | `Transport`                        | `"http"`         |
    /// | `MalformedStream`      | `StreamInterrupted`                | `"malformed_stream"` |
    /// | `Overloaded`           | `Overloaded`                       | `"overloaded"`   |
    /// | `RateLimited`          | `RateLimited`                      | `"rate_limited"` |
    /// | `PromptTooLong`        | `ContextOverflow`                  | `"prompt_too_long"` |
    /// | *(llm-runtime only)*    | `InvalidRequest`                   | `"invalid_request"` |
    /// | *(llm-runtime only)*    | `QuotaExceeded`                    | `"quota_exceeded"` |
    /// | *(llm-runtime only)*    | `ModelUnavailable`                 | `"model_unavailable"` |
    /// | *(llm-runtime only)*    | `CostUnavailable`                  | `"cost_unavailable"` |
    /// | *(llm-runtime only)*    | `UnsupportedCapability`            | `"unsupported_capability"` |
    fn error_kind(err: &LlmError) -> &'static str {
        match err {
            // "unauthorized" — api-client `Unauthorized(_) => "unauthorized"` (:1153).
            // A dead OAuth session is an auth failure like any other here; it
            // differs only in the copy the orchestrator renders for it.
            LlmError::Authentication { .. }
            | LlmError::OAuthRefreshDead
            | LlmError::PermissionDenied { .. } => "unauthorized",
            // "server" — api-client `Server { .. } => "server"` (:1157)
            LlmError::ProviderInternal => "server",
            // "http" — api-client `Http(_) => "http"` (:1146)
            // A timeout is still an HTTP-layer failure for telemetry.
            LlmError::Transport { .. } | LlmError::TransportTimeout { .. } => "http",
            // "ssl_cert_error" — 2.1.201 classifier distinguishes SSL/cert
            // transport failures (`if(JF(e)?.isSSLError)return"ssl_cert_error"`).
            LlmError::TlsCert { .. } => "ssl_cert_error",
            // "malformed_stream" — api-client `MalformedStream(_) => "malformed_stream"` (:1155)
            LlmError::StreamInterrupted { .. } => "malformed_stream",
            LlmError::MalformedToolInput { .. } => "malformed_tool_input",
            // "overloaded" — api-client `Overloaded { .. } => "overloaded"` (:1149)
            LlmError::Overloaded { .. } => "overloaded",
            // "rate_limited" — api-client `RateLimited { .. } => "rate_limited"` (:1148)
            LlmError::RateLimited { .. } => "rate_limited",
            // "prompt_too_long" — api-client `PromptTooLong { .. } => "prompt_too_long"` (:1147)
            LlmError::ContextOverflow { .. } => "prompt_too_long",
            // "request_too_large" — 2.1.212 error classifier: a 413 whose message
            // lacks "context window" → `"request_too_large"` (distinct from the
            // context-window `"prompt_too_long"` above).
            LlmError::RequestTooLarge => "request_too_large",
            // llm-runtime-only classes — no api-client analogue; use descriptive names.
            LlmError::InvalidRequest { .. } => "invalid_request",
            LlmError::QuotaExceeded => "quota_exceeded",
            LlmError::ModelUnavailable => "model_unavailable",
            LlmError::CostUnavailable { .. } => "cost_unavailable",
            LlmError::MediaDelegationUnavailable { .. }
            | LlmError::MediaDelegationPartial { .. } => "media_delegation_unavailable",
            LlmError::UnsupportedCapability { .. } => "unsupported_capability",
        }
    }

    /// Return the most recently observed 2xx rate-limit header snapshot, if any.
    ///
    /// Populated on every successful response from `drive_non_stream` and on the
    /// connect-success path of `drive_stream`.  `None` until the first successful
    /// response is received.
    ///
    /// TUI wiring note: no existing `OrchestratorHandle` surface maps naturally
    /// to per-request rate-limit metadata.  Callers that need this should hold an
    /// `Arc<ApiService>` and call this method directly.  A future task can
    /// wire it through the handle if needed.
    pub fn last_rate_limit_info(&self) -> Option<RateLimitInfo> {
        self.last_rate_limit.lock().unwrap().clone()
    }

    /// The request id of the most recently recorded response. Backs the
    /// `OrchestratorApiClient::last_request_id` trait override (used to stamp the
    /// persisted assistant line's top-level `requestId`). Returns the server id
    /// when present, else the client-generated fallback (see
    /// [`Self::last_request_id_origin`]). `None` until the first recorded
    /// response (or when both are absent).
    #[must_use]
    pub fn last_request_id(&self) -> Option<String> {
        self.last_request_id
            .lock()
            .unwrap()
            .as_ref()
            .map(|(value, _origin)| value.clone())
    }

    /// Origin of the value returned by [`Self::last_request_id`] —
    /// [`RequestIdOrigin::Server`] when it came from a provider response header,
    /// [`RequestIdOrigin::Client`] when it is the client-generated fallback.
    /// `None` when no request id has been recorded.
    #[must_use]
    pub fn last_request_id_origin(&self) -> Option<RequestIdOrigin> {
        self.last_request_id
            .lock()
            .unwrap()
            .as_ref()
            .map(|(_value, origin)| *origin)
    }

    /// Number of budget-consuming retry attempts the most recent drive performed
    /// before its terminal outcome. Backs the `last_retry_count` trait overrides
    /// (both `OrchestratorApiClient` and `StreamingApiClient`). `0` until the
    /// first drive.
    #[must_use]
    pub fn last_retry_count(&self) -> u32 {
        *self.last_retry_count.lock().unwrap()
    }

    fn thinking_recovery_scope(&self) -> crate::thinking_scope::ThinkingRecoveryScope {
        crate::thinking_scope::current().unwrap_or_else(|| self.thinking_recovery.clone())
    }

    /// Recovery status for the owning query (legacy direct callers use a private scope).
    #[must_use]
    pub fn thinking_signature_stripped(&self) -> bool {
        self.thinking_recovery_scope().stripped()
    }

    /// Compatibility arm: capture only the next request's historical identities.
    pub fn set_thinking_signature_stripped(&self, stripped: bool) {
        self.thinking_recovery_scope().arm(stripped);
    }

    pub fn thinking_stripped_messages(
        &self,
    ) -> std::collections::HashMap<protocol::MessageId, usize> {
        self.thinking_recovery_scope().messages()
    }

    pub fn set_thinking_stripped_messages(
        &self,
        messages: std::collections::HashMap<protocol::MessageId, usize>,
    ) {
        self.thinking_recovery_scope().merge(messages);
    }

    /// Strip thinking blocks after a thinking-signature 400 on any provider.
    /// Returns `true` when the caller should retry immediately.
    async fn handle_thinking_signature_strip(&self, req: &mut crate::LlmRequest) -> bool {
        let (signed, unsigned) =
            crate::model::thinking_signature::count_thinking_signature_blocks(&req.messages);
        if !crate::model::thinking_signature::strip_thinking_blocks_for_signature_recovery(
            &mut req.messages,
        ) {
            return false;
        }
        tracing::warn!(
            "[thinking] server rejected a thinking block; stripping all thinking blocks and retrying."
        );
        telemetry::emit_thinking_signature_strip_retry(
            &self.analytics,
            req.query_source.as_deref(),
            &req.model,
            signed,
            unsigned,
        )
        .await;
        let scope = req
            .thinking_recovery_scope
            .clone()
            .unwrap_or_else(|| self.thinking_recovery_scope());
        scope.rejected(
            req.thinking_source_message_ids
                .iter()
                .map(|id| (*id, 0))
                .collect(),
        );
        scope.persist().await;
        true
    }

    /// The most recently observed RAW per-window utilization snapshot. Backs the
    /// `OrchestratorApiClient::last_raw_utilization` trait override. `None` until
    /// the first recorded response.
    #[must_use]
    pub fn last_raw_utilization(&self) -> Option<RawUtilization> {
        *self.last_raw_utilization.lock().unwrap()
    }

    /// The user-facing copy composed from the most recent 429 **error**
    /// response. Backs the
    /// `OrchestratorApiClient::last_rate_limit_error_message` trait override (the
    /// orchestrator's terminal-429 re-map, claude-code `errors.ts:480-524`).
    /// `None` when neither unified Anthropic limits nor an OpenRouter free-model
    /// response supplied actionable context.
    #[must_use]
    pub fn last_rate_limit_error_message(&self) -> Option<String> {
        self.last_429_message.lock().unwrap().clone()
    }

    /// Consume the pending near-limit wrap-up hint once.
    ///
    /// The dedupe window key intentionally survives the consume: once a
    /// subagent has seen the hint for a five-hour window, later responses in
    /// that same window must not re-arm it. A later reset starts a new window.
    #[must_use]
    pub fn consume_pending_near_limit_wrap_up_hint(&self) -> bool {
        let mut pending = self.pending_near_limit_wrap_up_hint.lock().unwrap();
        if !*pending {
            return false;
        }
        *pending = false;
        true
    }

    /// Parse rate-limit headers from a 2xx response and update the cached snapshot.
    ///
    /// Emits a `tracing::warn!` when the overage status indicates the account is
    /// at or near exhaustion (`overage_status == "rejected"` or `"allowed_warning"`).
    /// Wall-clock milliseconds since the Unix epoch — the record timestamp for
    /// the rate-limit monotonic guard (binary `Date.now()`).
    fn now_ms() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    }

    /// Monotonic rate-limit record guard — ports the binary's `Bha`/`Nha`
    /// (@210953352): returns `true` (STALE ⇒ caller skips the snapshot update)
    /// when `ts_ms` is OLDER than the last recorded timestamp; otherwise
    /// records `ts_ms` and returns `false`. Prevents an out-of-order (older)
    /// parallel response from overwriting a newer rate-limit snapshot — the
    /// 2.1.196 flicker fix. Under normal monotonic wall-clock operation this
    /// never drops, so production behaviour is unchanged.
    fn rate_limit_record_stale(&self, ts_ms: u128) -> bool {
        let mut guard = self.last_rate_limit_record_ts_ms.lock().unwrap();
        match *guard {
            Some(prev) if ts_ms < prev => true,
            _ => {
                *guard = Some(ts_ms);
                false
            }
        }
    }

    fn record_rate_limit_from_headers(
        &self,
        headers: &std::collections::BTreeMap<String, String>,
        client_request_id: &str,
    ) {
        self.record_rate_limit_from_headers_at(headers, client_request_id, Self::now_ms());
    }

    fn record_rate_limit_from_headers_at(
        &self,
        headers: &std::collections::BTreeMap<String, String>,
        client_request_id: &str,
        ts_ms: u128,
    ) {
        let hvec: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        // Capture the request id on every recorded response — the SDK's
        // `response._request_id`, which claude-code persists as the assistant
        // line's top-level `requestId`. Provider-aware: tries each provider's
        // canonical id header (Anthropic `request-id`, OpenAI `x-request-id`,
        // Azure `apim-request-id`, Bedrock `x-amzn-requestid`, …) via the shared
        // transport extractor. When the provider returns none, fall back to the
        // client-generated `x-request-id` we sent (origin = Client) so the field
        // is never blank — this is correlation-only and NOT valid for
        // provider-side lookups. `None` only when both are absent (so a stale id
        // never leaks onto a later line).
        *self.last_request_id.lock().unwrap() =
            match crate::transport_bridge::extract_response_request_id(headers) {
                Some(server_id) => Some((server_id, RequestIdOrigin::Server)),
                None if !client_request_id.is_empty() => {
                    tracing::debug!(
                        client_request_id,
                        "no provider request-id header on response; \
                         falling back to client-generated id (correlation-only)"
                    );
                    Some((client_request_id.to_string(), RequestIdOrigin::Client))
                }
                None => None,
            };
        // Monotonic guard (binary `Bha`/`Nha`): a response whose record
        // timestamp is OLDER than the last recorded one is STALE — skip the
        // rate-limit snapshot updates so an out-of-order parallel response can
        // never flip the warning off (2.1.196 flicker fix). request-id capture
        // above and the pending-429 bookkeeping below stay unconditional.
        let stale = self.rate_limit_record_stale(ts_ms);
        // Task 2 (llm-runtime future-work batch 5): track the raw per-window
        // snapshot on EVERY recorded (non-stale) headers pass — `rawUtilization
        // = extractRawUtilization(headersToUse)` (claudeAiLimits.ts:476), NOT
        // gated on `has_unified_headers()` like the limits snapshot below.
        let raw = RawUtilization::from_headers(&hvec);
        if !stale {
            *self.last_raw_utilization.lock().unwrap() = Some(raw);
        }
        let info = RateLimitInfo::from_headers(&hvec);
        if !stale {
            self.update_near_limit_wrap_up_state(&info, raw, ts_ms);
        }
        if !stale && info.has_unified_headers() {
            // Warn when the account is near or at exhaustion.
            match info.overage_status.as_deref() {
                Some("rejected") => {
                    tracing::warn!(
                        overage_status = "rejected",
                        rate_limit_type = ?info.rate_limit_type,
                        "Rate limit: overage rejected — usage limit exhausted"
                    );
                }
                Some("allowed_warning") => {
                    tracing::warn!(
                        overage_status = "allowed_warning",
                        rate_limit_type = ?info.rate_limit_type,
                        "Rate limit: overage warning — nearing usage limit"
                    );
                }
                _ => {}
            }
            *self.last_rate_limit.lock().unwrap() = Some(info);
        }
        // Task 6 (batch 5): a successful response supersedes any cached 429
        // limits copy — the slot always reflects the most recent response.
        *self.last_429_message.lock().unwrap() = None;
        // B6-T1: a success also discards any staged-but-unpromoted 429 from an
        // earlier retried attempt (TS resets module state to the success's
        // `status`, never leaving a stale `rejected` behind).
        self.clear_pending_429();
    }

    /// Whether the live subscription snapshot is a Pro or Enterprise plan —
    /// the `getSubscriptionType() === 'pro' || 'enterprise'` predicate gating
    /// the `seven_day_sonnet` wording (claude-code
    /// `rateLimitMessages.ts:176-181`).
    ///
    /// Reads the live [`Self::subscription`] slot directly (the snapshot
    /// carries `subscription_type`; [`SubscriberState`] does not). With no
    /// resolved snapshot, falls back to the build-time enterprise bit —
    /// `pro` is unknowable pre-snapshot, matching an unresolved
    /// `getSubscriptionType()` evaluating to neither.
    fn is_pro_or_enterprise(&self) -> bool {
        if let Some(slot) = &self.subscription {
            if let Ok(guard) = slot.read() {
                if let Some(snap) = guard.as_ref() {
                    return matches!(
                        snap.subscription_type.as_deref(),
                        Some("pro" | "enterprise")
                    );
                }
            }
        }
        self.subscriber.is_enterprise
    }

    /// Record the unified rate-limit context from a 429 **error** response —
    /// the Rust seam for claude-code `errors.ts:471-524`, which extracts the
    /// unified headers from the error itself when a turn dies on a 429
    /// (success-path recording never sees them).
    ///
    /// When the 429 carries unified headers (the
    /// `if (rateLimitType || overageStatus)` gate, `errors.ts:480`):
    /// 1. the forced-`rejected` limits view is STAGED in [`Self::pending_429`]
    ///    (TS updates its limits state from the error headers with
    ///    `status: 'rejected'`, `errors.ts:482-516`), and
    /// 2. the composed `getRateLimitErrorMessage` copy is cached for the
    ///    orchestrator's terminal-error re-map
    ///    (`OrchestratorError::RateLimitRejected`). A headerless OpenRouter
    ///    free-model 429 instead caches its `error.message` plus retry/model
    ///    switching guidance.
    ///
    /// The RAW per-window utilization is parsed from the SAME error headers
    /// UNCONDITIONALLY — `extractRawUtilization(headersToUse)` runs for ANY
    /// error headers (claudeAiLimits.ts:500), independent of the limits gate —
    /// and staged alongside.
    ///
    /// The staged slot is PROMOTED into the live `last_rate_limit` /
    /// `last_raw_utilization` caches only when the retry loop declares the
    /// error TERMINAL (via [`Self::promote_pending_429`]), mirroring the TS
    /// terminal catch handler `extractQuotaStatusFromError`
    /// (claudeAiLimits.ts:487) — NOT on a retried attempt that later recovers.
    /// This closes the prior per-attempt-write divergence (a
    /// retried-then-recovered 429 no longer plants a rejected snapshot).
    ///
    /// The staged slot is written on EVERY non-stale 429, even when both the
    /// gated limits view and the raw windows are empty/default. That preserves
    /// Claude Code's unconditional `rawUtilization = extractRawUtilization(...)`
    /// assignment on terminal 429s, allowing a headerless rejection to clear a
    /// previously non-empty raw snapshot.
    /// `body` is the 429's parsed JSON error body, when available — Task 5
    /// threads it through to [`crate::RateLimitInfo::from_429_error`] so the
    /// `Nqi(e)` `credits_required` / body-derived `overage_disabled_reason`
    /// can be recovered from the error BODY (not just the response headers).
    fn record_rate_limit_from_429(
        &self,
        headers: &std::collections::BTreeMap<String, String>,
        body: Option<&serde_json::Value>,
        model: &str,
    ) {
        self.record_rate_limit_from_429_at(headers, body, model, Self::now_ms());
    }

    fn record_rate_limit_from_429_at(
        &self,
        headers: &std::collections::BTreeMap<String, String>,
        body: Option<&serde_json::Value>,
        model: &str,
        ts_ms: u128,
    ) {
        let hvec: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        // `extractRawUtilization(headersToUse)` (claudeAiLimits.ts:500) runs
        // for ANY error headers, independent of the limits gate below.
        let raw = RawUtilization::from_headers(&hvec);
        let info = RateLimitInfo::from_429_error(&hvec, body);

        // MESSAGE composition ports errors.ts:482-516 (a LOCAL limits object
        // built from the error headers) — kept verbatim; it is NOT staged and
        // is set per-attempt because the terminal error re-map reads it
        // directly (the most-recent-429 slot, cleared on success).
        let composed = info.as_ref().map(|info| {
            // `formatResetTime(…, true)` analogue for both reset headers
            // (`rateLimitMessages.ts:144-148`), formatted at error time.
            let formatted = formatted_reset_times_from_headers(&hvec);
            rate_limit_error_message(
                info,
                &formatted.as_reset_times(),
                SubscriptionContext {
                    is_pro_or_enterprise: self.is_pro_or_enterprise(),
                },
            )
        });
        *self.last_429_message.lock().unwrap() = composed.flatten().or_else(|| {
            is_free_tier_model(model).then(|| openrouter_free_rate_limit_message(body))
        });

        // Monotonic guard (binary `Bha`/`Nha`): a stale (out-of-order) 429 must
        // not stage a snapshot that could later PROMOTE over a newer response's
        // state. The composed message above is per-attempt (most-recent-429)
        // and stays unconditional; only the promotable staged slot is gated.
        if self.rate_limit_record_stale(ts_ms) {
            return;
        }
        // Stage EVERY non-stale 429 so the terminal promote can also write the
        // EMPTY raw snapshot and thereby clear stale raw-window state.
        *self.pending_429.lock().unwrap() = Some(Pending429 { info, raw });
    }

    /// Discard any staged 429 snapshot. Called at drive entry and on success so
    /// a retried-then-recovered 429 (or a non-RateLimited terminal that leaves a
    /// staged slot) cannot promote into a LATER drive. Defensive backstop: the
    /// active cross-drive isolation is the per-attempt record stage-or-clear in
    /// [`Self::record_rate_limit_from_429`] (a fresh attempt always overwrites
    /// or clears the slot before the terminal promote runs); this guards against
    /// a future refactor that adds a promote-without-record path.
    fn clear_pending_429(&self) {
        *self.pending_429.lock().expect("pending_429 poisoned") = None;
    }

    /// Promote a staged 429 snapshot into the live caches — the Rust analogue
    /// of the TS terminal catch handler `extractQuotaStatusFromError`
    /// (claudeAiLimits.ts:487-515), which updates module state only when the
    /// turn DIES on a 429.
    ///
    /// `.take()`s [`Self::pending_429`]; when `Some`:
    /// - `info` `Some` → the forced-`rejected` limits snapshot replaces
    ///   `last_rate_limit`;
    /// - the raw per-window snapshot (including the EMPTY `{}` snapshot)
    ///   replaces `last_raw_utilization`.
    ///
    /// The orchestrator may still choose not to emit the EMPTY snapshot on its
    /// event stream, but the client cache preserves it so stale state can be
    /// cleared at the next seam that wants the exact current snapshot.
    /// Idempotent via `.take()`: a second call after promotion is a no-op.
    fn promote_pending_429(&self) {
        let Some(pending) = self.pending_429.lock().unwrap().take() else {
            return;
        };
        if let Some(info) = pending.info {
            *self.last_rate_limit.lock().unwrap() = Some(info);
        }
        *self.last_raw_utilization.lock().unwrap() = Some(pending.raw);
    }

    fn near_limit_wrap_up_threshold(&self) -> f64 {
        if let Some(slot) = &self.subscription {
            if let Ok(guard) = slot.read() {
                if let Some(snapshot) = guard.as_ref() {
                    return match snapshot.rate_limit_tier.as_deref() {
                        Some("default_claude_max_5x") => NEAR_LIMIT_WRAP_UP_MAX5X_THRESHOLD,
                        Some("default_claude_max_20x") => NEAR_LIMIT_WRAP_UP_MAX20X_THRESHOLD,
                        _ => NEAR_LIMIT_WRAP_UP_DEFAULT_THRESHOLD,
                    };
                }
            }
        }
        // The static subscriber seed carries only subscriber/enterprise bits,
        // not `getRateLimitTier()`. The oracle's `OZt(mw())` therefore falls
        // through to the default threshold until the live tier snapshot lands.
        NEAR_LIMIT_WRAP_UP_DEFAULT_THRESHOLD
    }

    fn update_near_limit_wrap_up_state(
        &self,
        info: &RateLimitInfo,
        raw: RawUtilization,
        _observation_ts_ms: u128,
    ) {
        // Oracle 2.1.252 `extractQuotaStatusFromHeaders` uses `Date.now()` for
        // expiry/future checks, independently of the observation timestamp
        // used by the stale-response guard. `five_hour` names the quota bucket;
        // it is not a "reset within five hours" duration predicate.
        let now_secs = u64::try_from(Self::now_ms() / 1000).unwrap_or(u64::MAX);
        let mut pending = self.pending_near_limit_wrap_up_hint.lock().unwrap();
        let mut window_key = self.near_limit_wrap_up_window_key.lock().unwrap();
        if window_key.is_some_and(|reset| reset < now_secs) {
            *pending = false;
            *window_key = None;
        }

        let Some(window) = raw.five_hour else {
            return;
        };
        if !window.utilization.is_finite()
            || window.resets_at <= now_secs
            || window.utilization < self.near_limit_wrap_up_threshold()
        {
            return;
        }

        // Oracle also requires `!aM()` here, where `aM()` is its separate
        // low-priority/slow-mode controller. LingXi does not implement that
        // subsystem, so every representable runtime state is the inactive
        // branch. If slow mode is added, it must suppress arming here and make
        // `consume_pending_near_limit_wrap_up_hint` clear-then-return-false.

        // Extra usage suppresses an armed hint but deliberately preserves the
        // reset key. If overage later turns off in the same window, the hint
        // must not re-arm (`nearLimitWrapUpWindowKey !== resets_at`).
        if matches!(
            info.overage_status.as_deref(),
            Some("allowed" | "allowed_warning")
        ) {
            *pending = false;
            return;
        }

        if *window_key != Some(window.resets_at) {
            *window_key = Some(window.resets_at);
            *pending = true;
        }
    }

    /// HTTP status code approximation for `emit_failed` (best-effort: only the
    /// variants that carry an HTTP status are non-None).
    fn status_of(err: &LlmError) -> Option<u16> {
        match err {
            LlmError::Authentication { .. } | LlmError::PermissionDenied { .. } => Some(401),
            LlmError::InvalidRequest { .. } | LlmError::ContextOverflow { .. } => Some(400),
            LlmError::RequestTooLarge => Some(413),
            LlmError::RateLimited { .. } | LlmError::QuotaExceeded => Some(429),
            LlmError::ModelUnavailable => Some(404),
            LlmError::ProviderInternal => Some(500),
            LlmError::Overloaded { .. } => Some(529),
            // No HTTP status: the request never reached the model API. The
            // refresh call to the IdP failed locally, so inventing a 401 here
            // would put a status in the transcript that no server ever sent.
            LlmError::OAuthRefreshDead
            | LlmError::Transport { .. }
            | LlmError::TransportTimeout { .. }
            | LlmError::TlsCert { .. }
            | LlmError::StreamInterrupted { .. }
            | LlmError::MalformedToolInput { .. }
            | LlmError::CostUnavailable { .. }
            | LlmError::UnsupportedCapability { .. }
            | LlmError::MediaDelegationUnavailable { .. }
            | LlmError::MediaDelegationPartial { .. } => None,
        }
    }

    /// claude-code `YNd`: a `speed:"fast"` request whose account/model does not
    /// support fast mode is rejected with a 400 whose message includes
    /// `"Fast mode is not enabled"` (`e.status===400 && e.message?.includes(
    /// "Fast mode is not enabled")`). The drive loop responds by clearing
    /// `req.speed` and retrying (uncounted) rather than failing the turn
    /// (`if(T && YNd(v)){o.fastMode=!1;continue}`).
    fn is_fast_mode_not_enabled(err: &LlmError) -> bool {
        matches!(err, LlmError::InvalidRequest { message } if message.contains("Fast mode is not enabled"))
    }

    // ── Non-stream drive (Step 1 + 1b) ───────────────────────────────────────

    /// Shared non-stream retry driver. Accepts an already-built `LlmRequest` so
    /// the two call-paths (`messages_create` and `messages_create_with_fallback`)
    /// can both route here.
    ///
    /// Step 1b: before classifying a retryable 5xx, the driver checks
    /// `x-should-retry: false` — that header makes the response terminal (same
    /// behaviour as api-client `retry.rs:196`).
    ///
    /// `dispatch` classifies the query for the `anthropic-dispatch-id` gate
    /// (`fB(i.querySource)`) and carries this query's `Kt` latch.
    #[allow(clippy::too_many_lines)]
    async fn drive_non_stream(
        &self,
        req: LlmRequest,
        retry_control: RetryControl,
        dispatch: DispatchHeaderState,
    ) -> Result<LlmResponse, LlmError> {
        self.drive_non_stream_seeded_with_chain(req, retry_control, 0, &[], dispatch)
            .await
    }

    /// Non-stream retry driver with a pre-seeded `consecutive_overloaded` counter
    /// and an optional fallback chain.
    ///
    /// The seed is set to 1 when this call is a non-streaming fallback triggered by
    /// a mid-stream `LlmError::Overloaded` — mirroring TS `claude.ts:2559`
    /// (`initialConsecutive529Errors: is529Error(streamingError) ? 1 : 0`).
    ///
    /// `chain` is the ordered slice of fallback models to walk on consecutive
    /// overload events.  `retry_control` must already carry `chain[0]` as
    /// `fallback_model` (set by [`Self::messages_create_with_fallback`]); on
    /// each [`DriveStep::Fallback`] the loop advances `chain_idx` and rebuilds
    /// `retry_control` with `chain[chain_idx]` (or disables fallback when
    /// exhausted).
    #[allow(clippy::too_many_lines)]
    async fn drive_non_stream_seeded_with_chain(
        &self,
        mut req: LlmRequest,
        mut retry_control: RetryControl,
        initial_consecutive_overloaded: u8,
        chain: &[String],
        mut dispatch: DispatchHeaderState,
    ) -> Result<LlmResponse, LlmError> {
        let request_id = new_request_id();
        let started = Instant::now();
        // Sibling connections of this model's provider group, captured from the
        // FIRST prepare: once `req.profile` is pinned to one connection a later
        // resolve sees only that one, so the remaining hops must be held here.
        let mut connection_chain: Vec<crate::ConnectionHop> = Vec::new();
        let mut connection_index = 0usize;
        let mut failover = crate::FailoverTriggers::NONE;
        let mut connections_captured = false;
        telemetry::emit_started(&self.analytics, &req.model, &request_id, false).await;
        if let Some(query_source) = req.query_source.as_deref() {
            telemetry::emit_query_source(&self.analytics, &req.model, query_source).await;
        }

        // B6-T1: discard any 429 snapshot staged by a PRIOR drive (whose
        // terminal was non-rate-limited, so it never promoted) — TS module
        // state for the terminal catch handler is per-error, never carried
        // across calls.
        self.clear_pending_429();

        // Batch-5 Task 3: resolve the live subscriber state ONCE per drive call
        // (not per attempt) — RetryState persists across the retry loop, so the
        // 429/enterprise gate is stable for the whole request, matching the TS
        // granularity (the gate effectively stabilizes per request).
        let sub = self.effective_subscriber();
        let mut state = RetryState {
            consecutive_overloaded: initial_consecutive_overloaded,
            is_subscriber: sub.is_subscriber,
            is_enterprise: sub.is_enterprise,
            // Fail FAST on a rate limit that cannot clear in the backoff window
            // (a subscription plan's quota, an OpenRouter free-tier share);
            // API-key routes + Anthropic keep the parity 429-retry.
            rate_limit_terminal: rate_limit_cannot_clear(req.profile.as_deref(), &req.model),
            ..RetryState::default()
        };
        // thinking_budget for telemetry: Adaptive → 0, Enabled{b} → b.
        let thinking_budget: u32 = reasoning_budget(req.reasoning);
        // Index into `chain` for the NEXT fallback entry to use.
        // chain_idx=0 means chain[0] is the current fallback in `retry_control`.
        // After a Fallback step, chain_idx advances to point at the next entry.
        // When chain_idx >= chain.len(), the chain is exhausted.
        let mut chain_idx: usize = 0;
        // 2.1.198 `u`/`Ygf`: AWS-auth-triggered retries taken this drive.
        let mut aws_auth_attempts: u32 = 0;
        let mut max_tokens_adjusted = false;
        loop {
            // Strip rejected thinking before encode so Gemini / OpenAI-compat
            // thinking models can prepare. DeepSeek / Kimi skip this.
            // prepare → inject headers → execute.
            let mut prepared = match self.client.prepare_on(&req, self.transport.clone()).await {
                Ok(p) => p,
                Err(e) => {
                    // prepare() errors (auth, capability, encoding) are always terminal.
                    telemetry::emit_failed(
                        &self.analytics,
                        &req.model,
                        &request_id,
                        Self::error_kind(&e),
                        Self::status_of(&e),
                    )
                    .await;
                    return Err(e);
                }
            };
            if !connections_captured {
                connections_captured = true;
                connection_chain.clone_from(&prepared.route.resolved_route.connection_chain);
                failover = prepared.route.resolved_route.failover;
            }
            Self::log_deepseek_prepared_request(&req.model, &prepared, false);
            self.inject_headers(&mut prepared, &request_id, dispatch);
            self.client.seal_prepared(&mut prepared).await?;
            let mut attempt = self.begin_model_attempt(&req, &prepared).await?;
            let call = prepared.wire_call.take().expect("sealed call");
            let pricing = (self.estimator.is_some() || req.model_attempt.is_some()).then(|| {
                let snapshot = call.pricing_snapshot();
                self.estimator.as_ref().map_or_else(
                    || snapshot.clone(),
                    |est| {
                        est.capture(
                            snapshot.clone(),
                            &prepared.route.resolved_route.pricing_model,
                        )
                    },
                )
            });
            let resp_result = match call
                .dispatch_once_with(|| {
                    attempt
                        .mark_dispatched()
                        .map_err(crate::execution::wire_error)
                })
                .await
            {
                Ok(received) => received.collect().await.map_err(crate::upstream::error),
                Err(error) => Err(crate::upstream::error(error)),
            };

            match resp_result {
                Err(transport_err) => {
                    attempt.finish().await?;
                    // (cc 2.1.219) dispatch-header degradation: a connection
                    // error on an attempt that carried anthropic-dispatch-id
                    // strips it for the rest of this query and retries
                    // immediately WITHOUT consuming retry budget
                    // ("retry:dispatch-header-strip").
                    if let Some((reason, status)) =
                        Self::note_dispatch_header_failure(&mut dispatch, &prepared, &transport_err)
                    {
                        telemetry::emit_dispatch_header_fallback(
                            &self.analytics,
                            &req.model,
                            reason,
                            status,
                        )
                        .await;
                        continue;
                    }
                    if let Some(next) = advance_connection(
                        &mut req,
                        &mut state,
                        &connection_chain,
                        &mut connection_index,
                        failover,
                        &transport_err,
                    ) {
                        tracing::info!(
                            event = "connection_failover",
                            next_connection = %next,
                            "endpoint failed; retrying the same model on the next connection"
                        );
                        continue;
                    }
                    // Transport-layer failure; feed into the retry driver.
                    let step = next_step_with_backoff(
                        &mut state,
                        &retry_control,
                        &transport_err,
                        thinking_budget,
                        self.settings_backoff_ms,
                    );
                    if let DriveStep::RetryAfter(delay) = step {
                        self.report_and_sleep_retry(&transport_err, delay, &state, &retry_control)
                            .await;
                        continue;
                    }
                    telemetry::emit_failed(
                        &self.analytics,
                        &req.model,
                        &request_id,
                        Self::error_kind(&transport_err),
                        None,
                    )
                    .await;
                    return Err(transport_err);
                }
                Ok(collected) => {
                    let provider_resp = crate::execution::response(collected.response());
                    // Step 1b: x-should-retry: false is terminal for retryable 5xx.
                    let x_should_retry_false = provider_resp
                        .headers
                        .get("x-should-retry")
                        .is_some_and(|v| v.as_str() == "false");

                    let estimate = frozen_stream_quote(
                        pricing.as_ref(),
                        &prepared.route.resolved_route.pricing_model,
                        collected.usage_report(),
                        collected.inference_report(),
                    );
                    let mut extracted_usage = crate::upstream::usage(
                        collected.usage_report(),
                        collected.inference_report(),
                    );
                    if let Some((usage, completeness)) = &mut extracted_usage {
                        usage.cost_estimate = estimate.clone();
                        attempt.observe(usage, *completeness);
                    }
                    let decoded = crate::execution::decode(&collected).and_then(|decoded| {
                        crate::upstream::project_response(
                            decoded,
                            provider_resp.clone(),
                            crate::upstream::family(&prepared.route.protocol),
                        )
                    });
                    if extracted_usage.is_none() {
                        if let Ok(response) = &decoded {
                            let completeness =
                                if crate::model_attempt::has_usage_report(&response.usage) {
                                    crate::ModelAttemptUsageCompleteness::Complete
                                } else {
                                    crate::ModelAttemptUsageCompleteness::Partial
                                };
                            attempt.observe(&response.usage, completeness);
                        }
                    }
                    attempt.finish().await?;
                    collected.finish().await;
                    match decoded {
                        Ok(mut response) => {
                            // Feed rate-limit headers from every 2xx success response.
                            self.record_rate_limit_from_headers(
                                &provider_resp.headers,
                                &request_id,
                            );
                            // 3c-T3: populate response.cost when an estimator is wired.
                            // Unpriced or unknown models leave response.cost = None — never an error.
                            response.cost = estimate;
                            let elapsed_ms =
                                u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                            telemetry::emit_succeeded(
                                &self.analytics,
                                &req.model,
                                &request_id,
                                elapsed_ms,
                                provider_resp.status,
                            )
                            .await;
                            if req.capture_retry_count {
                                if !response.provider_metadata.is_object() {
                                    response.provider_metadata = serde_json::json!({});
                                }
                                let metadata = response
                                    .provider_metadata
                                    .as_object_mut()
                                    .expect("provider metadata normalized to an object");
                                metadata.insert(
                                    "_lingxi_retry_count".to_string(),
                                    serde_json::Value::from(state.attempt),
                                );
                            }
                            // #5: surface this drive's retry count to the cost
                            // path via `last_retry_count()`.
                            *self.last_retry_count.lock().unwrap() = state.attempt;
                            return Ok(response);
                        }
                        Err(decode_err) => {
                            // Step 1b: honour x-should-retry: false as terminal.
                            if x_should_retry_false {
                                telemetry::emit_failed(
                                    &self.analytics,
                                    &req.model,
                                    &request_id,
                                    Self::error_kind(&decode_err),
                                    Self::status_of(&decode_err),
                                )
                                .await;
                                return Err(decode_err);
                            }

                            // Rate-limited: resolve delay from headers.
                            let effective_err = if let LlmError::RateLimited { .. } = &decode_err {
                                // Task 6 (batch 5): capture the 429's OWN
                                // unified headers (errors.ts:471-516) so a
                                // terminal 429 can surface the limits copy.
                                self.record_rate_limit_from_429(
                                    &provider_resp.headers,
                                    Some(&provider_resp.body_json),
                                    &req.model,
                                );
                                let delay = Self::resolve_retry_after(&provider_resp.headers);
                                telemetry::emit_rate_limited(
                                    &self.analytics,
                                    &req.model,
                                    u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                                )
                                .await;
                                LlmError::RateLimited {
                                    retry_after: Some(delay),
                                    scope: None,
                                }
                            } else {
                                decode_err.clone()
                            };

                            // 2.1.198 `V_c`/`G_c`/`s_f` + `Ygf`: an AWS-auth
                            // failure (401/403) on the Bedrock provider runs
                            // the awsAuthRefresh flow (`ZBd`) and retries,
                            // bounded at AWS_AUTH_MAX_ATTEMPTS (Ygf=2). The
                            // binary clears the memoized credential resolver
                            // (`xce()`) and lets the retry's credential
                            // resolve run ZBd; lingxi resolves credentials
                            // inside `prepare()`, so the flow runs inline
                            // before the re-prepare. Once the bound is hit
                            // the error falls through to the normal driver
                            // (Authentication ⇒ Terminal — the binary's
                            // `api_request_aws_auth_exhausted` throw).
                            if let Some(aws) = &self.aws_auth {
                                if aws_auth_attempts < crate::aws_auth::AWS_AUTH_MAX_ATTEMPTS
                                    && crate::aws_auth::is_aws_auth_error(
                                        &decode_err,
                                        &prepared.route.resolved_route.provider_id,
                                    )
                                {
                                    aws_auth_attempts += 1;
                                    aws.refresh().await;
                                    continue;
                                }
                            }

                            // Fast-mode-400 (claude-code `if(T && YNd(v)){Klc(),
                            // o.fastMode=!1;continue}`): a `speed:"fast"` request
                            // whose account/model doesn't support fast mode gets a
                            // 400 "Fast mode is not enabled". Clear `req.speed`
                            // (so the re-prepared body drops the `speed` key) and
                            // retry WITHOUT counting it against the retry budget,
                            // rather than surfacing the 400 as a terminal
                            // InvalidRequest that fails the /fast user's turn.
                            // Guarded on fast mode being ON so a genuine 400
                            // without fast mode still terminates; once cleared the
                            // 400 cannot recur (the body carries no `speed`).
                            if req.speed.as_deref() == Some("fast")
                                && Self::is_fast_mode_not_enabled(&decode_err)
                            {
                                tracing::info!(
                                    event = "fast_mode_disabled_retry",
                                    "fast mode not enabled for this account/model; disabling and retrying"
                                );
                                req.speed = None;
                                continue;
                            }

                            // (cc 2.1.219) dispatch-header degradation: an
                            // HTTP 5xx on an attempt that carried
                            // anthropic-dispatch-id strips it for the rest of
                            // this query and retries immediately WITHOUT
                            // consuming retry budget
                            // ("retry:dispatch-header-strip").
                            if let Some((reason, status)) = Self::note_dispatch_header_failure(
                                &mut dispatch,
                                &prepared,
                                &decode_err,
                            ) {
                                telemetry::emit_dispatch_header_fallback(
                                    &self.analytics,
                                    &req.model,
                                    reason,
                                    status,
                                )
                                .await;
                                continue;
                            }

                            if let Some(next) = advance_connection(
                                &mut req,
                                &mut state,
                                &connection_chain,
                                &mut connection_index,
                                failover,
                                &effective_err,
                            ) {
                                tracing::info!(
                                    event = "connection_failover",
                                    next_connection = %next,
                                    "endpoint failed; retrying the same model on the next connection"
                                );
                                continue;
                            }
                            let step = guard_max_tokens_adjustment(
                                next_step_with_backoff(
                                    &mut state,
                                    &retry_control,
                                    &effective_err,
                                    thinking_budget,
                                    self.settings_backoff_ms,
                                ),
                                req.max_tokens,
                                max_tokens_adjusted,
                            );
                            match step {
                                DriveStep::RetryAfter(delay) => {
                                    self.report_and_sleep_retry(
                                        &effective_err,
                                        delay,
                                        &state,
                                        &retry_control,
                                    )
                                    .await;
                                    continue;
                                }
                                DriveStep::AdjustMaxTokens(new_max) => {
                                    // Emit telemetry for the overflow adjustment.
                                    if let Some(overflow) =
                                        crate::model::overflow::parse_overflow_message(
                                            match &decode_err {
                                                LlmError::InvalidRequest { message } => message,
                                                _ => "",
                                            },
                                        )
                                    {
                                        telemetry::emit_max_tokens_overflow_adjustment(
                                            &self.analytics,
                                            &req.model,
                                            overflow.input_tokens,
                                            overflow.context_limit,
                                            new_max,
                                            state.attempt,
                                        )
                                        .await;
                                    }
                                    max_tokens_adjusted = true;
                                    req.max_tokens = Some(new_max);
                                    continue;
                                }
                                DriveStep::StripThinkingSignature => {
                                    if self.handle_thinking_signature_strip(&mut req).await {
                                        continue;
                                    }
                                    telemetry::emit_failed(
                                        &self.analytics,
                                        &req.model,
                                        &request_id,
                                        Self::error_kind(&decode_err),
                                        Self::status_of(&decode_err),
                                    )
                                    .await;
                                    return Err(decode_err);
                                }
                                DriveStep::Fallback { fallback_model } => {
                                    if req.model_attempt.is_some() {
                                        return Err(decode_err);
                                    }
                                    // Switch to the fallback model; advance the
                                    // chain index so the next iteration's ctl
                                    // points at chain[chain_idx] (or is
                                    // exhausted → allow_fallback=false).
                                    strip_signature_blocks_for_fallback(&mut req.messages);
                                    req.model = fallback_model;
                                    chain_idx += 1;
                                    // Reset the consecutive-overload counter so
                                    // the new primary model's 529 budget is fresh.
                                    state.consecutive_overloaded = 0;
                                    // Rebuild retry_control with the next chain
                                    // entry (None when exhausted).
                                    let next_fallback = chain.get(chain_idx).cloned();
                                    let allow_fallback = next_fallback.is_some();
                                    retry_control = resolve_retry_control_with_settings(
                                        &req.model,
                                        next_fallback,
                                        sub.is_subscriber,
                                        &ResolveRetryEnv::from_process_env(),
                                        self.settings_max_retries,
                                    );
                                    if allow_fallback {
                                        retry_control.allow_fallback = true;
                                    }
                                    continue;
                                }
                                DriveStep::Terminal => {
                                    // B6-T1: the turn DIES here — promote the
                                    // 429 snapshot staged this attempt into the
                                    // live caches (the TS terminal catch handler
                                    // `extractQuotaStatusFromError`,
                                    // claudeAiLimits.ts:487). Gated on the
                                    // RateLimited discriminant so a non-429
                                    // terminal never promotes a stale slot.
                                    if matches!(decode_err, LlmError::RateLimited { .. }) {
                                        self.promote_pending_429();
                                    }
                                    telemetry::emit_failed(
                                        &self.analytics,
                                        &req.model,
                                        &request_id,
                                        Self::error_kind(&decode_err),
                                        Self::status_of(&decode_err),
                                    )
                                    .await;
                                    return Err(decode_err);
                                }
                                DriveStep::RepeatedOverloaded => {
                                    // External non-sandbox threshold: surface the
                                    // repeated bit so the conversion layer produces
                                    // `OrchestratorError::RepeatedOverloaded` with the
                                    // byte-locked "Repeated 529 Overloaded errors" copy
                                    // (errors.ts:166).
                                    let repeated_err = LlmError::Overloaded { repeated: true };
                                    telemetry::emit_failed(
                                        &self.analytics,
                                        &req.model,
                                        &request_id,
                                        Self::error_kind(&repeated_err),
                                        Self::status_of(&repeated_err),
                                    )
                                    .await;
                                    return Err(repeated_err);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // ── Inherent provider-neutral entry points ───────────────────────────────

    /// Non-streaming call (provider-neutral). The drive logic of the
    /// orchestrator's `OrchestratorApiClient::messages_create`. `profile` is the
    /// optional provider profile (the orchestrator threads `SessionState`'s; the
    /// subagent seam passes `None`).
    pub async fn messages_create(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
    ) -> Result<LlmResponse, LlmError> {
        let req = self.build_request(model, profile, system, messages, tools, false, None)?;
        let ctl = resolve_retry_control_with_settings(
            model,
            None,
            self.effective_subscriber().is_subscriber,
            &ResolveRetryEnv::from_process_env(),
            self.settings_max_retries,
        );
        self.drive_non_stream(req, ctl, DispatchHeaderState::default())
            .await
    }

    /// Non-streaming call carrying a `context_hint` offer.
    ///
    /// Identical to [`Self::messages_create`] except the request sets
    /// [`crate::LlmRequest::context_hint`], which the Anthropic codec emits as
    /// the top-level `context_hint` body key and [`Self::beta_context`] reads
    /// back to add the `context-hint-2026-04-09` beta.
    ///
    /// `None` is byte-identical to [`Self::messages_create`] — which is the
    /// state every request is in unless a host turns the controller on.
    pub async fn messages_create_with_context_hint(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
        context_hint: Option<serde_json::Value>,
    ) -> Result<LlmResponse, LlmError> {
        let mut req = self.build_request(model, profile, system, messages, tools, false, None)?;
        req.context_hint = context_hint;
        let ctl = resolve_retry_control_with_settings(
            model,
            None,
            self.effective_subscriber().is_subscriber,
            &ResolveRetryEnv::from_process_env(),
            self.settings_max_retries,
        );
        self.drive_non_stream(req, ctl, DispatchHeaderState::default())
            .await
    }

    /// Non-streaming call with an explicit `max_tokens` escalation override
    /// (provider-neutral). The drive logic of the orchestrator's
    /// `OrchestratorApiClient::messages_create_with_opts`. `profile` is the
    /// optional provider profile.
    pub async fn messages_create_with_opts(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
        max_tokens: u32,
    ) -> Result<LlmResponse, LlmError> {
        let req = self.build_request(
            model,
            profile,
            system,
            messages,
            tools,
            false,
            Some(max_tokens),
        )?;
        let ctl = resolve_retry_control_with_settings(
            model,
            None,
            self.effective_subscriber().is_subscriber,
            &ResolveRetryEnv::from_process_env(),
            self.settings_max_retries,
        );
        self.drive_non_stream(req, ctl, DispatchHeaderState::default())
            .await
    }

    /// Build the canonical non-strict side-query request used by both
    /// estimation and the live Session dispatch path.
    #[allow(clippy::too_many_arguments)]
    pub fn build_side_query_request_with_thinking(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
        max_tokens: Option<u32>,
        tool_choice: Option<crate::ToolChoice>,
        stop_sequences: Vec<String>,
        thinking: Option<crate::model::thinking::ThinkingConfig>,
        effort: Option<serde_json::Value>,
        temperature: Option<f32>,
        query_source: Option<&str>,
    ) -> Result<LlmRequest, LlmError> {
        let mut req = crate::thinking_scope::isolated(|| {
            self.build_request(model, profile, system, messages, tools, false, max_tokens)
        })?;
        req.effort = effort;
        req.tool_choice = tool_choice;
        req.stop_sequences = stop_sequences;
        req.capture_retry_count = true;
        req.query_source = query_source.map(str::to_string);
        self.apply_side_query_thinking(&mut req, model, thinking, temperature);
        Ok(req)
    }

    /// Build the canonical strict JSON-schema request used by both estimation
    /// and the live Session dispatch path.
    #[allow(clippy::too_many_arguments)]
    pub fn build_json_schema_request_with_thinking(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        schema: serde_json::Value,
        max_tokens: Option<u32>,
        effort: Option<serde_json::Value>,
        thinking: Option<crate::model::thinking::ThinkingConfig>,
        temperature: Option<f32>,
        query_source: Option<&str>,
    ) -> Result<LlmRequest, LlmError> {
        let mut req = self.build_request(
            model,
            profile,
            system,
            messages,
            Vec::new(),
            true,
            max_tokens,
        )?;
        req.tool_choice = None;
        req.effort = effort;
        req.response_format = Some(crate::ResponseFormat::JsonSchema { schema });
        self.apply_side_query_thinking(&mut req, model, thinking, temperature);
        if query_source.is_some() {
            req.query_source = query_source.map(str::to_string);
        }
        Ok(req)
    }

    /// Run a session-bound side query through the same request builder,
    /// provider routing, credentials, cache layout, headers, and retry driver
    /// as the parent conversation.
    ///
    /// This is the compaction/recap path. It deliberately replaces the main
    /// request's forced tool choice (for example `--json-schema`) with the side
    /// query's own choice: Claude Code's compaction call exposes no tools and
    /// must not inherit a main-turn `StructuredOutput` requirement. All other
    /// session-scoped wire behavior, including thinking configuration and
    /// request metadata, remains shared.
    ///
    /// `max_tokens` is `None` for the model-aware ordinary request budget — the
    /// same signal the main turn passes. For Claude this remains its native
    /// default; catalog-backed non-Claude routes use a safe 32k default rather
    /// than treating a hard provider ceiling as a per-turn target. It was
    /// previously a bare `u32`, which forced every caller to invent a ceiling;
    /// on a reasoning model that invented number silently capped the THINKING
    /// pass as well as the answer. Pass `Some(n)` only where a caller has a real
    /// reason to request a different budget.
    #[allow(clippy::too_many_arguments)]
    pub async fn messages_create_side_query(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
        max_tokens: Option<u32>,
        tool_choice: Option<crate::ToolChoice>,
        stop_sequences: Vec<String>,
        temperature: Option<f32>,
        query_source: Option<&str>,
    ) -> Result<LlmResponse, LlmError> {
        let mut req = crate::thinking_scope::isolated(|| {
            self.build_request(model, profile, system, messages, tools, false, max_tokens)
        })?;

        // `build_request` applies main-turn-only overrides. A forked summary
        // owns these fields independently, so restore its explicit values.
        req.tool_choice = tool_choice;
        req.stop_sequences = stop_sequences;
        req.temperature = temperature.map(f64::from);
        req.query_source = query_source.map(str::to_string);

        let ctl = resolve_retry_control_with_settings(
            model,
            None,
            self.effective_subscriber().is_subscriber,
            &ResolveRetryEnv::from_process_env(),
            self.settings_max_retries,
        );
        self.drive_non_stream(req, ctl, DispatchHeaderState::AUXILIARY)
            .await
    }

    /// Non-streaming side query with explicit thinking semantics.
    ///
    /// `thinking = None` means emit no reasoning field at all; `Some(cfg)`
    /// resolves through the same model-specific thinking policy as the main
    /// request path.
    #[allow(clippy::too_many_arguments)]
    pub async fn messages_create_side_query_with_thinking(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
        max_tokens: Option<u32>,
        tool_choice: Option<crate::ToolChoice>,
        stop_sequences: Vec<String>,
        thinking: Option<crate::model::thinking::ThinkingConfig>,
        effort: Option<serde_json::Value>,
        temperature: Option<f32>,
        query_source: Option<&str>,
    ) -> Result<LlmResponse, LlmError> {
        let req = self.build_side_query_request_with_thinking(
            model,
            profile,
            system,
            messages,
            tools,
            max_tokens,
            tool_choice,
            stop_sequences,
            thinking,
            effort,
            temperature,
            query_source,
        )?;

        let ctl = resolve_retry_control_with_settings(
            model,
            None,
            self.effective_subscriber().is_subscriber,
            &ResolveRetryEnv::from_process_env(),
            self.settings_max_retries,
        );
        self.drive_non_stream(req, ctl, DispatchHeaderState::AUXILIARY)
            .await
    }

    /// Open a session-bound streaming side query for bounded embedded clients.
    /// This mirrors [`Self::messages_create_side_query`] while preserving the
    /// caller's output and temperature limits on the streaming request.
    #[allow(clippy::too_many_arguments)]
    pub async fn messages_create_side_query_stream(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
        max_tokens: Option<u32>,
        tool_choice: Option<crate::ToolChoice>,
        stop_sequences: Vec<String>,
        temperature: Option<f32>,
        query_source: Option<&str>,
    ) -> Result<BoxStream<'static, Result<LlmEvent, LlmError>>, LlmError> {
        let mut req = crate::thinking_scope::isolated(|| {
            self.build_request(model, profile, system, messages, tools, true, max_tokens)
        })?;
        req.tool_choice = tool_choice;
        req.stop_sequences = stop_sequences;
        req.temperature = temperature.map(f64::from);
        req.query_source = query_source.map(str::to_string);
        self.drive_stream(req).await
    }

    /// Streaming side query with explicit thinking semantics.
    #[allow(clippy::too_many_arguments)]
    pub async fn messages_create_side_query_stream_with_thinking(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
        max_tokens: Option<u32>,
        tool_choice: Option<crate::ToolChoice>,
        stop_sequences: Vec<String>,
        thinking: Option<crate::model::thinking::ThinkingConfig>,
        temperature: Option<f32>,
        query_source: Option<&str>,
    ) -> Result<BoxStream<'static, Result<LlmEvent, LlmError>>, LlmError> {
        let mut req = crate::thinking_scope::isolated(|| {
            self.build_request(model, profile, system, messages, tools, true, max_tokens)
        })?;
        req.tool_choice = tool_choice;
        req.stop_sequences = stop_sequences;
        req.query_source = query_source.map(str::to_string);
        self.apply_side_query_thinking(&mut req, model, thinking, temperature);
        self.drive_stream(req).await
    }

    /// Non-streaming call with the **Opus-fallback** policy wired
    /// (provider-neutral). The drive logic of the orchestrator's
    /// `OrchestratorApiClient::messages_create_with_fallback`.
    ///
    /// Routes through [`resolve_retry_control_with_settings`] which computes
    /// `allow_fallback` from the env + subscriber state. The
    /// `_is_subscriber` / `_is_enterprise` parameters are **ignored** — the
    /// service always reads subscriber state via [`Self::effective_subscriber`]
    /// (the live shared snapshot when attached, else the construction-time copy).
    /// The underscore prefix signals that these call-site values are not used;
    /// the parameters are kept for API compatibility.
    #[allow(clippy::too_many_arguments)]
    pub async fn messages_create_with_fallback(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
        fallback_model: Option<&str>,
        _is_subscriber: bool,
        _is_enterprise: bool,
    ) -> Result<LlmResponse, LlmError> {
        // Per-model settings fallback wins over global fallback_model.
        // Call-site fallback_model (from OrchestratorApiClient) wins over both when
        // it's explicitly passed.
        //
        // Normalize the request model via alias_to_display so that an alias
        // request (e.g. "claude-3-5-sonnet" → display "claude-sonnet-4-5")
        // still finds the per-model fallback entry whose key is the display model.
        let display_model = self
            .alias_to_display
            .get(model)
            .map_or(model, String::as_str);

        // Build the effective chain:
        //   1. explicit call-site fallback_model → single-entry chain (legacy path)
        //   2. per-model settings chain          → full multi-entry chain
        //   3. global fallback_model             → single-entry chain
        // The chain is walked entry-by-entry in the drive loop.
        let effective_chain: Vec<String> = if let Some(fb) = fallback_model {
            // The legacy call-site parameter remains a string for API
            // compatibility, but Claude's CLI value is an ordered CSV list.
            parse_fallback_chain(fb)
        } else if let Some(chain) = self.fallback_overrides.get(display_model) {
            chain.clone()
        } else if !self.fallback_models.is_empty() {
            self.fallback_models.clone()
        } else {
            vec![]
        };

        // Primary request uses the passed profile; fallback requests use None
        // (the fallback config string has no associated profile).
        let req = self.build_request(model, profile, system, messages, tools, false, None)?;
        // Initial ctl: chain[0] as fallback_model (None when chain is empty).
        let mut ctl = resolve_retry_control_with_settings(
            model,
            effective_chain.first().cloned(),
            self.effective_subscriber().is_subscriber,
            &ResolveRetryEnv::from_process_env(),
            self.settings_max_retries,
        );
        // If any fallback is configured, honour allow_fallback regardless of
        // the model-type heuristic (preserves the pre-Task-8 contract: an
        // explicit or configured fallback always enables the gate).
        if !effective_chain.is_empty() {
            ctl.allow_fallback = true;
        }
        self.drive_non_stream_seeded_with_chain(
            req,
            ctl,
            0,
            &effective_chain,
            DispatchHeaderState::default(),
        )
        .await
    }

    /// Count the input tokens a non-streaming `messages.create` for
    /// `(model, profile, system, messages, tools)` would consume on its resolved
    /// route. The drive logic of the orchestrator's
    /// `OrchestratorApiClient::count_tokens`: build the same non-streaming request
    /// shape `messages_create` sends, then delegate to the count_tokens facade —
    /// the real `/v1/messages/count_tokens` endpoint (with the `count_tokens`
    /// beta) on Anthropic routes, byte-length/4 approximation elsewhere.
    pub async fn count_tokens(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
    ) -> Result<u64, LlmError> {
        let req = self.build_request(model, profile, system, messages, tools, false, None)?;
        crate::model::count_tokens::count_tokens(
            self.client.as_ref(),
            self.transport.as_ref(),
            &req,
        )
        .await
    }

    /// Return an exact provider token count when the resolved route supports
    /// it. Unlike [`Self::count_tokens`], this never substitutes the generic
    /// text-only approximation, which is unsuitable for ToolSearch's schema
    /// threshold calculation.
    pub async fn count_tokens_exact(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
    ) -> Result<Option<u64>, LlmError> {
        let req = self.build_request(model, profile, system, messages, tools, false, None)?;
        crate::model::count_tokens::try_count_tokens_exact(
            self.client.as_ref(),
            self.transport.as_ref(),
            &req,
        )
        .await
    }

    /// Non-streaming call seeded with a pre-counted consecutive-529 value
    /// (provider-neutral). The drive logic of the orchestrator's
    /// `OrchestratorApiClient::messages_create_seeded`: used by the mid-stream 529
    /// fallback (Task 7) so the streaming 529 that triggered the fallback is
    /// pre-counted into the retry budget. Mirrors TS `claude.ts:2559`
    /// (`initialConsecutive529Errors: is529Error(streamingError) ? 1 : 0`).
    pub async fn messages_create_seeded(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
        initial_consecutive_overloaded: u8,
    ) -> Result<LlmResponse, LlmError> {
        let req = self.build_request(model, profile, system, messages, tools, false, None)?;
        let ctl = resolve_retry_control_with_settings(
            model,
            None,
            self.effective_subscriber().is_subscriber,
            &ResolveRetryEnv::from_process_env(),
            self.settings_max_retries,
        );
        self.drive_non_stream_seeded_with_chain(
            req,
            ctl,
            initial_consecutive_overloaded,
            &[],
            DispatchHeaderState::default(),
        )
        .await
    }

    /// Enumerate available `provider/model` ids + `@aliases` for `/model`'s list
    /// mode — the available-model ids captured from the client registry at
    /// construction. Backs the orchestrator's
    /// `OrchestratorApiClient::available_models`.
    #[must_use]
    pub fn available_models(&self) -> Vec<String> {
        self.available_model_ids.clone()
    }

    /// Enumerate full provider-specific model metadata for picker surfaces.
    #[must_use]
    pub fn model_listings(&self) -> Vec<crate::ModelListing> {
        self.model_listings.clone()
    }

    /// Capture the configured pricing policy for one exact provider profile.
    /// Unlike diagnostic model metadata, this preserves explicit override
    /// declarations. It exposes no credentials and does not resolve a price.
    #[must_use]
    pub fn profile_pricing_config(&self, profile: &str) -> Option<crate::PricingConfig> {
        self.client.profile_pricing_config(profile)
    }

    /// Conservative rate ceilings for a model with contextual or scheduled prices.
    /// These authorize a budget; only a frozen execution quote may settle it.
    pub fn attempt_price_bounds(
        &self,
        route: &crate::ResolvedRoute,
    ) -> Result<Option<crate::AttemptPriceBounds>, LlmError> {
        self.client.attempt_price_bounds(route)
    }

    // ── OpenAI Responses WebSocket preconnect ────────────────────────────────

    /// Best-effort startup preconnect for OpenAI Responses WebSocket profiles.
    ///
    /// This opens the WebSocket handshake only; no prompt payload is sent.
    /// Callers intentionally ignore failures so normal HTTP/SSE or later WS
    /// connect paths remain authoritative.
    pub async fn preconnect_responses_websocket(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<(), LlmError> {
        let mut request = LlmRequest::new(model);
        request.profile = profile.map(str::to_string);
        request.stream = true;
        let mut session = self.responses_ws_session.lock().await;
        self.client
            .preconnect_websocket(&request, self.transport.as_ref(), &mut session)
            .await
    }

    /// Spawn [`Self::preconnect_responses_websocket`] on the current runtime and
    /// discard errors. Intended for engine startup where latency reduction must
    /// never block session initialization.
    pub fn spawn_responses_websocket_preconnect(
        self: &Arc<Self>,
        model: String,
        profile: Option<String>,
    ) {
        let adapter = Arc::clone(self);
        tokio::spawn(async move {
            let _ = adapter
                .preconnect_responses_websocket(&model, profile.as_deref())
                .await;
        });
    }

    /// Best-effort startup **prewarm** for OpenAI Responses WebSocket providers:
    /// send the provided (empty-history) request with `generate=false` over the
    /// session so the handshake + first round-trip are warm. Backs the
    /// `OrchestratorApiClient::prewarm_responses_websocket` trait override.
    pub async fn prewarm_responses_websocket(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
    ) -> Result<(), LlmError> {
        let req = self.build_request(model, profile, system, messages, tools, true, None)?;
        let mut prepared = self.client.prepare_on(&req, self.transport.clone()).await?;
        let request_id = new_request_id();
        // Responses-WebSocket prewarm only — never a first-party Anthropic
        // route, so the dispatch gate is inert here.
        self.inject_stream_headers(&mut prepared, &request_id, DispatchHeaderState::default());
        let mut session = self.responses_ws_session.lock().await;
        self.client
            .prewarm_prepared_websocket(prepared, self.transport.as_ref(), &mut session)
            .await
    }

    /// Close any reusable Responses WebSocket session held by this service. Backs
    /// the `OrchestratorApiClient::close_responses_websocket_session` trait
    /// override.
    pub async fn close_responses_websocket_session(&self) -> Result<(), LlmError> {
        let mut session = self.responses_ws_session.lock().await;
        session.close().await
    }

    // ── Stream drive (Step 2) ─────────────────────────────────────────────────

    /// Drive a streaming call; connect-phase failures retry through the driver.
    ///
    /// Uses `DefaultLlmClient::execute_stream` which already handles the
    /// connect-phase error-drain path internally.  For the retry loop we re-prepare
    /// on each attempt so a fresh `PreparedLlmCall` (with correct auth headers) is
    /// sent even after a previous attempt fails.
    ///
    /// **Streaming rate-limit headers (3c-T1 closed):** `Transport::open_stream`
    /// now returns real `StreamingResponse{status, headers}` via the additive
    /// `stream_sse_with_meta` path added in plan 3c.  The connect-phase ≥400
    /// branch below reads `streaming.headers` and calls `resolve_retry_after`
    /// just as the non-stream path does, so 429+`retry-after` delays are
    /// honoured on the streaming path.
    #[allow(clippy::too_many_lines)]
    async fn drive_stream(
        &self,
        mut req: LlmRequest,
    ) -> Result<BoxStream<'static, Result<LlmEvent, LlmError>>, LlmError> {
        let request_id = new_request_id();
        telemetry::emit_started(&self.analytics, &req.model, &request_id, true).await;
        if let Some(query_source) = req.query_source.as_deref() {
            telemetry::emit_query_source(&self.analytics, &req.model, query_source).await;
        }

        // B6-T1: discard any 429 snapshot staged by a PRIOR drive (see the
        // non-stream drive fn) — per-error state, never carried across calls.
        self.clear_pending_429();

        // Batch-5 Task 3: live subscriber state, resolved ONCE per drive call
        // (see `drive_non_stream_seeded_with_chain` for the granularity note).
        let sub = self.effective_subscriber();
        let mut state = RetryState {
            is_subscriber: sub.is_subscriber,
            is_enterprise: sub.is_enterprise,
            // Subscription / free-tier rate limits fail fast (see non-stream drive).
            rate_limit_terminal: rate_limit_cannot_clear(req.profile.as_deref(), &req.model),
            ..RetryState::default()
        };
        // Stream path uses settings-based retry control (same precedence as non-stream).
        let ctl = resolve_retry_control_with_settings(
            &req.model,
            None, // fallback not used on stream connect-phase
            sub.is_subscriber,
            &ResolveRetryEnv::from_process_env(),
            self.settings_max_retries,
        );
        let thinking_budget: u32 = reasoning_budget(req.reasoning);
        // Connection failover state — see the non-stream drive for why the chain
        // is captured once. The stream connect phase had NO fallback of any kind
        // before this, which is why `routing.fallback` never ran on desktop or
        // mobile (both drive turns through `StreamingTurnDriver`).
        let mut connection_chain: Vec<crate::ConnectionHop> = Vec::new();
        let mut connection_index = 0usize;
        let mut failover = crate::FailoverTriggers::NONE;
        let mut connections_captured = false;
        // 2.1.198 `u`/`Ygf`: AWS-auth-triggered retries taken this drive.
        let mut aws_auth_attempts: u32 = 0;
        // `StreamNoResponse` owns a separate one-retry ledger in the oracle.
        // It is intentionally independent from the ordinary retry budget.
        let mut no_response_retries: u8 = 0;
        let mut max_tokens_adjusted = false;
        // (cc 2.1.219) `Kt`/`no` — per-query, reset with every drive. Streams
        // are main-thread or subagent turns, never `fB()==="auxiliary"`.
        let mut dispatch = DispatchHeaderState::default();

        loop {
            // Prepare so we can inject headers, then call execute_stream via
            // a thin wrapper transport that uses our already-modified request.
            let mut prepared = match self.client.prepare_on(&req, self.transport.clone()).await {
                Ok(p) => p,
                Err(e) => return Err(e),
            };
            if !connections_captured {
                connections_captured = true;
                connection_chain.clone_from(&prepared.route.resolved_route.connection_chain);
                failover = prepared.route.resolved_route.failover;
            }
            Self::log_deepseek_prepared_request(&req.model, &prepared, true);
            tracing::debug!(model = %req.model, event = "request_prepared");
            self.inject_stream_headers(&mut prepared, &request_id, dispatch);
            // Captured before the move below — the dispatch degradation check
            // in the Err arm needs to know whether THIS attempt carried the
            // header.
            let attempt_carried_dispatch = prepared
                .provider_request
                .headers
                .contains_key(DISPATCH_ID_HEADER);

            let provider = &prepared.route.resolved_route.provider_id;
            let body_bytes = prepared.provider_request.wire_body_bytes()?.len();
            let first_byte_timeout = self.stream_first_byte_timeout_override.or_else(|| {
                crate::model::stream_watchdog::resolve_stream_first_byte_timeout(
                    provider, body_bytes,
                )
            });

            let mut attempt = crate::model_attempt::WireAttempt::new(None);

            // Open stream through the prepared-call path so injected headers are
            // preserved while OpenAI Responses providers can reuse a WebSocket
            // session and apply previous_response_id deltas. A registered
            // attempt takes the same path: the opener marks dispatch
            // immediately before whichever transport call carries it, so a
            // WebSocket send is metered exactly like an HTTP one.
            let opened = async {
                // HTTP attempts have no shared connection state. Never hold
                // the WebSocket mutex while an HTTP admission waits for a
                // durable intent: a concurrent revoked call must reject without
                // waiting for the first intent's acknowledgement.
                let mut http_session = ResponsesWebSocketSession::new();
                let mut shared_session = if matches!(
                    prepared.provider_request.stream_transport,
                    crate::ProviderStreamTransport::ResponsesWebSocket
                ) {
                    Some(self.responses_ws_session.lock().await)
                } else {
                    None
                };
                let responses_ws_session =
                    shared_session.as_deref_mut().unwrap_or(&mut http_session);
                let preparing = tokio::time::Instant::now();
                let prepared = crate::execution::first_byte_bound(
                    first_byte_timeout,
                    self.client
                        .prepare_shared_stream(prepared, responses_ws_session),
                )
                .await?;
                let remaining =
                    first_byte_timeout.map(|timeout| timeout.saturating_sub(preparing.elapsed()));
                // Budget/queue admission is host work, outside the network
                // watchdog. Its wait must not masquerade as a provider timeout.
                attempt = self.begin_model_attempt(&req, &prepared).await?;
                crate::execution::first_byte_bound(
                    remaining,
                    self.client
                        .open_shared_stream(prepared, responses_ws_session, &mut || {
                            attempt.mark_dispatched()
                        }),
                )
                .await
            }
            .await;
            match opened {
                Err(transport_err) => {
                    attempt.finish().await?;
                    // The oracle permits one `StreamNoResponse` retry across the
                    // whole request, then terminates before generic retry logic.
                    // On the first occurrence it still flows through dispatch
                    // degradation and the normal retry/backoff classifier.
                    if crate::model::stream_watchdog::is_stream_no_response(&transport_err) {
                        if no_response_retries >= 1 {
                            return Err(transport_err);
                        }
                        no_response_retries += 1;
                    }
                    // (cc 2.1.219) dispatch-header degradation, arm 1 (2.1.220
                    // @237555467) on the stream CONNECT phase: strip for the
                    // rest of this query + immediate budget-free retry.
                    if let Some((reason, status)) = Self::note_dispatch_header_failure_carried(
                        &mut dispatch,
                        attempt_carried_dispatch,
                        &transport_err,
                    ) {
                        telemetry::emit_dispatch_header_fallback(
                            &self.analytics,
                            &req.model,
                            reason,
                            status,
                        )
                        .await;
                        continue;
                    }
                    if let Some(next) = advance_connection(
                        &mut req,
                        &mut state,
                        &connection_chain,
                        &mut connection_index,
                        failover,
                        &transport_err,
                    ) {
                        tracing::info!(
                            event = "connection_failover",
                            next_connection = %next,
                            "endpoint failed; retrying the same model on the next connection"
                        );
                        continue;
                    }
                    let step = next_step_with_backoff(
                        &mut state,
                        &ctl,
                        &transport_err,
                        thinking_budget,
                        self.settings_backoff_ms,
                    );
                    match step {
                        DriveStep::RetryAfter(delay) => {
                            self.report_and_sleep_retry(&transport_err, delay, &state, &ctl)
                                .await;
                            continue;
                        }
                        _ => return Err(transport_err),
                    }
                }
                Ok((prepared, streaming)) => {
                    let response_headers: std::collections::BTreeMap<String, String> = streaming
                        .headers()
                        .iter()
                        .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
                        .collect();
                    tracing::debug!(
                        model = %req.model,
                        event = "stream_opened",
                        status = streaming.status()
                    );
                    // Connect-phase status ≥ 400: drain and decode as error.
                    if streaming.status() >= 400 {
                        let collected = match streaming.collect().await {
                            Ok(collected) => collected,
                            Err(error) => {
                                attempt.finish().await?;
                                return Err(crate::upstream::error(error));
                            }
                        };
                        let body_json = crate::execution::response(collected.response()).body_json;
                        if let Some((usage, completeness)) = crate::upstream::usage(
                            collected.usage_report(),
                            collected.inference_report(),
                        ) {
                            attempt.observe(&usage, completeness);
                        }
                        let decode_err = collected
                            .decode()
                            .err()
                            .map(crate::upstream::error)
                            .unwrap_or(LlmError::ProviderInternal);
                        collected.finish().await;
                        attempt.finish().await?;

                        // Mirror the non-stream path: for 429s, resolve the
                        // actual retry delay from the real response headers
                        // (retry-after / anthropic-ratelimit-*).  Empty headers
                        // fall through to the 1 s fallback inside
                        // `resolve_retry_after`.
                        let effective_err = if let LlmError::RateLimited { .. } = &decode_err {
                            // Task 6 (batch 5): same 429-error-header capture
                            // as the non-stream path (errors.ts:471-516).
                            self.record_rate_limit_from_429(
                                &response_headers,
                                Some(&body_json),
                                &req.model,
                            );
                            LlmError::RateLimited {
                                retry_after: Some(Self::resolve_retry_after(&response_headers)),
                                scope: None,
                            }
                        } else {
                            decode_err.clone()
                        };

                        // 2.1.198 `V_c`/`G_c`/`s_f` + `Ygf` — stream connect
                        // twin of the non-stream AWS auth-refresh hook (see
                        // `drive_non_stream_seeded_with_chain`).
                        if let Some(aws) = &self.aws_auth {
                            if aws_auth_attempts < crate::aws_auth::AWS_AUTH_MAX_ATTEMPTS
                                && crate::aws_auth::is_aws_auth_error(
                                    &decode_err,
                                    &prepared.route.resolved_route.provider_id,
                                )
                            {
                                aws_auth_attempts += 1;
                                aws.refresh().await;
                                continue;
                            }
                        }

                        // Fast-mode-400 (stream twin of the non-stream `YNd`
                        // handler): a `speed:"fast"` request the account/model
                        // rejects with a 400 "Fast mode is not enabled" clears
                        // `req.speed` and retries (uncounted) instead of failing
                        // the /fast user's streamed turn.
                        if req.speed.as_deref() == Some("fast")
                            && Self::is_fast_mode_not_enabled(&decode_err)
                        {
                            tracing::info!(
                                event = "fast_mode_disabled_retry",
                                "fast mode not enabled for this account/model; disabling and retrying (stream)"
                            );
                            req.speed = None;
                            continue;
                        }

                        if let Some(next) = advance_connection(
                            &mut req,
                            &mut state,
                            &connection_chain,
                            &mut connection_index,
                            failover,
                            &effective_err,
                        ) {
                            tracing::info!(
                                event = "connection_failover",
                                next_connection = %next,
                                "endpoint failed; retrying the same model on the next connection"
                            );
                            continue;
                        }
                        let step = guard_max_tokens_adjustment(
                            next_step_with_backoff(
                                &mut state,
                                &ctl,
                                &effective_err,
                                thinking_budget,
                                self.settings_backoff_ms,
                            ),
                            req.max_tokens,
                            max_tokens_adjusted,
                        );
                        match step {
                            DriveStep::RetryAfter(delay) => {
                                self.report_and_sleep_retry(&effective_err, delay, &state, &ctl)
                                    .await;
                                // Re-prepare on next iteration so headers stay fresh.
                                continue;
                            }
                            DriveStep::AdjustMaxTokens(new_max) => {
                                if let LlmError::InvalidRequest { message } = &decode_err {
                                    if let Some(overflow) =
                                        crate::model::overflow::parse_overflow_message(message)
                                    {
                                        telemetry::emit_max_tokens_overflow_adjustment(
                                            &self.analytics,
                                            &req.model,
                                            overflow.input_tokens,
                                            overflow.context_limit,
                                            new_max,
                                            state.attempt,
                                        )
                                        .await;
                                    }
                                }
                                max_tokens_adjusted = true;
                                req.max_tokens = Some(new_max);
                                continue;
                            }
                            DriveStep::StripThinkingSignature => {
                                if self.handle_thinking_signature_strip(&mut req).await {
                                    continue;
                                }
                            }
                            _ => {}
                        }
                        // B6-T1: the stream connect DIES here — promote the
                        // 429 snapshot staged this attempt (TS terminal catch
                        // handler, claudeAiLimits.ts:487). Gated on the
                        // RateLimited discriminant so a non-429 terminal never
                        // promotes a stale slot.
                        if matches!(decode_err, LlmError::RateLimited { .. }) {
                            self.promote_pending_429();
                        }
                        // Terminal twin for the connect-phase emit_started
                        // (mirrors the non-stream terminal arms).
                        telemetry::emit_failed(
                            &self.analytics,
                            &req.model,
                            &request_id,
                            Self::error_kind(&decode_err),
                            Self::status_of(&decode_err),
                        )
                        .await;
                        return Err(decode_err);
                    }

                    self.record_rate_limit_from_headers(&response_headers, &request_id);
                    *self.last_retry_count.lock().unwrap() = state.attempt;
                    let decoder = crate::upstream::Decoder::projection(
                        crate::upstream::family(&prepared.route.protocol),
                        crate::stream_provider_metadata_from_headers(&response_headers),
                    );
                    let mut frames = streaming
                        .into_stream()
                        .map_err(|_| LlmError::ProviderInternal)?;
                    let pricing_model = prepared.route.resolved_route.pricing_model.clone();
                    let pricing = frames
                        .pricing_snapshot()
                        .filter(|_| self.estimator.is_some() || req.model_attempt.is_some())
                        .map(|snapshot| {
                            self.estimator.as_ref().map_or_else(
                                || snapshot.clone(),
                                |estimator| estimator.capture(snapshot.clone(), &pricing_model),
                            )
                        });

                    // Clone analytics + metadata into the unfold state so
                    // emit_succeeded / emit_failed can fire from inside the async closure.
                    let stream_started = Instant::now();
                    let stream_analytics = self.analytics.clone();
                    let stream_model = req.model.clone();
                    let stream_request_id = request_id.clone();
                    // Streaming idle watchdog (cc 2.1.196 default-on): resolve
                    // the per-event idle timeout from the env once at
                    // stream-open. `None` when disabled.
                    let stream_idle_timeout = self
                        .stream_idle_timeout_override
                        .or_else(crate::model::stream_watchdog::resolve_stream_idle_timeout);

                    // (cc 2.1.219) dispatch-header degradation, arm 2 (2.1.220
                    // @237577972 — `reason:"body_phase"`): the oracle re-checks
                    // the header inside the stream BODY catch, so a connection
                    // error raised before the first event (`!oc`) strips it and
                    // `continue e`s — ahead of, and without consuming, the
                    // stale-connection budget (`ko<jt`). The port hands the
                    // stream to the caller here, so the only point where an
                    // attempt loop still exists is a one-frame lookahead. It is
                    // taken ONLY when this attempt carried the header, i.e.
                    // never on the default-off path; the frame it reads is
                    // seeded back into the unfold so decoding is unchanged.
                    let mut seed: Option<Result<Option<lingxi_llm_client::StreamBatch>, LlmError>> =
                        None;
                    if attempt_carried_dispatch {
                        let first = match stream_idle_timeout {
                            Some(t) => {
                                // Wall clock across the same wait the monotonic
                                // timeout bounds: monotonic time stops while the
                                // machine is suspended, so the excess IS the sleep.
                                let wall = std::time::SystemTime::now();
                                match tokio::time::timeout(
                                    t,
                                    crate::execution::next_batch(&mut frames),
                                )
                                .await
                                {
                                    Ok(r) => r,
                                    Err(_elapsed) => {
                                        Err(crate::model::stream_watchdog::watchdog_abort_error(
                                            t,
                                            wall.elapsed().unwrap_or(t),
                                        ))
                                    }
                                }
                            }
                            None => crate::execution::next_batch(&mut frames).await,
                        };
                        if let Ok(Some(batch)) = &first {
                            if let Some((mut usage, completeness)) =
                                crate::upstream::usage(&batch.usage, &batch.inference)
                            {
                                usage.cost_estimate = frozen_stream_quote(
                                    pricing.as_ref(),
                                    &pricing_model,
                                    &batch.usage,
                                    &batch.inference,
                                );
                                attempt.observe(&usage, completeness);
                            }
                        }
                        let first = match first {
                            Ok(Some(batch))
                                if batch.usage.usage.is_none()
                                    && batch.events.len() == 1
                                    && batch.events[0].is_err() =>
                            {
                                Err(crate::upstream::error(
                                    batch.events.into_iter().next().unwrap().unwrap_err(),
                                ))
                            }
                            other => other,
                        };
                        if let Err(first_err) = &first {
                            if Self::note_dispatch_body_phase_failure(
                                &mut dispatch,
                                attempt_carried_dispatch,
                                first_err,
                            ) {
                                attempt.finish().await?;
                                telemetry::emit_dispatch_header_fallback(
                                    &self.analytics,
                                    &req.model,
                                    "body_phase",
                                    None,
                                )
                                .await;
                                continue;
                            }
                        }
                        seed = Some(first);
                    }

                    // Assemble events via a manual unfold that drives next_frame + decode.
                    // We keep a queue of pre-decoded events and drain them first.
                    let stream_state = StreamState {
                        attempt,
                        decoder,
                        frames,
                        pricing,
                        pricing_model,
                        seed,
                        queue: VecDeque::new(),
                        finished: false,
                        done: false,
                        analytics: stream_analytics,
                        model: stream_model,
                        request_id: stream_request_id,
                        started: stream_started,
                        idle_timeout: stream_idle_timeout,
                    };

                    let boxed: BoxStream<'static, Result<LlmEvent, LlmError>> = Box::pin(
                        futures::stream::unfold(stream_state, |mut s| async move {
                            loop {
                                if let Some(event) = s.queue.pop_front() {
                                    // Emit succeed telemetry on the terminal event
                                    // (MessageStop or Completed) — once, guarded by `done`.
                                    let is_terminal = matches!(
                                        event,
                                        LlmEvent::MessageStop | LlmEvent::Completed { .. }
                                    );
                                    if is_terminal && !s.done {
                                        if let Err(error) = s.attempt.finish().await {
                                            s.finished = true;
                                            s.queue.clear();
                                            return Some((Err(error), s));
                                        }
                                        s.done = true;
                                        let elapsed_ms =
                                            u64::try_from(s.started.elapsed().as_millis())
                                                .unwrap_or(u64::MAX);
                                        telemetry::emit_succeeded(
                                            &s.analytics,
                                            &s.model,
                                            &s.request_id,
                                            elapsed_ms,
                                            200,
                                        )
                                        .await;
                                    }
                                    return Some((Ok(event), s));
                                }
                                if s.finished {
                                    if let Err(error) = s.attempt.finish().await {
                                        return Some((Err(error), s));
                                    }
                                    return None;
                                }
                                // Watchdog: bound the blocking frame read by the
                                // configured idle timeout (reset per event). On
                                // elapse, abort the stream with a detectable
                                // idle-timeout error (binary
                                // `tengu_streaming_watchdog_retry` surface).
                                let seed_has_usage = s.seed.as_ref().is_some_and(|seed| matches!(seed, Ok(Some(batch)) if batch.usage.usage.is_some()));
                                let frame = match s.seed.take() {
                                    Some(seeded) => seeded,
                                    None => match s.idle_timeout {
                                        Some(timeout) => tokio::time::timeout(
                                            timeout,
                                            crate::execution::next_batch(&mut s.frames),
                                        )
                                        .await
                                        .unwrap_or_else(|_elapsed| {
                                            Err(crate::model::stream_watchdog::idle_timeout_error(
                                                timeout,
                                            ))
                                        }),
                                        None => crate::execution::next_batch(&mut s.frames).await,
                                    },
                                };
                                match frame {
                                    Ok(Some(frame)) => {
                                        let quote = frozen_stream_quote(
                                            s.pricing.as_ref(),
                                            &s.pricing_model,
                                            &frame.usage,
                                            &frame.inference,
                                        );
                                        match s.decoder.project_batch(frame) {
                                            Ok(mut events) => {
                                                attach_frozen_stream_quote(
                                                    &mut events,
                                                    quote.as_ref(),
                                                );
                                                if let Some((mut usage, completeness)) =
                                                    s.decoder.observed_usage()
                                                {
                                                    usage.cost_estimate = quote.clone();
                                                    if !seed_has_usage {
                                                        s.attempt.observe(&usage, completeness);
                                                    }
                                                } else {
                                                    s.attempt.observe_events(&events);
                                                }
                                                s.queue.extend(events);
                                            }
                                            Err(e) => {
                                                if let Some((mut usage, completeness)) =
                                                    s.decoder.observed_usage()
                                                {
                                                    usage.cost_estimate = quote.clone();
                                                    if !seed_has_usage {
                                                        s.attempt.observe(&usage, completeness);
                                                    }
                                                }
                                                let e = s.attempt.finish().await.err().unwrap_or(e);
                                                s.finished = true;
                                                if !s.done {
                                                    s.done = true;
                                                    telemetry::emit_failed(
                                                        &s.analytics,
                                                        &s.model,
                                                        &s.request_id,
                                                        ApiService::error_kind(&e),
                                                        ApiService::status_of(&e),
                                                    )
                                                    .await;
                                                }
                                                return Some((Err(e), s));
                                            }
                                        }
                                    }
                                    Ok(None) => {
                                        s.finished = true;
                                        let quote = frozen_stream_quote(
                                            s.pricing.as_ref(),
                                            &s.pricing_model,
                                            &s.frames.usage_report(),
                                            &s.frames.inference_report(),
                                        );
                                        match s.decoder.finish() {
                                            Ok(mut events) => {
                                                attach_frozen_stream_quote(
                                                    &mut events,
                                                    quote.as_ref(),
                                                );
                                                if let Some((mut usage, completeness)) =
                                                    s.decoder.observed_usage()
                                                {
                                                    usage.cost_estimate = quote.clone();
                                                    s.attempt.observe(&usage, completeness);
                                                } else {
                                                    s.attempt.observe_events(&events);
                                                }
                                                s.queue.extend(events);
                                            }
                                            Err(e) => {
                                                if let Some((mut usage, completeness)) =
                                                    s.decoder.observed_usage()
                                                {
                                                    usage.cost_estimate = quote.clone();
                                                    s.attempt.observe(&usage, completeness);
                                                }
                                                let e = s.attempt.finish().await.err().unwrap_or(e);
                                                if !s.done {
                                                    s.done = true;
                                                    telemetry::emit_failed(
                                                        &s.analytics,
                                                        &s.model,
                                                        &s.request_id,
                                                        ApiService::error_kind(&e),
                                                        ApiService::status_of(&e),
                                                    )
                                                    .await;
                                                }
                                                return Some((Err(e), s));
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        if let Some((usage, completeness)) =
                                            s.decoder.observed_usage()
                                        {
                                            s.attempt.observe(&usage, completeness);
                                        }
                                        let e = s.attempt.finish().await.err().unwrap_or(e);
                                        s.finished = true;
                                        if !s.done {
                                            s.done = true;
                                            telemetry::emit_failed(
                                                &s.analytics,
                                                &s.model,
                                                &s.request_id,
                                                ApiService::error_kind(&e),
                                                ApiService::status_of(&e),
                                            )
                                            .await;
                                        }
                                        return Some((Err(e), s));
                                    }
                                }
                            }
                        }),
                    );
                    return Ok(boxed);
                }
            }
        }
    }

    // ── Inherent streaming entry points ──────────────────────────────────────

    /// Streaming call (provider-neutral). The drive logic of the orchestrator's
    /// `StreamingApiClient::stream` and the subagent's `messages_create_stream`
    /// — `profile` is `None` for the subagent path; `effort` is `None` for the
    /// `StreamingApiClient::stream` path (leaving `req.effort` at its default).
    pub async fn stream(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
        effort: Option<serde_json::Value>,
        speed: Option<String>,
    ) -> Result<BoxStream<'static, Result<LlmEvent, LlmError>>, LlmError> {
        let mut req = self.build_request(model, profile, system, messages, tools, true, None)?;
        req.effort = effort;
        // (fast mode) `Some("fast")` from the main loop lights the fast-mode
        // beta via `beta_context`; `None` keeps the body byte-identical.
        req.speed = speed;
        self.drive_stream(req).await
    }

    /// Structured-output streaming call (provider-neutral). The drive logic of
    /// the subagent's `messages_create_stream_forced`: build the request, attach
    /// `effort`, and force `tool_choice` to the named tool so the model must emit
    /// a matching structured call.
    pub async fn stream_forced(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
        forced_tool: Option<&str>,
        effort: Option<serde_json::Value>,
    ) -> Result<BoxStream<'static, Result<LlmEvent, LlmError>>, LlmError> {
        let mut req = self.build_request(model, profile, system, messages, tools, true, None)?;
        req.effort = effort;
        if let Some(name) = forced_tool {
            req.tool_choice = Some(crate::ToolChoice::Tool {
                name: name.to_string(),
            });
        }
        self.drive_stream(req).await
    }

    /// Like [`Self::stream`], but ALSO threads a per-turn output-token ceiling
    /// and a COGS query-source label onto the WIRE request (the agent crate's
    /// `SubagentApiCallOpts` seam — Fusion panels and any other opts-aware
    /// subagent caller). `max_tokens: None` and `query_source: None` keep the
    /// body byte-identical to [`Self::stream`] (auto-computed ceiling, no
    /// label).
    #[allow(clippy::too_many_arguments)]
    pub async fn stream_with_opts(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
        effort: Option<serde_json::Value>,
        max_tokens: Option<u32>,
        query_source: Option<&str>,
    ) -> Result<BoxStream<'static, Result<LlmEvent, LlmError>>, LlmError> {
        let mut req =
            self.build_request(model, profile, system, messages, tools, true, max_tokens)?;
        req.effort = effort;
        req.query_source = query_source.map(str::to_string);
        self.drive_stream(req).await
    }

    /// Like [`Self::stream_forced`], with the same per-turn output-token
    /// ceiling + COGS query-source label as [`Self::stream_with_opts`].
    #[allow(clippy::too_many_arguments)]
    pub async fn stream_forced_with_opts(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
        forced_tool: Option<&str>,
        effort: Option<serde_json::Value>,
        max_tokens: Option<u32>,
        query_source: Option<&str>,
    ) -> Result<BoxStream<'static, Result<LlmEvent, LlmError>>, LlmError> {
        let mut req =
            self.build_request(model, profile, system, messages, tools, true, max_tokens)?;
        req.effort = effort;
        if let Some(name) = forced_tool {
            req.tool_choice = Some(crate::ToolChoice::Tool {
                name: name.to_string(),
            });
        }
        req.query_source = query_source.map(str::to_string);
        self.drive_stream(req).await
    }

    /// Structured-output streaming call constrained by a JSON SCHEMA rather
    /// than by a forced tool.
    ///
    /// This is claude-code's `sideQuery` shape: the model is told to emit a
    /// document matching `schema`, which on Anthropic rides the
    /// `structured-outputs` beta (`output_config`) and on OpenAI the stable
    /// `response_format` key. Both encodings live in their provider codecs.
    ///
    /// # Errors
    /// Propagates request-building and transport errors, including a provider
    /// whose codec has no encoding for structured output.
    pub async fn stream_json_schema(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        schema: serde_json::Value,
        max_tokens: Option<u32>,
        effort: Option<serde_json::Value>,
    ) -> Result<BoxStream<'static, Result<LlmEvent, LlmError>>, LlmError> {
        let mut req = self.build_request(
            model,
            profile,
            system,
            messages,
            Vec::new(),
            true,
            max_tokens,
        )?;
        // JSON-schema side queries are independent structured-output requests,
        // not forced-tool calls. A parent `--json-schema` turn may have set a
        // session-level `forced_tool_choice`; do not send that choice with the
        // empty tool list used by this request.
        req.tool_choice = None;
        req.effort = effort;
        req.response_format = Some(crate::ResponseFormat::JsonSchema { schema });
        self.drive_stream(req).await
    }

    /// [`Self::stream_json_schema`] with explicit thinking and temperature
    /// semantics (F003 round 2), mirroring
    /// [`Self::messages_create_side_query_stream_with_thinking`].
    ///
    /// Plain `stream_json_schema` never touches `req.reasoning` or
    /// `req.temperature` at all — it just inherits whatever `build_request`
    /// already derives from the LIVE session thinking config. That is
    /// correct for a caller (like the auto-mode propose query) that wants to
    /// inherit the session's thinking decision untouched. It is wrong for a
    /// caller that wants a specific temperature contract (e.g. the fusion
    /// analyst's `temperature: 0.0`): `build_request` defaults session
    /// thinking to `Adaptive` (ON), so setting a bare temperature override on
    /// top of that inherited config could emit BOTH a `thinking` block and a
    /// non-1.0 `temperature` — a pairing every provider that supports
    /// extended thinking rejects.
    ///
    /// Use this method whenever the caller has an opinion about `thinking`
    /// (including "none at all" — pass `None`, which clears any
    /// session-derived reasoning, exactly like the `sidequery` crate's
    /// `SideQueryRequest.thinking: None` convention) and/or a specific
    /// `temperature` contract, rather than raw `stream_json_schema`.
    #[allow(clippy::too_many_arguments)]
    pub async fn stream_json_schema_with_thinking(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        schema: serde_json::Value,
        max_tokens: Option<u32>,
        effort: Option<serde_json::Value>,
        thinking: Option<crate::model::thinking::ThinkingConfig>,
        temperature: Option<f32>,
        query_source: Option<&str>,
    ) -> Result<BoxStream<'static, Result<LlmEvent, LlmError>>, LlmError> {
        let req = self.build_json_schema_request_with_thinking(
            model,
            profile,
            system,
            messages,
            schema,
            max_tokens,
            effort,
            thinking,
            temperature,
            query_source,
        )?;
        self.drive_stream(req).await
    }
}

// ── Media capping (stripExcessMediaItems) ─────────────────────────────────────

/// Maximum media items (images + documents) the API accepts per request.
/// Above this we trim oldest-first. Mirrors TS `API_MAX_MEDIA_PER_REQUEST`
/// (apiLimits.ts:94).
const MAX_MEDIA_PER_REQUEST: usize = 100;

/// (cc 2.1.219) `S8s` — the dispatch-routing opt-in header name.
const DISPATCH_ID_HEADER: &str = "anthropic-dispatch-id";
/// (cc 2.1.219) `Vtp` — the dispatch-routing opt-in header value.
const DISPATCH_ID_V2S: &str = "v2s";
/// `T1e`'s allow-list — the only host `Yd()` accepts as first-party.
const FIRST_PARTY_API_HOST: &str = "api.anthropic.com";

/// True when a nested `tool_result.content` block (a raw JSON value, e.g. an MCP
/// image/resource result) is a media item — `type === "image" || "document"`,
/// matching claude-code `isMedia` (`claude.ts:943`).
fn is_media_value(v: &serde_json::Value) -> bool {
    is_nested_media_value(v)
}

/// Count media (image/document) content blocks across all messages, INCLUDING
/// media NESTED inside `tool_result.content` (the `content_blocks` array MCP
/// image/resource results populate). 1:1 with claude-code `stripExcessMediaItems`
/// counting (`claude.ts:961-971`) — top-level media that ignored the nested
/// channel let an MCP-image-heavy transcript silently exceed the API media cap.
fn count_media(msgs: &[ConversationMessage]) -> usize {
    msgs.iter()
        .map(|m| match m {
            ConversationMessage::User { content, .. }
            | ConversationMessage::Assistant { content, .. } => content
                .iter()
                .map(|b| match b {
                    ContentBlock::Image { .. } | ContentBlock::Document { .. } => 1,
                    ContentBlock::ToolResult {
                        content_blocks: Some(blocks),
                        ..
                    } => blocks.iter().filter(|v| is_media_value(v)).count(),
                    _ => 0,
                })
                .sum::<usize>(),
            ConversationMessage::System { .. } => 0,
        })
        .sum()
}

/// Return `msgs` with the OLDEST media items stripped until at most `limit`
/// remain. 1:1 with claude-code `stripExcessMediaItems` (`claude.ts:975-1014`):
/// for each message (oldest-first), strip media NESTED in `tool_result.content`
/// FIRST (the `.map`, `:982-999`), then TOP-LEVEL media (the `.filter`,
/// `:1000-1006`).
fn strip_excess_media(
    mut msgs: Vec<ConversationMessage>,
    limit: usize,
) -> Vec<ConversationMessage> {
    let total = count_media(&msgs);
    if total <= limit {
        return msgs;
    }
    let mut to_remove = total - limit;
    for m in &mut msgs {
        if to_remove == 0 {
            break;
        }
        let content = match m {
            ConversationMessage::User { content, .. }
            | ConversationMessage::Assistant { content, .. } => content,
            ConversationMessage::System { .. } => continue,
        };
        // (1) Nested-in-tool_result media first (claude-code `.map`).
        for block in content.iter_mut() {
            if to_remove == 0 {
                break;
            }
            if let ContentBlock::ToolResult {
                content_blocks: Some(blocks),
                ..
            } = block
            {
                blocks.retain(|v| {
                    if to_remove > 0 && is_media_value(v) {
                        to_remove -= 1;
                        false
                    } else {
                        true
                    }
                });
            }
        }
        // (2) Top-level media (claude-code `.filter`).
        content.retain(|b| {
            if to_remove > 0
                && (matches!(b, ContentBlock::Image { .. })
                    || matches!(b, ContentBlock::Document { .. }))
            {
                to_remove -= 1;
                false
            } else {
                true
            }
        });
    }
    msgs
}

/// Insert `block` into a content array relative to its `tool_result` blocks.
/// 1:1 port of claude-code `insertBlockAfterToolResults`
/// (`utils/contentArray.ts:21-51`):
///   - if any `tool_result` exists, insert after the LAST one; if that lands the
///     inserted block last, append a `{type:'text', text:'.'}` continuation
///     (some APIs reject a prompt ending in non-text content);
///   - otherwise insert before the last block (`max(0, len-1)`).
/// Mutates `content` in place.
fn insert_block_after_tool_results(
    content: &mut Vec<crate::ContentBlock>,
    block: crate::ContentBlock,
) {
    use crate::ContentBlock as Cb;
    let mut last_tool_result_index: isize = -1;
    for (i, item) in content.iter().enumerate() {
        if matches!(item, Cb::ToolResult { .. }) {
            last_tool_result_index = i as isize;
        }
    }
    if last_tool_result_index >= 0 {
        let insert_pos = (last_tool_result_index as usize) + 1;
        content.insert(insert_pos, block);
        // Append a text continuation if the inserted block is now last.
        if insert_pos == content.len() - 1 {
            content.push(Cb::Text {
                text: ".".to_string(),
                cache_control: None,
            });
        }
    } else {
        // No tool_result blocks — insert before the last block.
        let insert_index = content.len().saturating_sub(1);
        content.insert(insert_index, block);
    }
}

/// 1P experimental cache-editing pass — the `useCachedMC` tail of claude-code
/// `addCacheBreakpoints` (`services/api/claude.ts:3108-3208`). The caller gates
/// this behind `should_use_cache_editing`; here we assume it's armed.
///
/// `enable_caching` mirrors the TS `enablePromptCaching` flag (the
/// `cache_reference`-on-tool_results pass at 3164 is additionally gated on it).
/// `new_edits` = `newCacheEdits.edits`; `pinned` = `pinnedEdits`.
fn apply_cache_editing(
    messages: &mut [crate::Message],
    enable_caching: bool,
    new_edits: &[crate::CacheEdit],
    pinned: &[PinnedCacheEdits],
) {
    use crate::ContentBlock as Cb;

    // Track all cache_references being deleted to prevent duplicates across
    // blocks (claude.ts:3112-3125 seenDeleteRefs + deduplicateEdits).
    let mut seen_delete_refs: std::collections::HashSet<String> = std::collections::HashSet::new();
    let dedup = |edits: &[crate::CacheEdit],
                 seen: &mut std::collections::HashSet<String>|
     -> Vec<crate::CacheEdit> {
        edits
            .iter()
            .filter(|e| {
                let crate::CacheEdit::Delete { cache_reference } = e;
                if seen.contains(cache_reference) {
                    false
                } else {
                    seen.insert(cache_reference.clone());
                    true
                }
            })
            .cloned()
            .collect()
    };

    // Re-insert all previously-pinned cache_edits at their original positions
    // (claude.ts:3127-3139). Only when that message is a `user` message.
    for p in pinned {
        if let Some(msg) = messages.get_mut(p.user_message_index) {
            if msg.role == "user" {
                let deduped = dedup(&p.edits, &mut seen_delete_refs);
                if !deduped.is_empty() {
                    insert_block_after_tool_results(
                        &mut msg.content,
                        Cb::CacheEdits { edits: deduped },
                    );
                }
            }
        }
    }

    // Insert new cache_edits into the LAST user message and (in TS) pin them
    // (claude.ts:3141-3162). LingXi has no cross-call pin store, so the pinning
    // side-effect is a residual — the in-request insertion is faithful.
    if !messages.is_empty() {
        let deduped_new = dedup(new_edits, &mut seen_delete_refs);
        if !deduped_new.is_empty() {
            for i in (0..messages.len()).rev() {
                if messages[i].role == "user" {
                    insert_block_after_tool_results(
                        &mut messages[i].content,
                        Cb::CacheEdits { edits: deduped_new },
                    );
                    break;
                }
            }
        }
    }

    // Add cache_reference to tool_result blocks within the cached prefix
    // (claude.ts:3164-3207). Must run AFTER cache_edits insertion since that
    // modifies content arrays.
    if enable_caching {
        // Find the last message containing a cache_control marker.
        let mut last_cc_msg: isize = -1;
        for (i, msg) in messages.iter().enumerate() {
            for block in &msg.content {
                let has_cc = match block {
                    Cb::Text { cache_control, .. } | Cb::ToolResult { cache_control, .. } => {
                        cache_control.is_some()
                    }
                    _ => false,
                };
                if has_cc {
                    last_cc_msg = i as isize;
                }
            }
        }

        // Stamp `cache_reference = tool_use_id` on tool_results in `user`
        // messages STRICTLY before the last cache_control marker. (TS uses strict
        // "before" to avoid edge cases where cache_edits splicing shifts indices;
        // it also clones rather than mutating in place to avoid contaminating
        // blocks reused by non-cache-editing secondary queries — here each
        // request owns its `messages`, so an in-place set is equivalent.)
        if last_cc_msg >= 0 {
            for i in 0..(last_cc_msg as usize) {
                if messages[i].role != "user" {
                    continue;
                }
                for block in &mut messages[i].content {
                    if let Cb::ToolResult {
                        tool_call_id,
                        cache_reference,
                        ..
                    } = block
                    {
                        *cache_reference = Some(tool_call_id.clone());
                    }
                }
            }
        }
    }
}

/// Generate a short client-side request id (same alphabet as api-client).
///
/// **Header name**: `x-request-id` — sourced from `api-client/src/anthropic.rs:868`.
#[must_use]
fn new_request_id() -> String {
    use rand::Rng;
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-";
    let mut rng = rand::rng();
    (0..16)
        .map(|_| CHARSET[rng.random_range(0..CHARSET.len())] as char)
        .collect()
}

#[cfg(test)]
#[path = "service_test.rs"]
mod service_test;
