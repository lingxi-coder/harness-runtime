//! Provider-neutral API service: drive the LLM client with full
//! retry/rate-limit/betas.
//!
//! `ApiService` owns the inherent drive loop relocated from the orchestrator's
//! `ProviderApiAdapter` (`orchestrator/src/provider_adapter.rs`). It speaks
//! `lingxi_core::types::ConversationMessage` + llm-runtime types and references only
//! `crate::*` + `protocol`/`traits`/`telemetry` — never any orchestrator-internal
//! path. The orchestrator's consumer-trait impls delegate to it 1:1.

use crate::agent_prompt_cache_ttl_override;
use crate::convert::{
    ensure_tool_result_pairing_with_sources,
    normalize_messages_for_api_with_tool_search_and_sources, to_llm_messages, to_tool_declarations,
    ConversationMessagesWithSources,
};
use crate::dispatch_header::{
    DispatchAttempt, DispatchFailure, DispatchFallback, DispatchHeaderState, DISPATCH_ID_HEADER,
};
use crate::model::betas::{
    apply_beta_header_with_auth_and_custom, bedrock_extra_body_betas, BetaContext, Endpoint,
    Provider, FAST_MODE,
};
use crate::model::rate_limit::{
    formatted_reset_times_from_decoded, rate_limit_error_message, RateLimitInfo, RawUtilization,
    SubscriptionContext,
};
use crate::model::retry::{
    resolve_retry_control_with_settings, DriveStep, ResolveRetryEnv, RetryControl, RetryState,
};
use crate::model::retry_scope::ModelCallRetryScope;
use crate::model::telemetry;
use crate::model::user_agent::{user_agent, UserAgentEnv};
use crate::{
    CostEstimator, HistoryEvent, HistoryResponse, LlmError, LlmRequest, MediaRoute, ModelRuntime,
    PromptCacheQuerySource, ResponsesSession, Transport,
};

fn env_truthy(name: &str) -> bool {
    std::env::var(name).ok().is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn native_prompt_cache_bare_mode() -> bool {
    if env_truthy(branding::SIMPLE_ENV) {
        return true;
    }
    for argument in std::env::args_os() {
        if argument == std::ffi::OsStr::new("--") {
            break;
        }
        if argument == std::ffi::OsStr::new("--bare") {
            return true;
        }
    }
    false
}

fn native_anthropic_unix_socket_enabled() -> bool {
    std::env::var_os("ANTHROPIC_UNIX_SOCKET").is_some_and(|value| !value.is_empty())
}

fn custom_system_prompt(
    text: Option<&str>,
) -> Option<lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput> {
    text.map(|text| {
        native_custom_system_prompt(
            lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_string(text),
        )
    })
}

/// Convert a Host `customSystemPrompt` string at its Native input boundary.
/// Native `MDo` applies `hwe` only to this string field; `overrideSystemPrompt`
/// and already-typed source vectors must not pass through this splitter.
pub fn native_custom_system_prompt(
    text: lingxi_llm_client::providers::anthropic::system_prompt::PromptText,
) -> lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput {
    use lingxi_llm_client::providers::anthropic::system_prompt::{
        PromptText, SystemPromptInput, DYNAMIC_BOUNDARY,
    };

    let units = text.utf16_code_units();
    let lines = units
        .split(|unit| *unit == b'\n' as u16)
        .collect::<Vec<_>>();
    let marker_units = DYNAMIC_BOUNDARY.encode_utf16().collect::<Vec<_>>();
    let Some(boundary) = lines
        .iter()
        .position(|line| js_trim_whitespace(line) == marker_units.as_slice())
    else {
        return SystemPromptInput::native_custom_prompt(text.clone(), vec![text]);
    };

    let mut elements = Vec::with_capacity(3);
    let mut before = join_js_split_lines(&lines[..boundary]);
    before.push(b'\n' as u16);
    if !js_trim_whitespace(&before).is_empty() {
        elements.push(PromptText::from_utf16(before));
    }
    elements.push(PromptText::from_string(DYNAMIC_BOUNDARY));

    let mut after = vec![b'\n' as u16];
    after.extend(join_js_split_lines(&lines[boundary + 1..]));
    if !js_trim_whitespace(&after).is_empty() {
        elements.push(PromptText::from_utf16(after));
    }

    SystemPromptInput::native_custom_prompt(text, elements)
}

fn join_js_split_lines(lines: &[&[u16]]) -> Vec<u16> {
    let capacity = lines.iter().map(|line| line.len()).sum::<usize>() + lines.len();
    let mut joined = Vec::with_capacity(capacity);
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            joined.push(b'\n' as u16);
        }
        joined.extend_from_slice(line);
    }
    joined
}

fn js_trim_whitespace(mut units: &[u16]) -> &[u16] {
    while units.first().is_some_and(|unit| is_js_whitespace(*unit)) {
        units = &units[1..];
    }
    while units.last().is_some_and(|unit| is_js_whitespace(*unit)) {
        units = &units[..units.len() - 1];
    }
    units
}

fn is_js_whitespace(unit: u16) -> bool {
    matches!(
        unit,
        0x0009
            | 0x000a
            | 0x000b
            | 0x000c
            | 0x000d
            | 0x0020
            | 0x00a0
            | 0x1680
            | 0x2028
            | 0x2029
            | 0x202f
            | 0x205f
            | 0x3000
            | 0xfeff
    ) || (0x2000..=0x200a).contains(&unit)
}

use futures::stream::BoxStream;
use lingxi_core::types::{is_nested_media_value, ContentBlock, ConversationMessage};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

tokio::task_local! {
    static MOD_REQUEST_EFFORT: serde_json::Value;
}

/// Apply a Mod's effort override to requests assembled by this task only.
/// The ordinary session request and concurrent side-query tasks keep their
/// own effort setting.
pub async fn with_mod_request_effort<T>(
    effort: &str,
    future: impl std::future::Future<Output = T>,
) -> T {
    MOD_REQUEST_EFFORT
        .scope(serde_json::Value::String(effort.to_owned()), future)
        .await
}

/// Mirror claude-code `getPromptCachingEnabled` (services/api/claude.ts:333).
///
/// Collapse a [`ReasoningConfig`] to a numeric budget for telemetry labels.
/// `Adaptive` → 0, `Enabled{b}` → b.
fn reasoning_budget(thinking: Option<&lingxi_llm_client::protocol::ThinkingConfig>) -> u32 {
    match thinking.and_then(|thinking| thinking.budget) {
        Some(lingxi_llm_client::protocol::ThinkingBudget::Tokens(tokens)) => tokens,
        _ => 0,
    }
}

/// Remove provider-authenticated assistant blocks before retrying on a different
/// model. Their signatures are scoped to the model that produced them and must
/// never be replayed to a fallback provider/model.
fn strip_signature_blocks(messages: &mut [lingxi_llm_client::protocol::ConversationMessage]) {
    for message in messages
        .iter_mut()
        .filter(|message| message.role == lingxi_llm_client::protocol::MessageRole::Assistant)
    {
        message.content.retain(|block| {
            !matches!(
                block,
                lingxi_llm_client::protocol::ContentBlock::Thinking { .. }
                    | lingxi_llm_client::protocol::ContentBlock::RedactedThinking { .. }
            ) && !matches!(block, lingxi_llm_client::protocol::ContentBlock::ProviderContent { value, .. }
                if value["type"] == "connector_text")
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
/// Keep the logical call's ordinary retry count across endpoint changes.
/// Only the consecutive-overload counter is specific to this connection.
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
    req.input.model.clone_from(&hop.request_model);
    state.consecutive_overloaded = 0;
    Some(hop.profile_name.clone())
}

fn strip_signature_blocks_for_fallback(
    messages: &mut [lingxi_llm_client::protocol::ConversationMessage],
) {
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

/// The SDK classifies provider execution state; the host combines that fact
/// with its transport, settlement, and retry policy before any repeat dispatch.
fn allows_automatic_replay(request: &LlmRequest) -> bool {
    if request.execution.computer_submission.is_some()
        || request.execution.expected_computer_binding.is_some()
        || request.execution.input_protocol == Some(crate::ProtocolFamily::GeminiInteractions)
    {
        return false;
    }
    lingxi_llm_client::execution_safety::request_replay_safety(&request.input)
        == lingxi_llm_client::execution_safety::RequestReplaySafety::Stateless
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

/// Ordered model fallback policy for one logical non-streaming request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum FallbackPolicy {
    /// Keep the selected model for this logical request.
    #[default]
    Disabled,
    /// Resolve the selected model's settings, then the global settings.
    Configured,
    /// Use exactly this ordered model chain, including an explicitly empty chain.
    Models(Vec<String>),
}

impl FallbackPolicy {
    /// Parse the current CLI's ordered comma-separated model list.
    #[must_use]
    pub fn from_models_csv(models: &str) -> Self {
        Self::Models(parse_fallback_chain(models))
    }
}

/// Controls consumed while assembling and executing a main request.
#[derive(Debug, Clone, Default)]
pub struct MessagesCreateOptions {
    /// Output ceiling, applied before model-aware reasoning and context bounds.
    pub max_output_tokens: Option<u32>,
    /// Anthropic context-hint offer; the codec and beta policy consume it.
    pub context_hint: Option<serde_json::Value>,
    /// Activate the hint beta independently of whether an offer meets its floor.
    pub context_hint_beta: bool,
    /// Prior streaming overload count. `Some(0)` identifies a stream fallback.
    pub initial_consecutive_overloaded: Option<u8>,
    /// Ordered fallback policy for this logical request.
    pub fallback: FallbackPolicy,
    /// Trusted host accounting authority; never enters model input.
    pub model_attempt: Option<lingxi_core::host::ModelAttemptContext>,
    /// Sanitized Native `querySource` used by cache-TTL policy and accounting.
    pub query_source: Option<String>,
    /// Trusted elapsed-time fact from the failed streaming request.
    pub failed_stream_outlasted_timeout: bool,
    /// Native `skipGlobalCacheForSystemPrompt`, computed from registered active
    /// tools before their provider schemas are flattened.
    pub skip_global_cache_for_system_prompt: bool,
    /// Host-owned admission checked immediately before SDK transport dispatch.
    pub request_dispatch_admission: Option<crate::RequestDispatchAdmission>,
}

/// Current owned non-streaming main-message request.
#[derive(Debug, Clone)]
pub struct MessagesCreateRequest {
    pub model: String,
    pub profile: Option<String>,
    pub system: Option<lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
    pub messages: Vec<ConversationMessage>,
    pub tools: Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    pub opts: MessagesCreateOptions,
}

impl MessagesCreateRequest {
    /// Own the assembled conversation and selected route for one logical call.
    #[must_use]
    pub fn new(
        model: &str,
        profile: Option<&str>,
        system: Option<lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        messages: Vec<ConversationMessage>,
        tools: Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    ) -> Self {
        Self {
            model: model.to_owned(),
            profile: profile.map(str::to_owned),
            system,
            messages,
            tools,
            opts: MessagesCreateOptions::default(),
        }
    }
}

/// Header classification independent of a canonical request's body policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonStreamingRequestClass {
    Main,
    Auxiliary,
}

/// Retry controls shared by main and isolated canonical requests.
#[derive(Debug, Clone, Default)]
pub struct NonStreamingRetryOptions {
    pub initial_consecutive_overloaded: Option<u8>,
    pub fallback: FallbackPolicy,
}

// ── Stream state (used in drive_stream unfold) ────────────────────────────────

/// State threaded through the `futures::stream::unfold` loop in `drive_stream`.
struct StreamState {
    per_turn_effort: Option<String>,
    safety: crate::safety_observation::SafetyObservation,
    attempt: crate::model_attempt::WireAttempt,
    decoder: crate::history_projection::HistoryProjector,
    frames: lingxi_llm_client::ModelStream,
    pricing: Option<lingxi_llm_client::FrozenPricing>,
    pricing_model: crate::PricingModelRef,
    server_fallback_lane:
        Option<lingxi_llm_client::providers::anthropic::fallback_request::ServerLane>,
    server_fallback_quote_finalized: bool,
    server_fallback_quote_candidate_seen: bool,
    server_fallback_quote_metadata: Option<serde_json::Value>,
    server_fallback_quote_estimate: Option<crate::CostEstimate>,
    pending_service_error: Option<LlmError>,
    /// First frame already pulled by the drive loop's dispatch body-phase
    /// lookahead.
    /// Consumed in place of the first `next_frame()` so decoding, watchdog and
    /// error handling stay identical to the un-seeded path. `None` on every
    /// stream that did not carry `anthropic-dispatch-id`.
    seed: Option<Result<Option<lingxi_llm_client::StreamBatch>, LlmError>>,
    queue: VecDeque<HistoryEvent>,
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

fn server_fallback_quote_envelope(
    quote: Option<&lingxi_llm_client::AnthropicFallbackCostQuote>,
    summary_model: Option<&str>,
    complete: bool,
    reason: Option<&str>,
) -> serde_json::Value {
    let mut value = serde_json::json!({
        "kind": "anthropic_server_fallback_per_iteration",
        "completeness": if complete { "complete" } else { "incomplete" },
        "summaryModel": summary_model,
        "quote": quote.and_then(|quote| serde_json::to_value(quote).ok()),
    });
    if let Some(reason) = reason {
        value["reason"] = serde_json::Value::String(reason.into());
    }
    value
}

struct ServerFallbackQuoteProjection {
    estimate: Option<crate::CostEstimate>,
    metadata: serde_json::Value,
    summary_model: Option<String>,
}

/// `Some` means native cNe's served-fallback branch ran. The estimate may still
/// be absent when the frozen profile cannot price every native component; that
/// must suppress the ordinary dispatched-model aggregate estimate.
fn frozen_server_fallback_quote(
    pricing: Option<&lingxi_llm_client::FrozenPricing>,
    pricing_model: &crate::PricingModelRef,
    lane_model: &str,
    fallback: Option<&lingxi_llm_client::providers::anthropic::fallback_response::FallbackResponse>,
    inference: &lingxi_llm_client::protocol::InferenceReport,
) -> Option<ServerFallbackQuoteProjection> {
    use lingxi_llm_client::protocol::Submission;

    let facts = fallback?;
    let iterations = facts.iterations.as_ref()?;
    // This is the native cNe branch predicate. Some("") is intentionally
    // distinct from no model-bearing fallback iteration.
    iterations.served_fallback_model.as_ref()?;
    let resolved_summary_model = lingxi_llm_client::resolve_anthropic_server_fallback_summary_model(
        Some(lane_model),
        iterations,
    );
    let stop_reason = facts.final_stop_reason.as_deref();
    let Some(pricing) = pricing else {
        return Some(ServerFallbackQuoteProjection {
            estimate: None,
            metadata: server_fallback_quote_envelope(
                None,
                resolved_summary_model.as_deref(),
                false,
                Some("frozen_pricing_unavailable"),
            ),
            summary_model: resolved_summary_model,
        });
    };
    match pricing.estimate_anthropic_server_fallback(
        iterations,
        Some(lane_model),
        stop_reason,
        inference,
        Submission::default(),
    ) {
        Ok(Some(quote)) => {
            let summary_model = Some(quote.summary_model.clone());
            let estimate = quote
                .clone()
                .into_cost_estimate_if_complete()
                .and_then(|estimate| {
                    crate::cost::project_fallback_estimate(estimate, pricing_model).ok()
                });
            let complete = estimate.is_some();
            let reason = (!complete).then_some("native_quote_incomplete");
            Some(ServerFallbackQuoteProjection {
                estimate,
                metadata: server_fallback_quote_envelope(
                    Some(&quote),
                    summary_model.as_deref(),
                    complete,
                    reason,
                ),
                summary_model,
            })
        }
        Ok(None) => Some(ServerFallbackQuoteProjection {
            estimate: None,
            metadata: server_fallback_quote_envelope(
                None,
                resolved_summary_model.as_deref(),
                false,
                Some("native_quote_unavailable"),
            ),
            summary_model: resolved_summary_model,
        }),
        Err(_) => Some(ServerFallbackQuoteProjection {
            estimate: None,
            metadata: server_fallback_quote_envelope(
                None,
                resolved_summary_model.as_deref(),
                false,
                Some("native_quote_failed"),
            ),
            summary_model: resolved_summary_model,
        }),
    }
}

/// A stream may expose iteration facts before its terminal stop reason and
/// final iteration array arrive. Publish only a retractable marker at that
/// point; pricing waits for the terminal snapshot so refusal exclusion and
/// explicit empty-array replacement use the final facts.
fn server_fallback_quote_candidate(
    lane_model: &str,
    fallback: Option<&lingxi_llm_client::providers::anthropic::fallback_response::FallbackResponse>,
) -> Option<ServerFallbackQuoteProjection> {
    let iterations = fallback?.iterations.as_ref()?;
    iterations.served_fallback_model.as_ref()?;
    let summary_model = lingxi_llm_client::resolve_anthropic_server_fallback_summary_model(
        Some(lane_model),
        iterations,
    );
    Some(ServerFallbackQuoteProjection {
        estimate: None,
        metadata: server_fallback_quote_envelope(
            None,
            summary_model.as_deref(),
            false,
            Some("awaiting_terminal_fallback_facts"),
        ),
        summary_model,
    })
}

fn attach_frozen_stream_quote(events: &mut [HistoryEvent], quote: Option<&crate::CostEstimate>) {
    for event in events {
        if let HistoryEvent::MessageDelta {
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
    /// No server id header was present, so the outgoing `x-client-request-id`
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

type EffortSettingsSource =
    Arc<dyn Fn() -> Vec<lingxi_core::host::effort::EffortSettingsLayer> + Send + Sync>;
type PromptCacheTtlSettingsSource =
    Arc<dyn Fn() -> lingxi_core::settings::schema::PromptCacheTtlSettings + Send + Sync>;

fn native_prompt_cache_1h_allowlist() -> Vec<String> {
    static LATCH: OnceLock<Mutex<Option<Vec<String>>>> = OnceLock::new();
    let latch = LATCH.get_or_init(|| Mutex::new(None));
    let mut guard = latch.lock().unwrap_or_else(|error| error.into_inner());
    guard
        .get_or_insert_with(|| {
            ::telemetry::flag_string_list(
                lingxi_llm_client::providers::anthropic::system_prompt::PROMPT_CACHE_1H_ALLOWLIST_FEATURE,
                lingxi_llm_client::providers::anthropic::system_prompt::PROMPT_CACHE_1H_ALLOWLIST_DEFAULT,
            )
        })
        .clone()
}

fn sdk_prompt_cache_ttl_settings(
    settings: lingxi_core::settings::schema::PromptCacheTtlSettings,
) -> lingxi_llm_client::providers::anthropic::system_prompt::PromptCacheTtlSettings {
    use lingxi_core::settings::schema::PromptCacheTtl as HostTtl;
    use lingxi_llm_client::providers::anthropic::system_prompt::PromptCacheTtl as SdkTtl;

    let convert = |ttl: Option<HostTtl>| {
        ttl.map(|ttl| match ttl {
            HostTtl::FiveMinutes => SdkTtl::FiveMinutes,
            HostTtl::OneHour => SdkTtl::OneHour,
        })
    };
    lingxi_llm_client::providers::anthropic::system_prompt::PromptCacheTtlSettings {
        prompt_cache_ttl: convert(settings.main),
        subagent_prompt_cache_ttl: convert(settings.subagent),
    }
}

/// Admission-time fallback for a new unscoped request. A captured request or
/// registered origin always takes precedence and retains its ID through retries.
pub type RequestSessionIdSource =
    Arc<dyn Fn() -> crate::BoxFuture<'static, Option<String>> + Send + Sync>;

/// Production service: drives `ModelRuntime` with full retry/rate-limit/betas.
pub struct ApiService {
    fast_policy: crate::model::fast_admission::PolicySource,
    fast_availability: Arc<crate::model::fast_admission::Availability>,
    account_change_observer: Arc<ApiServiceAccountObserver>,
    account_change_observer_keepalive: Vec<Arc<dyn lingxi_core::host::auth::AccountChangeObserver>>,
    effort_settings_source: Option<EffortSettingsSource>,
    inherited_effort_settings: Option<Vec<lingxi_core::host::effort::EffortSettingsLayer>>,
    prompt_cache_ttl_settings_source: Option<PromptCacheTtlSettingsSource>,
    effort_table_options: lingxi_core::host::effort_table::TableOptions,
    session_effort: RwLock<lingxi_core::host::effort_table::SessionEffort>,
    model_attempt_hooks: RwLock<Option<Arc<dyn crate::ModelAttemptHooks>>>,
    client: Arc<ModelRuntime>,
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
    subscription: Option<lingxi_core::host::subscription::SharedSubscription>,
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
    request_session_id_source: RwLock<Option<RequestSessionIdSource>>,
    /// 1P experimental cache-editing inputs (claude.ts `addCacheBreakpoints`
    /// `newCacheEdits`/`pinnedEdits`, claude.ts:3068-3069). LingXi has no
    /// cached-microcompact scheduler to produce these, so the default is
    /// `None`/empty — the gate-armed `cache_reference`-on-tool_results pass
    /// (the directly-exercised behavior) still runs from `req.input.messages`. Set
    /// only by [`Self::with_cache_editing_inputs`] (test-only today); wiring a
    /// real producer is residual. See module note.
    cache_editing_inputs: CacheEditingInputs,
    /// User-agent environment snapshot (Task 3).
    ua: UserAgentEnv,
    /// Build version string for the User-Agent header.
    version: String,
    /// Explicit Anthropic compatibility version. Other providers retain the
    /// product build version and their own User-Agent branding.
    anthropic_compatible_version: Option<String>,
    native_system_prefix:
        Option<lingxi_llm_client::providers::anthropic::system_prompt::NativeSystemPrefix>,
    native_thinking_display:
        Option<lingxi_llm_client::providers::anthropic::thinking_display::ThinkingDisplayPolicy>,
    anthropic_client_metadata:
        Option<lingxi_llm_client::providers::anthropic::request_policy::AnthropicClientMetadata>,
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
    /// `client.available_models()`. Used by configured model fallback
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
    /// Optional cost estimator for populating `HistoryResponse.cost`.
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
    /// Scope-keyed Native overage facts and the auth generation they belong to.
    prompt_cache_overage: Arc<PromptCacheOverageState>,
    /// The request id of the most recently recorded response, with its origin,
    /// captured in [`Self::record_rate_limit_from_headers_for_route`] (the stream
    /// connect-success + non-stream header pass). Read via the
    /// `last_request_id()` trait method to stamp the persisted assistant line's
    /// top-level `requestId`. The value is the provider's server-side id when a
    /// known id header is present ([`RequestIdOrigin::Server`]); otherwise it
    /// falls back to the outgoing `x-client-request-id` we sent
    /// ([`RequestIdOrigin::Client`]) when one exists. That fallback is
    /// correlation-only and is NOT valid for provider-side log lookups. `None`
    /// until a response is recorded or when neither ID is available.
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
    /// parse in `record_rate_limit_from_headers_for_route` — claude-code assigns
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
    /// [`Self::record_rate_limit_from_429_for_route`] — Anthropic's composed limits copy
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
    /// recovers. [`Self::record_rate_limit_from_429_for_route`] writes this slot on
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
    /// [`crate::auth::external_aws::AWS_AUTH_MAX_ATTEMPTS`] (`Ygf = 2`). `None` (the
    /// default) keeps every error path unchanged. Provider-gated inside
    /// [`crate::auth::external_aws::is_aws_auth_error`] — non-AWS providers never
    /// reach the refresh.
    aws_auth: Option<Arc<dyn crate::auth::external_aws::AwsAuthRefresh>>,
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
    responses_ws_session: tokio::sync::Mutex<ResponsesSession>,
}

impl DispatchHeaderState {
    fn for_query_source(query_source: Option<&str>) -> Self {
        Self {
            auxiliary: query_source_category(query_source) == Some(QuerySourceCategory::Auxiliary),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuerySourceCategory {
    Main,
    Subagent,
    Auxiliary,
}

/// Native 2.1.287 `Ss`, src_180615120.js bytes [247071,247246).
fn query_source_category(source: Option<&str>) -> Option<QuerySourceCategory> {
    source.map(|source| {
        if source.starts_with("repl_main_thread") || source == "sdk" {
            QuerySourceCategory::Main
        } else if source.starts_with("agent:") || source == "hook_agent" {
            QuerySourceCategory::Subagent
        } else {
            QuerySourceCategory::Auxiliary
        }
    })
}

fn prompt_cache_query_source(source: Option<&str>) -> PromptCacheQuerySource<'_> {
    source.map_or(
        PromptCacheQuerySource::Unspecified,
        PromptCacheQuerySource::Named,
    )
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

#[derive(Default)]
struct PromptCacheOverageState {
    state: Mutex<PromptCacheOverageSnapshot>,
}

#[derive(Default)]
struct PromptCacheOverageSnapshot {
    account_epoch: u64,
    by_scope: std::collections::HashMap<crate::CredentialScope, PromptCacheOverageObservation>,
}

#[derive(Clone, Copy)]
struct PromptCacheOverageObservation {
    is_using_overage: bool,
    observed_at_ms: u128,
}

impl PromptCacheOverageState {
    fn account_epoch(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .account_epoch
    }

    fn reset_for_account_change(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.account_epoch = state.account_epoch.wrapping_add(1);
        state.by_scope.clear();
    }

    fn is_using_overage(&self, scope: &crate::CredentialScope, account_epoch: u64) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.account_epoch != account_epoch {
            return false;
        }
        state
            .by_scope
            .get(scope)
            .is_some_and(|observation| observation.is_using_overage)
    }

    fn record(
        &self,
        scope: &crate::CredentialScope,
        account_epoch: u64,
        is_using_overage: bool,
        observed_at_ms: u128,
    ) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.account_epoch != account_epoch
            || state
                .by_scope
                .get(scope)
                .is_some_and(|previous| observed_at_ms < previous.observed_at_ms)
        {
            return false;
        }
        state.by_scope.insert(
            scope.clone(),
            PromptCacheOverageObservation {
                is_using_overage,
                observed_at_ms,
            },
        );
        true
    }
}

struct ApiServiceAccountObserver {
    fast_availability: Arc<crate::model::fast_admission::Availability>,
    prompt_cache_overage: Arc<PromptCacheOverageState>,
}

impl lingxi_core::host::auth::AccountChangeObserver for ApiServiceAccountObserver {
    fn account_changed(&self) {
        self.fast_availability.account_changed();
        self.prompt_cache_overage.reset_for_account_change();
    }
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

pub(crate) fn extra_body_object(
) -> Result<Option<serde_json::Map<String, serde_json::Value>>, LlmError> {
    extra_body_object_uncached()
        .map(lingxi_llm_client::providers::anthropic::request_policy::sanitize_extra_body)
        .transpose()
        .map_err(crate::upstream::error)
}

fn extra_body_object_uncached() -> Option<serde_json::Map<String, serde_json::Value>> {
    let Ok(t) = std::env::var("CLAUDE_CODE_EXTRA_BODY") else {
        return None;
    };
    if t.is_empty() {
        return None;
    }
    match serde_json::from_str::<serde_json::Value>(t.strip_prefix('\u{FEFF}').unwrap_or(&t)) {
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
    pub fn supports_hosted_search(&self, model: &str, profile: Option<&str>) -> bool {
        self.client
            .search_profile(model, profile)
            .is_some_and(lingxi_llm_client::hosted_search::supports)
    }
    pub fn supports_hosted_search_config(
        &self,
        model: &str,
        profile: Option<&str>,
        config: &lingxi_llm_client::protocol::WebSearchConfig,
    ) -> bool {
        self.client
            .search_profile(model, profile)
            .is_some_and(|profile| {
                lingxi_llm_client::hosted_search::supports_with_config(profile, config)
            })
    }
    pub fn hosted_search_max_uses(&self, model: &str, profile: Option<&str>) -> Option<u32> {
        self.client
            .search_profile(model, profile)
            .filter(|p| {
                p.extra
                    .get("web_search")
                    .and_then(serde_json::Value::as_str)
                    == Some("anthropic")
            })
            .map(|_| 8)
    }

    pub fn transport(&self) -> Arc<dyn Transport> {
        self.transport.clone()
    }
    /// Execute a canonical side-query request through the same host hooks and
    /// retry driver as the high-level side-query entry points.
    pub async fn execute_side_query_request(
        &self,
        mut request: LlmRequest,
    ) -> Result<HistoryResponse, LlmError> {
        request.stream = false;
        request.execution.anthropic_request_kind = lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery;
        let control = resolve_retry_control_with_settings(
            &request.input.model,
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
    ) -> Result<HistoryResponse, LlmError> {
        request.stream = false;
        request.execution.anthropic_request_kind = lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery;
        let mut control = resolve_retry_control_with_settings(
            &request.input.model,
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
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        request.stream = true;
        self.drive_stream(request).await
    }

    /// Stream a Host subagent request; its COGS label remains separate from
    /// the typed subagent role used by prompt-cache policy.
    #[allow(clippy::too_many_arguments)]
    pub async fn stream_with_attempt_opts(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
        forced_tool: Option<&str>,
        effort: Option<serde_json::Value>,
        max_tokens: Option<u32>,
        query_source_label: Option<&str>,
        model_attempt: Option<lingxi_core::host::ModelAttemptContext>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        let system = custom_system_prompt(system);
        let mut request = self.build_request(
            model,
            profile,
            system.as_ref(),
            messages,
            tools,
            true,
            max_tokens,
            false,
            PromptCacheQuerySource::Subagent,
        )?;
        request.set_effort(effort)?;
        request.execution.query_source = query_source_label.map(str::to_string);
        request.execution.model_attempt = model_attempt;
        // Child tool choice belongs to this request, independently of a
        // shared service's main-loop structured-output configuration.
        request
            .set_tool_choice(forced_tool.map(|name| crate::ToolChoice::Tool { name: name.into() }));
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
        let Some(context) = request.execution.model_attempt.as_ref() else {
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
    /// Protocol selected for the model's canonical request and history adapter.
    pub fn protocol_for_model(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<crate::ProtocolFamily, LlmError> {
        self.client.protocol_for_model(model, profile)
    }

    pub fn resolve_media_route(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<MediaRoute, LlmError> {
        self.client.resolve_media_route(model, profile)
    }

    /// Redacted source from the selected route's host credential resolver.
    pub async fn credential_source(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<crate::CredentialSource, LlmError> {
        self.client.credential_source(model, profile).await
    }

    /// Native first-party route identity, without credential or network work.
    pub fn is_first_party_route(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<bool, LlmError> {
        self.client.is_first_party_route(model, profile)
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
        req.execution.side_thinking_disabled = matches!(
            thinking,
            Some(crate::model::thinking::ThinkingConfig::Disabled)
        );
        req.set_reasoning(thinking.and_then(|thinking| {
            crate::model::thinking::reasoning_for_request(thinking, model, req.input.max_tokens)
        }));
        req.input.temperature = temperature.or_else(|| {
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
    /// `HistoryResponse.cost` via the llm-runtime `CostEstimator`.  Pass
    /// `None` to leave ordinary cost estimation disabled. Registered attempts
    /// always retain a frozen quote for their host accounting hooks.
    #[must_use]
    pub fn new(
        client: Arc<ModelRuntime>,
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
    /// call this instead of [`Self::new`] to get live `HistoryResponse.cost` values.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_estimator(
        client: Arc<ModelRuntime>,
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
        client: Arc<ModelRuntime>,
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
        // Build alias→display map once so configured model fallback can
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
        let fast_availability = Arc::new(crate::model::fast_admission::Availability::default());
        let prompt_cache_overage = Arc::new(PromptCacheOverageState::default());
        let account_change_observer = Arc::new(ApiServiceAccountObserver {
            fast_availability: fast_availability.clone(),
            prompt_cache_overage: prompt_cache_overage.clone(),
        });
        Self {
            fast_policy: Arc::new(crate::model::fast_admission::Policy::default),
            fast_availability,
            account_change_observer: account_change_observer.clone(),
            account_change_observer_keepalive: vec![account_change_observer],
            client,
            model_attempt_hooks: RwLock::new(None),
            effort_settings_source: None,
            inherited_effort_settings: None,
            prompt_cache_ttl_settings_source: None,
            effort_table_options: Default::default(),
            session_effort: RwLock::new(Default::default()),
            transport,
            subscriber,
            subscription: None,
            retry_reporter: None,
            forced_tool_choice: None,
            thinking: RwLock::new(crate::model::thinking::ThinkingConfig::default()),
            request_metadata: None,
            request_session_id_source: RwLock::new(None),
            cache_editing_inputs: CacheEditingInputs::default(),
            ua,
            version: version.into(),
            anthropic_compatible_version: None,
            anthropic_client_metadata: None,
            native_thinking_display: None,
            native_system_prefix: None,
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
            prompt_cache_overage,
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
            responses_ws_session: tokio::sync::Mutex::new(ResponsesSession::new()),
        }
    }

    /// Attach the AWS auth-refresh driver (2.1.198 `awsAuthRefresh` flow).
    /// Builder-style; the default is `None` (no refresh, errors stay terminal).
    #[must_use]
    pub fn with_aws_auth(
        mut self,
        aws_auth: Arc<dyn crate::auth::external_aws::AwsAuthRefresh>,
    ) -> Self {
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
        slot: lingxi_core::host::subscription::SharedSubscription,
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
    /// Called immediately before each retry sleep. Capacity waits under the
    /// watchdog report their separate ordinal; other retries report the
    /// ordinary attempt number against `ctl.max_retries`.
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
                attempt: if ctl.watchdog
                    && matches!(
                        error,
                        LlmError::Overloaded { .. } | LlmError::RateLimited { .. }
                    ) {
                    state.watchdog_capacity_waits
                } else {
                    state.attempt
                },
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

    /// Install the host's admitted settings source. It is sampled before each
    /// physical preparation, so edits and route fallback use current caps.
    /// An empty result clears caps and never triggers ambient settings loading.
    #[must_use]
    pub fn with_effort_settings_source(mut self, source: EffortSettingsSource) -> Self {
        self.inherited_effort_settings = Some(source());
        self.effort_settings_source = Some(source);
        self
    }

    /// Install the host's merged current main/subagent cache-TTL settings.
    /// The source is sampled while building each request so file, CLI, managed,
    /// and mobile settings updates reach the SDK's Native TTL resolver.
    #[must_use]
    pub fn with_prompt_cache_ttl_settings_source(
        mut self,
        source: PromptCacheTtlSettingsSource,
    ) -> Self {
        self.prompt_cache_ttl_settings_source = Some(source);
        self
    }

    /// Supply the native first-start/managed override state without ambient discovery.
    #[must_use]
    pub fn with_effort_table_options(
        mut self,
        options: lingxi_core::host::effort_table::TableOptions,
    ) -> Self {
        self.effort_table_options = options;
        self
    }

    pub fn set_session_effort(&self, effort: lingxi_core::host::effort_table::SessionEffort) {
        *self
            .session_effort
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = effort;
    }

    /// Current route/model Fast gate, without authentication or transport.
    pub fn fast_model_allowed(&self, model: &str, profile: Option<&str>) -> Result<bool, LlmError> {
        if !self.client.fast_model_allowed(model, profile)? {
            return Ok(false);
        }
        let account = self.client.fast_account_identity(model, profile)?;
        let policy = (self.fast_policy)();
        let (org, oauth) = self.fast_availability.observed(&account.profile, &policy);
        let inputs = policy.inputs(&account, org, oauth, !self.interactive_session_for_policy());
        Ok(lingxi_core::host::fast_mode::decline(&inputs).is_none())
    }

    fn interactive_session_for_policy(&self) -> bool {
        self.interactive_session.unwrap_or_else(|| {
            !lingxi_core::host::session_flags::effective_non_interactive_session()
        })
    }

    /// Live root settings and managed-policy facts. No model-input field can
    /// manufacture organization admission.
    #[must_use]
    pub fn with_fast_policy_source(
        mut self,
        source: crate::model::fast_admission::PolicySource,
    ) -> Self {
        self.fast_policy = source;
        self
    }

    pub fn account_change_observer(
        &self,
    ) -> Arc<dyn lingxi_core::host::auth::AccountChangeObserver> {
        self.account_change_observer.clone()
    }

    /// Keep an account observer alive while the auth owner retains only its
    /// weak callback. Desktop uses this for its generation-guarded startup
    /// profile refresh writer.
    pub fn retain_account_change_observer(
        mut self,
        observer: Arc<dyn lingxi_core::host::auth::AccountChangeObserver>,
    ) -> Self {
        self.account_change_observer_keepalive.push(observer);
        self
    }

    /// Refresh the selected first-party account before accepting an explicit
    /// enable. Off remains available independently of account status.
    pub async fn validate_fast_enable(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<(), LlmError> {
        let identity = self.client.fast_account_identity(model, profile)?;
        let policy = (self.fast_policy)();
        if identity.first_party
            && !policy.no_user_account
            && !crate::structured_output::bool_environment(branding::DISABLE_FAST_MODE_ENV)
        {
            let account = self
                .client
                .load_fast_account(model, profile)
                .await
                .unwrap_or(identity);
            let ua = self.anthropic_user_agent();
            self.fast_availability
                .refresh(&account, &policy, self.transport.as_ref(), &ua, || {
                    self.client.refresh_fast_account(&account)
                })
                .await;
            return self.validate_fast_snapshot(&account, &policy);
        }
        self.validate_fast_snapshot(&identity, &policy)
    }

    fn validate_fast_snapshot(
        &self,
        account: &crate::model::fast_admission::Account,
        policy: &crate::model::fast_admission::Policy,
    ) -> Result<(), LlmError> {
        let (org, oauth) = self.fast_availability.observed(&account.profile, policy);
        let mut input = policy.inputs(account, org, oauth, !self.interactive_session_for_policy());
        input.session_only = input.non_interactive;
        if let Some(reason) = lingxi_core::host::fast_mode::decline(&input) {
            let message = reason.message(&input);
            if !message.is_empty() {
                return Err(LlmError::PermissionDenied {
                    message: format!("Fast mode unavailable: {message}"),
                });
            }
        }
        Ok(())
    }

    /// Current command state uses the admitted boot defaults and live caps,
    /// exactly like the next main preparation, without invoking authentication.
    pub fn effort_command_snapshot(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<Option<lingxi_core::host::effort::EffortCommandSnapshot>, LlmError> {
        let mut request = LlmRequest::new(model);
        request.profile = profile.map(str::to_owned);
        request.execution.resolve_native_effort = true;
        self.refresh_effort_settings(&mut request);
        self.client.effort_command_snapshot(&request)
    }

    fn refresh_effort_settings(&self, request: &mut LlmRequest) {
        request
            .execution
            .inherited_effort_settings
            .clone_from(&self.inherited_effort_settings);
        request
            .execution
            .effort_table_options
            .clone_from(&self.effort_table_options);
        request.execution.session_effort = self
            .session_effort
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(source) = &self.effort_settings_source {
            request.execution.effort_settings = Some(source());
        }
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

    /// Choose an Anthropic compatibility version without changing product UA.
    #[must_use]
    pub fn with_anthropic_compatible_version(mut self, version: impl Into<String>) -> Self {
        self.anthropic_compatible_version = Some(version.into());
        self
    }

    /// Supply a versioned SDK client identity for Anthropic-family requests.
    #[must_use]
    pub fn with_native_thinking_display(
        mut self,
        policy: lingxi_llm_client::providers::anthropic::thinking_display::ThinkingDisplayPolicy,
    ) -> Self {
        self.native_thinking_display = Some(policy);
        self
    }

    /// Explicit host policy for the system attribution envelope. Neutral
    /// providers and ordinary service callers keep their original source.
    pub fn with_native_system_prefix(
        mut self,
        prefix: lingxi_llm_client::providers::anthropic::system_prompt::NativeSystemPrefix,
    ) -> Self {
        self.native_system_prefix = Some(prefix);
        self
    }

    pub fn with_anthropic_client_metadata(
        mut self,
        metadata: lingxi_llm_client::providers::anthropic::request_policy::AnthropicClientMetadata,
    ) -> Self {
        self.anthropic_client_metadata = Some(metadata);
        self
    }

    /// Bind a live owner for admission of new unscoped requests, without
    /// retaining that orchestrator or changing an already-captured origin.
    pub fn set_request_session_id_source(&self, source: RequestSessionIdSource) {
        *self
            .request_session_id_source
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(source);
    }

    async fn capture_request_session_id(&self, request: &mut LlmRequest) {
        if request.execution.request_session_id.is_none() {
            let registered = request
                .execution
                .model_attempt
                .as_ref()
                .and_then(|context| {
                    self.model_attempt_hooks
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .as_ref()
                        .and_then(|hooks| hooks.request_session_id(context))
                })
                .map(|id| id.as_uuid().to_string());
            request.execution.request_session_id = registered.or_else(|| {
                if request.execution.model_attempt.is_some() {
                    None
                } else {
                    lingxi_core::host::session_flags::current_request_session_id()
                        .map(|id| id.as_uuid().to_string())
                }
            });
            if request.execution.request_session_id.is_none()
                && request.execution.model_attempt.is_none()
            {
                let source = self
                    .request_session_id_source
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                if let Some(source) = source {
                    request.execution.request_session_id = source().await;
                }
            }
        }
        if let (Some(session_id), Some(metadata)) = (
            &request.execution.request_session_id,
            &self.request_metadata,
        ) {
            // Recompose only the service-owned canonical identity. A caller's
            // explicit metadata override remains authoritative.
            if request
                .input
                .metadata
                .get("user_id")
                .and_then(serde_json::Value::as_str)
                == Some(metadata.user_id.as_str())
            {
                if let Ok(mut identity) =
                    serde_json::from_str::<serde_json::Value>(&metadata.user_id)
                {
                    if identity.is_object() {
                        identity["session_id"] = serde_json::Value::String(session_id.clone());
                        if let Ok(user_id) = serde_json::to_string(&identity) {
                            request.input.metadata["user_id"] = serde_json::Value::String(user_id);
                        }
                    }
                }
            }
        }
    }

    fn anthropic_user_agent(&self) -> String {
        user_agent(
            &self.ua,
            self.anthropic_compatible_version
                .as_deref()
                .unwrap_or(&self.version),
        )
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
        Self::build_api_metadata_user_id_with_extra(
            device_id,
            account_uuid,
            session_id,
            parent_session_id,
            extra_metadata_object(),
        )
    }

    /// Compose an identity from an explicit host snapshot without reading env.
    #[must_use]
    pub fn build_api_metadata_user_id_with_extra(
        device_id: &str,
        account_uuid: &str,
        session_id: &str,
        parent_session_id: Option<&str>,
        extra: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> String {
        let mut obj = extra.unwrap_or_default();
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

    fn prompt_cache_ttl_inputs(
        &self,
    ) -> lingxi_llm_client::providers::anthropic::system_prompt::PromptCacheTtlInputs {
        use lingxi_llm_client::providers::anthropic::system_prompt::{
            PromptCacheSubscriberState, PromptCacheTtlInputs,
        };
        PromptCacheTtlInputs {
            subscriber: PromptCacheSubscriberState::Unknown,
            is_using_overage: false,
            subscriber_allowlist: Arc::new(native_prompt_cache_1h_allowlist),
        }
    }

    /// Materialize the Native `systemPrompt: string[]` snapshot shape through
    /// the SDK's selected-route gate. An unresolved route deliberately gets no
    /// marker; the source sections themselves are still grouped and retained.
    pub fn prompt_snapshot_source_vector(
        &self,
        model: &str,
        profile: Option<&str>,
        sections: &[lingxi_llm_client::providers::anthropic::system_prompt::SourceSection],
    ) -> Vec<lingxi_llm_client::providers::anthropic::system_prompt::PromptText> {
        use lingxi_llm_client::providers::anthropic::system_prompt as prompt_cache;

        let route_profile = self.client.prompt_cache_profile(model, profile);
        let policy = prompt_cache::CachePolicy::from_process(
            lingxi_core::host::compliance_taints::is_tainted("hipaa"),
            agent_prompt_cache_ttl_override(),
            false,
            PromptCacheQuerySource::Unspecified,
            prompt_cache::PromptCacheTtlSettings::default(),
            prompt_cache::PromptCacheTtlInputs::default(),
        );
        prompt_cache::snapshot_source_vector(sections, route_profile.as_ref(), policy.gate())
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
    /// Build a provider request from the host's typed Native source vector.
    /// Per-query tool-cache policy and Native query source are supplied before
    /// provider schemas lose their registered-tool identity; process and route
    /// gates stay in SDK.
    #[allow(clippy::too_many_arguments)]
    fn build_request(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        msgs: Vec<ConversationMessage>,
        tools: Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
        stream: bool,
        max_tokens: Option<u32>,
        skip_global_cache_for_system_prompt: bool,
        query_source: PromptCacheQuerySource<'_>,
    ) -> Result<LlmRequest, LlmError> {
        // Auxiliary builders share normal policy, but the main turn alone
        // owns its computer continuation and durable receipt submission.
        crate::computer::without_computer_request(|| {
            self.build_main_request(
                model,
                profile,
                system,
                msgs,
                tools,
                stream,
                max_tokens,
                skip_global_cache_for_system_prompt,
                query_source,
            )
        })
    }

    fn build_main_request(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        msgs: Vec<ConversationMessage>,
        tools: Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
        stream: bool,
        max_tokens: Option<u32>,
        skip_global_cache_for_system_prompt: bool,
        query_source: PromptCacheQuerySource<'_>,
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
        let native_system_prefix = self
            .native_system_prefix
            .as_ref()
            .filter(|_| self.client.native_api_system_route(model, profile))
            .map(|prefix| {
                use lingxi_llm_client::providers::anthropic::system_prompt::{
                    NativePromptAttribution, PromptText,
                };
                let first = msgs
                    .iter()
                    .find_map(|message| match message {
                        ConversationMessage::User {
                            content,
                            is_meta: false,
                            ..
                        } => Some(content),
                        _ => None,
                    })
                    .and_then(|content| {
                        content.iter().find(|block| block.visible_text().is_some())
                    });
                let first_user_text = first
                    .map(|block| match block.visible_text_utf16_units() {
                        Some(units) => PromptText::from_utf16(units.to_vec()),
                        None => PromptText::from_string(block.visible_text().unwrap_or_default()),
                    })
                    .unwrap_or_else(|| PromptText::from_string(""));
                (
                    prefix.clone(),
                    NativePromptAttribution {
                        first_user_text,
                        is_subagent: lingxi_core::host::session_flags::current_request_is_subagent(
                        ),
                        workload: self.ua.workload.clone(),
                        previous_request_id: None,
                        prompt_id: None,
                        turn_origin: None,
                        turn_position: None,
                    },
                )
            });
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
                                lingxi_core::types::ContentBlock::Thinking { .. }
                                    | lingxi_core::types::ContentBlock::RedactedThinking { .. }
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
        if !self.client.native_api_system_route(model, profile) {
            for message in &mut msgs {
                if let ConversationMessage::User {
                    api_message_override,
                    ..
                } = message
                {
                    *api_message_override = None;
                }
            }
        }
        let mut per_message_effort = false;
        if self.anthropic_client_metadata.is_some() {
            let mut admitted = LlmRequest::new(model);
            admitted.profile = profile.map(str::to_owned);
            admitted.execution.resolve_native_effort = true;
            self.refresh_effort_settings(&mut admitted);
            if let Ok(effort) = MOD_REQUEST_EFFORT.try_with(Clone::clone) {
                admitted.set_effort(Some(effort))?;
            }
            if let Some(effort) = self
                .client
                .native_per_turn_effort_policy(&admitted)?
                .and_then(|policy| policy.value)
                .and_then(|value| value.as_str().map(str::to_owned))
            {
                per_message_effort = true;
                msgs = lingxi_core::types::project_per_turn_effort(msgs, &effort);
            }
        }
        if !per_message_effort {
            msgs.retain_mut(|message| {
                if let ConversationMessage::System {
                    api_system: Some(payload),
                    ..
                } = message
                {
                    payload.output_config = None;
                    return !payload.content.is_empty();
                }
                if let ConversationMessage::User {
                    api_message_override: Some(payload),
                    ..
                } = message
                {
                    payload.output_config = None;
                }
                true
            });
        }
        let tool_search_enabled = lingxi_core::host::session_flags::tool_search_enabled();
        let available_tool_names: std::collections::HashSet<String> = tools
            .iter()
            .filter_map(|tool| tool.get("name").and_then(serde_json::Value::as_str))
            .map(str::to_string)
            .collect();
        let messages = ensure_tool_result_pairing_with_sources(
            normalize_messages_for_api_with_tool_search_and_sources(
                strip_excess_media_with_sources(
                    ConversationMessagesWithSources::new(msgs),
                    MAX_MEDIA_PER_REQUEST,
                ),
                tool_search_enabled,
                Some(&available_tool_names),
            ),
        );
        let request_message_source_ids = messages.contributing_message_ids();
        let mut messages = to_llm_messages(messages.messages)?;
        let tool_decls = to_tool_declarations(tools)?;

        let mut req = LlmRequest::new(model);
        req.execution.request_message_source_ids = request_message_source_ids;
        req.execution.refusal_fallback_context =
            lingxi_core::host::refusal_driver::current_fallback_target();
        if let Some(p) = profile {
            req = req.with_profile(p);
        }

        use lingxi_llm_client::providers::anthropic::system_prompt as prompt_cache;
        let family = self
            .client
            .protocol_for_model(model, profile)
            .ok()
            .unwrap_or(lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages);
        let cache_policy = prompt_cache::CachePolicy::from_process(
            lingxi_core::host::compliance_taints::is_tainted("hipaa"),
            agent_prompt_cache_ttl_override(),
            skip_global_cache_for_system_prompt,
            query_source,
            self.prompt_cache_ttl_settings_source
                .as_ref()
                .map(|source| sdk_prompt_cache_ttl_settings(source()))
                .unwrap_or_default(),
            self.prompt_cache_ttl_inputs(),
        );
        let enable_caching = prompt_cache::prompt_caching_enabled(model, family);
        let prompt_cache_overage = self.prompt_cache_overage.clone();
        let prompt_cache_epoch = prompt_cache_overage.clone();
        req.execution.prompt_cache = Some(crate::PromptCacheRequestContext {
            system: system.cloned(),
            native_system_prefix,
            policy: cache_policy,
            current_account_epoch: Arc::new(move || prompt_cache_epoch.account_epoch()),
            native_bare_mode: native_prompt_cache_bare_mode(),
            native_unix_socket: native_anthropic_unix_socket_enabled(),
            overage_for_scope: Arc::new(move |scope, account_epoch| {
                prompt_cache_overage.is_using_overage(scope, account_epoch)
            }),
            pending_overage: Arc::new(Mutex::new(None)),
        });
        req.execution.thinking_source_message_ids = thinking_source_message_ids;
        req.execution.thinking_recovery_scope = Some(thinking_recovery_scope);

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
                &mut messages,
                enable_caching,
                &self.cache_editing_inputs.new_edits,
                &self.cache_editing_inputs.pinned,
            );
        }

        let (input, overrides) =
            crate::convert::history_input(model, &messages, &[], &tool_decls, family)?;
        req.input = input;
        // Preserve the semantic prompt for token bounds and pure request
        // inspection. Preparation replaces this projection after capturing
        // the actual credential and current account's TTL/overage state.
        if let (Some(system), Some(context)) = (system, req.execution.prompt_cache.as_ref()) {
            let selected_profile = self.client.prompt_cache_profile(model, profile);
            let projection = prompt_cache::project_system_prompt(
                system,
                selected_profile.as_ref(),
                model,
                family,
                context.policy.clone(),
            );
            req.input.system = projection.iter().map(|item| item.block.clone()).collect();
            req.input.prompt_cache.breakpoints.extend(
                projection
                    .into_iter()
                    .filter_map(|item| item.cache_breakpoint),
            );
        }
        req.execution.input_protocol = Some(family);
        req.execution.message_json_string_overrides = overrides;
        prompt_cache::apply_last_message_breakpoint(&mut req.input, enable_caching);
        crate::computer::apply_request_projection(&mut req)?;
        // No tool-array breakpoint (matches TS baseline).
        // Forced tool choice (e.g. `--json-schema` → `StructuredOutput`). Unset
        // for every normal turn, so the request carries no `tool_choice` and the
        // model chooses freely — byte-identical to the pre-feature request.
        if let Some(choice) = &self.forced_tool_choice {
            req.set_tool_choice(Some(choice.clone()));
        }
        if req.input.model.contains("deepseek") || req.profile.as_deref() == Some("deepseek") {
            tracing::debug!(
                event = "build_request",
                model = %req.input.model,
                profile = req.profile.as_deref().unwrap_or("<none>"),
                messages = messages.len(),
                tools = req.input.tools.len(),
                forced_tool_choice = self.forced_tool_choice.is_some(),
                active_tool_choice = ?req.input.tool_choice,
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
        req.input.max_tokens = Some(
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
        if let Some(mt) = req.input.max_tokens {
            let bounded = bound_output_to_context(mt, context_window, input_est);
            if bounded == 0 {
                return Err(LlmError::ContextOverflow {
                    token_gap: input_est.saturating_sub(context_window),
                });
            }
            req.input.max_tokens = Some(bounded);
        }

        // thinking (DIV-1) + temperature (DIV-4), mirroring claude.ts:1596-1630
        // and claude.ts:1693. Computed AFTER max_tokens is known (the fixed-
        // budget cap clamps to max_tokens-1).
        {
            use crate::model::thinking::{model_sends_temperature, session_thinking_active};

            let thinking = self.thinking();
            if self.anthropic_client_metadata.is_some() {
                req.execution.resolve_native_effort = true;
                req.execution.anthropic_context_management = Some(
                    lingxi_llm_client::providers::anthropic::request_policy::AnthropicContextManagement {
                        has_thinking: !matches!(thinking, crate::model::thinking::ThinkingConfig::Disabled)
                            && !crate::model::thinking::is_thinking_env_disabled("LINGXI_DISABLE_THINKING"),
                        tool_clearing:None,
                    }
                );
                req.execution.native_thinking_display = self.native_thinking_display.clone();
            }
            let has_thinking = session_thinking_active(thinking);

            // The claude/non-claude branch, the env kill switches and the
            // budget clamp live in `model::thinking::reasoning_for_request` —
            // the SAME session-config resolution the compaction side-query
            // path inherits (cc 2.1.198). Behavior is byte-identical to the
            // previous inline block.
            req.set_reasoning(crate::model::thinking::reasoning_for_request(
                thinking,
                model,
                req.input.max_tokens,
            ));

            // temperature:1 ONLY when thinking is disabled AND the model is in the
            // `rhn` temperature-gate set (binary @205866168:
            // `!xs && rhn(u) ? temperatureOverride ?? 1 : void 0`). The default
            // opus-4-8 (and 4-7/fable-5/mythos-5/unknowns) are NOT in `rhn` → the
            // field is omitted. The Anthropic codec emits temperature on Some only.
            req.input.temperature = if !has_thinking
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
        req.input.metadata = self
            .request_metadata
            .as_ref()
            .map(|m| serde_json::json!({"user_id":m.user_id}))
            .unwrap_or(serde_json::Value::Null);

        if let Ok(effort) = MOD_REQUEST_EFFORT.try_with(Clone::clone) {
            req.set_effort(Some(effort))?;
        }
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

    /// Apply provider-aware beta, User-Agent, dispatch, and refusal headers to a
    /// prepared request. Native client request-id generation belongs to the SDK
    /// selected-route policy and is already sealed before this Host step.
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
        // YMe emits a beta for a resolved string or a supported main default.
        let effort = prepared
            .provider_request
            .body_json
            .get("output_config")
            .and_then(|oc| oc.get("effort"));
        let explicit_effort = prepared.anthropic_request_kind
            == lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main
            && prepared
                .extra_body
                .as_ref()
                .and_then(|extra| extra.get("output_config"))
                .and_then(serde_json::Value::as_object)
                .is_some_and(|config| config.contains_key("effort"));
        let automatic_schema = prepared
            .provider_request
            .body_json
            .get("output_config")
            .and_then(|config| config.get("format"))
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
        let interactive = self.interactive_session.unwrap_or_else(|| {
            !lingxi_core::host::session_flags::effective_non_interactive_session()
        });
        let beta_provider = match prepared.route.protocol {
            crate::ProtocolFamily::FoundryClaude => Provider::Foundry,
            crate::ProtocolFamily::VertexClaude => Provider::Vertex,
            crate::ProtocolFamily::BedrockClaude => Provider::Bedrock,
            _ => Provider::Anthropic,
        };
        let supported = prepared.effort_policy.as_ref().map_or_else(
            || crate::model::betas::effort_capable(beta_provider, model),
            |policy| policy.supported,
        );
        let selected = match prepared.effort_policy.as_ref() {
            Some(policy) => policy.value.as_ref(),
            None => effort,
        };
        let automatic_effort = supported
            && !explicit_effort
            && selected.map_or(prepared.anthropic_request_kind == lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main, serde_json::Value::is_string);
        BetaContext::for_model(model)
            .with_interactive(interactive)
            .with_show_thinking_summaries(
                lingxi_core::host::session_flags::show_thinking_summaries(),
            )
            .with_fast_mode(fast_mode)
            .with_effort(automatic_effort)
            .with_structured_output(automatic_schema)
            .with_request_kind(prepared.anthropic_request_kind)
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
        self.interactive_session.unwrap_or_else(|| {
            !lingxi_core::host::session_flags::effective_non_interactive_session()
        })
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

    async fn capture_fast_account(
        &self,
        prepared: &mut crate::PreparedLlmCall,
    ) -> Result<(), LlmError> {
        if !Self::direct_anthropic_api_route(prepared)
            || !prepared.fast_mode_allowed
            || lingxi_core::host::fast_mode::ModelRejections::for_process().blocked(
                &self.fast_rejection_identity(&prepared.route.resolved_route.request_model),
            )
        {
            return Ok(());
        }
        let fast_requested = prepared
            .provider_request
            .body_json
            .get("speed")
            .and_then(serde_json::Value::as_str)
            == Some("fast")
            || prepared
                .extra_body
                .as_ref()
                .and_then(|body| body.get("speed"))
                .and_then(serde_json::Value::as_str)
                == Some("fast");
        if fast_requested {
            let generation = self.fast_availability.generation();
            let credential = prepared.authenticator.captured_credential().await?;
            prepared.fast_account_binding = Some(crate::model::fast_admission::Binding::new(
                credential.as_ref(),
                generation,
            ));
        }
        Ok(())
    }

    fn enforce_fast_route(&self, prepared: &mut crate::PreparedLlmCall) {
        let route = &prepared.route.resolved_route;
        let admitted = self
            .client
            .fast_account_identity(&route.request_model, Some(&route.profile_name))
            .is_ok_and(|account| {
                let policy = (self.fast_policy)();
                let (org, oauth) = prepared.fast_account_binding.map_or_else(
                    || self.fast_availability.observed(&account.profile, &policy),
                    |binding| {
                        self.fast_availability
                            .observed_for(&account.profile, &policy, binding)
                    },
                );
                let mut inputs =
                    policy.inputs(&account, org, oauth, !self.interactive_session_for_policy());
                // Model/global admission was captured before SDK/credential work.
                inputs.disabled = false;
                inputs.model_fast = prepared.fast_mode_allowed;
                lingxi_core::host::fast_mode::decline(&inputs).is_none()
            });
        let allowed = Self::direct_anthropic_api_route(prepared)
            && prepared.fast_mode_allowed
            && admitted
            && !lingxi_core::host::fast_mode::ModelRejections::for_process().blocked(
                &self.fast_rejection_identity(&prepared.route.resolved_route.request_model),
            );
        if allowed {
            return;
        }
        lingxi_llm_client::providers::anthropic::request_policy::remove_fast(
            &mut prepared.provider_request.body_json,
            &mut prepared.provider_request.headers,
            &mut prepared.provider_request.json_string_overrides,
            FAST_MODE,
        );
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
        use lingxi_llm_client::providers::anthropic::request_policy::{
            apply_user_agent, UserAgentPolicy,
        };
        let anthropic = Self::is_anthropic_family_protocol(&prepared.route.protocol);
        let value = if anthropic {
            self.anthropic_user_agent()
        } else {
            format!("LingXi-Code/{}", self.version)
        };
        let policy = if anthropic {
            UserAgentPolicy::Replace(&value)
        } else {
            UserAgentPolicy::IfAbsent(&value)
        };
        apply_user_agent(&mut prepared.provider_request.headers, policy);
        if anthropic {
            if let Some(metadata) = &self.anthropic_client_metadata {
                if let Some(draft) = prepared.wire_draft.as_mut() {
                    draft.set_http1_header_layout(Some(
                        lingxi_llm_client::Http1HeaderLayout::NativeFetch,
                    ));
                }
                let mut metadata = metadata.clone();
                // An admitted request owns its identity. Missing authority never
                // falls back to a possibly stale boot session at dispatch.
                metadata.session_id = prepared.request_session_id.clone();
                lingxi_llm_client::providers::anthropic::request_policy::apply_client_metadata(
                    &mut prepared.provider_request.headers,
                    &metadata,
                );
            }
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
    #[cfg(test)]
    fn parse_extra_body(betas: &[String]) -> serde_json::Map<String, serde_json::Value> {
        lingxi_llm_client::providers::anthropic::request_policy::beta_body(
            extra_body_object()
                .expect("valid extra body")
                .unwrap_or_default(),
            betas,
        )
    }

    fn prepared_per_turn_effort(prepared: &crate::PreparedLlmCall) -> Option<String> {
        (prepared.anthropic_request_kind
            == lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main
            && prepared.route.resolved_route.provider_id == crate::ProviderId::AnthropicFirstParty
            && lingxi_llm_client::providers::anthropic::supports_per_message_effort(
                &prepared.route.resolved_route.request_model,
            ))
        .then(|| {
            prepared
                .effort_policy
                .as_ref()
                .and_then(|policy| policy.value.as_ref())
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .flatten()
    }

    fn is_main_thinking_display_call(prepared: &crate::PreparedLlmCall) -> bool {
        prepared.anthropic_request_kind
            == lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main
            && prepared.route.protocol == crate::ProtocolFamily::AnthropicMessages
            && prepared.route.resolved_route.provider_id == crate::ProviderId::AnthropicFirstParty
    }

    fn settle_thinking_display_probe(
        prepared: &crate::PreparedLlmCall,
        scope: &ModelCallRetryScope,
    ) {
        if Self::is_main_thinking_display_call(prepared) {
            scope.display_probe_succeeded(&prepared.beta_rejection_state);
        }
    }

    /// Try the SDK's unclaimed display repair before ordinary API retries.
    fn probe_thinking_display_error(
        &self,
        prepared: &crate::PreparedLlmCall,
        scope: &ModelCallRetryScope,
        status: u16,
        body: &serde_json::Value,
    ) -> bool {
        use lingxi_llm_client::providers::anthropic::thinking_display;
        if !Self::is_main_thinking_display_call(prepared) {
            return false;
        }
        let named = lingxi_llm_client::providers::anthropic::beta_repair::request_rejections(
            status,
            body,
            &prepared.computed_beta_headers,
        );
        if !named.is_empty() {
            lingxi_llm_client::providers::anthropic::beta_repair::reject_named(
                &named, &prepared.beta_rejection_state,
                Some(&prepared.route.resolved_route.request_model),
                &lingxi_llm_client::providers::anthropic::beta_repair::ModelBetaRejections::for_process(),
            );
            return true;
        }
        let identity = self.fast_rejection_identity(&prepared.route.resolved_route.request_model);
        let admission = thinking_display::probe_admission(
            status,
            body,
            prepared
                .computed_beta_headers
                .iter()
                .any(|beta| beta == thinking_display::UPDATES_BETA),
            Self::direct_anthropic_api_route(prepared),
            lingxi_llm_client::providers::anthropic::error_recognition::RecognitionContext {
                request_model: &prepared.route.resolved_route.request_model,
                refusal_fallback_target: prepared.refusal_fallback_context.as_ref().is_some_and(
                    |context| {
                        context.is_target(&prepared.route.resolved_route.request_model, |model| {
                            self.fast_rejection_identity(model)
                        })
                    },
                ),
                previous_fast_rejection:
                    lingxi_core::host::fast_mode::ModelRejections::for_process()
                        .known_rejected(&identity),
                prefix_heal_declined: false,
            },
            |model| self.fast_rejection_identity(model),
        );
        scope.display_probe_error(
            status,
            admission,
            &thinking_display::DisplayProbeBudget::for_process(),
        )
    }

    fn fast_rejection_identity(&self, model: &str) -> String {
        let lower = model.to_lowercase();
        let model = self
            .alias_to_display
            .get(model)
            .or_else(|| self.alias_to_display.get(&lower))
            .map_or(lower.as_str(), String::as_str);
        let canonical = crate::model::betas::beta_canonical(model);
        canonical
            .strip_suffix("[1m]")
            .unwrap_or(&canonical)
            .to_string()
    }

    fn repair_fast_model_rejection(
        &self,
        req: &mut LlmRequest,
        prepared: &crate::PreparedLlmCall,
        status: u16,
        body: &serde_json::Value,
    ) -> bool {
        if req.input.service_tier != Some(lingxi_llm_client::protocol::ServiceTier::Fast) {
            return false;
        }
        if Self::is_main_thinking_display_call(prepared)
            && prepared
                .provider_request
                .body_json
                .get("speed")
                .and_then(serde_json::Value::as_str)
                != Some("fast")
        {
            return false;
        }
        if lingxi_llm_client::providers::anthropic::error_recognition::fast_not_enabled(
            status, body,
        ) {
            req.input.service_tier = None;
            return true;
        }
        if !Self::is_main_thinking_display_call(prepared) {
            return false;
        }
        let model = &prepared.route.resolved_route.request_model;
        let identity = self.fast_rejection_identity(model);
        let models = lingxi_core::host::fast_mode::ModelRejections::for_process();
        let target = prepared
            .refusal_fallback_context
            .as_ref()
            .is_some_and(|context| {
                context.is_target(model, |name| self.fast_rejection_identity(name))
            });
        if !(target || models.known_rejected(&identity))
            || !lingxi_llm_client::providers::anthropic::error_recognition::speed_rejected_for(
                status,
                body,
                model,
                |name| self.fast_rejection_identity(name),
            )
        {
            return false;
        }
        models.reject(&identity);
        req.input.service_tier = None;
        true
    }

    fn apply_refusal_headers(prepared: &mut crate::PreparedLlmCall) {
        let Some(context) = &prepared.refusal_fallback_context else {
            return;
        };
        let first_party = Self::dispatch_first_party(prepared);
        lingxi_llm_client::providers::anthropic::refusal_fallback::apply_request_headers(
            &mut prepared.provider_request.headers,
            first_party,
            context.refusal_header_armed,
            context.refusal_occurred,
            context.refusal_lane_enabled,
            context.refusal_origin_request_id.as_deref(),
        );
    }

    fn apply_server_fallback(prepared: &mut crate::PreparedLlmCall) -> Result<(), LlmError> {
        if prepared.anthropic_request_kind
            != lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main
            || !matches!(
                prepared.route.protocol,
                crate::ProtocolFamily::AnthropicMessages
                    | crate::ProtocolFamily::FoundryClaude
                    | crate::ProtocolFamily::BedrockClaude
                    | crate::ProtocolFamily::VertexClaude
            )
        {
            return Ok(());
        }
        use lingxi_llm_client::providers::anthropic::fallback_request::{
            LaneMode, RequestPolicy, ServerLane,
        };
        let mut policy = prepared
            .server_fallback
            .clone()
            .or_else(|| {
                let facts = prepared
                    .refusal_fallback_context
                    .as_ref()?
                    .server_fallback
                    .as_ref()?;
                let mut host_policy = facts.policy.clone();
                let sticky = prepared.server_fallback_betas.snapshot();
                host_policy.explicit_beta_rejected |= sticky.explicit_rejected;
                host_policy.default_beta_rejected |= sticky.default_rejected;
                let decision = host_policy.decide(&facts.model, &facts.query);
                Some(RequestPolicy {
                    lane: decision.server_lane.map(|lane| ServerLane {
                        for_model: lane.for_model,
                        model: lane.model,
                        mode: match lane.mode {
                            lingxi_core::host::refusal_server::Mode::Default => LaneMode::Default,
                            lingxi_core::host::refusal_server::Mode::Explicit => LaneMode::Explicit,
                        },
                    }),
                    explicit_target_eligible: host_policy.explicit_target_eligible,
                    silent_arm: host_policy.silent_arm,
                    threaded_request: host_policy.threaded_request,
                    beta_transport_enabled: host_policy.beta_transport_enabled,
                    simulated_proxy_usage: false,
                })
            })
            .unwrap_or(RequestPolicy {
                beta_transport_enabled: true,
                ..Default::default()
            });
        policy.simulated_proxy_usage |=
            crate::structured_output::bool_environment("CLAUDE_CODE_SIMULATE_PROXY_USAGE");
        let first_party = Self::dispatch_first_party(prepared);
        prepared.server_fallback_lane = first_party.then(|| policy.lane.clone()).flatten();
        policy
            .apply(
                &prepared.route.resolved_route.request_model,
                first_party,
                &prepared.server_fallback_betas,
                &mut prepared.provider_request.body_json,
                &mut prepared.provider_request.headers,
                &mut prepared.provider_request.json_string_overrides,
            )
            .map_err(crate::upstream::error)
    }

    /// Consume the SDK's typed server-fallback beta repair fact for this query.
    /// The parameter-presence input is captured before the extra-body spread,
    /// matching native `a1`; the SDK facts helper owns the native cause and
    /// one-shot guards, while the prepared call supplies the conversation-local
    /// state handle.
    fn repair_server_fallback_beta_rejection(
        prepared: &crate::PreparedLlmCall,
        status: u16,
        body: &serde_json::Value,
        server_fallback_parameter_added: bool,
        repair_already_attempted: bool,
    ) -> bool {
        // Native r3e catches this SDK-classified error across all Anthropic
        // Messages transports, including the cloud Claude wrappers.
        if !Self::is_anthropic_family_protocol(&prepared.route.protocol) {
            return false;
        }
        use lingxi_llm_client::providers::anthropic::fallback_request;
        let Some(cause) = fallback_request::classify_server_fallback_rejection(status, body) else {
            return false;
        };
        let Some(facts) = fallback_request::server_fallback_repair_facts(
            cause,
            server_fallback_parameter_added,
            repair_already_attempted,
        ) else {
            return false;
        };
        prepared.server_fallback_betas.reject(facts.rejected_mode);
        true
    }

    fn apply_automatic_thinking_display(&self, prepared: &mut crate::PreparedLlmCall) {
        use lingxi_llm_client::providers::anthropic::thinking_display::{self, UpdatesAdmission};
        if !Self::is_main_thinking_display_call(prepared) {
            return;
        }
        lingxi_llm_client::providers::anthropic::beta_repair::apply_rejections(
            &mut prepared.provider_request.headers,
            &mut prepared.provider_request.body_json,
            &prepared.beta_rejection_state,
            Some(&prepared.route.resolved_route.request_model),
            &lingxi_llm_client::providers::anthropic::beta_repair::ModelBetaRejections::for_process(
            ),
        );
        if let Some(policy) = &prepared.native_thinking_display {
            if thinking_display::apply_display_policy(
                &mut prepared.provider_request.body_json,
                policy,
            ) {
                prepared
                    .provider_request
                    .json_string_overrides
                    .retain(|path, _| {
                        path != "/thinking/display" && !path.starts_with("/thinking/display/")
                    });
            }
        }
        if crate::structured_output::experimental_betas_disabled() {
            return;
        }
        let model = &prepared.route.resolved_route.request_model;
        let canonical = crate::model::thinking::canonical(model);
        let updates = std::env::var(branding::THINKING_DISPLAY_UPDATES_ENV)
            .ok()
            .is_none_or(|value| crate::structured_output::bool_value(&value));
        let display = prepared
            .provider_request
            .body_json
            .pointer("/thinking/display")
            .and_then(serde_json::Value::as_str);
        let mode = thinking_display::connector_mode(
            display,
            prepared
                .native_thinking_display
                .as_ref()
                .is_some_and(|policy| policy.is_explicit()),
            lingxi_core::host::session_flags::show_thinking_summaries(),
            updates,
        );
        thinking_display::apply_request_updates(
            &mut prepared.provider_request.body_json,
            &mut prepared.provider_request.headers,
            &mut prepared.provider_request.json_string_overrides,
            UpdatesAdmission {
                mode,
                supports_interleaved: !canonical.starts_with("claude-3-")
                    && canonical != "claude-haiku-4-5",
                extra_has_thinking: prepared
                    .extra_body
                    .as_ref()
                    .is_some_and(|extra| extra.contains_key("thinking")),
                simulated_proxy: crate::structured_output::bool_environment(
                    "CLAUDE_CODE_SIMULATE_PROXY_USAGE",
                ),
            },
            &prepared.beta_rejection_state,
            &prepared.thinking_display_probe,
        );
    }

    /// Apply the main/side extra-body policy after automatic fields and beta
    /// selection, preserving JS spread collisions and property ordering.
    fn merge_extra_body(&self, prepared: &mut crate::PreparedLlmCall) -> Result<(), LlmError> {
        if !Self::is_anthropic_family_protocol(&prepared.route.protocol) {
            return Ok(());
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
        lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestPolicy {
            request_kind: prepared.anthropic_request_kind,
            extra_body: prepared.extra_body.clone().unwrap_or_default(),
            body_betas,
            effort: prepared.effort_policy.clone(),
            context_management: (prepared.anthropic_request_kind == lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main
                && matches!(prepared.route.resolved_route.provider_id, crate::ProviderId::AnthropicFirstParty | crate::ProviderId::FoundryClaude | crate::ProviderId::BedrockClaude | crate::ProviderId::VertexClaude))
                .then(|| prepared.anthropic_context_management.clone()).flatten(),
            message_header_parameters: matches!(
                prepared.route.protocol,
                crate::ProtocolFamily::AnthropicMessages
                    | crate::ProtocolFamily::FoundryClaude
                    | crate::ProtocolFamily::BedrockClaude
                    | crate::ProtocolFamily::VertexClaude
            ),
            ..Default::default()
        }
        .apply(
            &mut prepared.provider_request.body_json,
            &mut prepared.provider_request.headers,
            &mut prepared.provider_request.json_string_overrides,
            &mut prepared.provider_request.url,
        )
        .map_err(crate::upstream::error)?;
        if prepared.stream_fallback
            && prepared.route.protocol == crate::ProtocolFamily::AnthropicMessages
        {
            lingxi_llm_client::providers::anthropic::request_policy::nonstream_fallback_parameters(
                &mut prepared.provider_request.body_json,
                &mut prepared.provider_request.json_string_overrides,
            )
            .map_err(crate::upstream::error)?;
        }
        Ok(())
    }

    fn apply_structured_output_beta(prepared: &mut crate::PreparedLlmCall) {
        if matches!(
            prepared.route.protocol,
            crate::ProtocolFamily::AnthropicMessages
                | crate::ProtocolFamily::FoundryClaude
                | crate::ProtocolFamily::BedrockClaude
                | crate::ProtocolFamily::VertexClaude
        ) && !Self::extra_replaces_message_betas(prepared)
            && prepared
                .provider_request
                .body_json
                .get("output_config")
                .and_then(|config| config.get("format"))
                .is_some()
        {
            lingxi_llm_client::providers::anthropic::request_policy::merge_beta_header(
                &mut prepared.provider_request.headers,
                &[crate::model::betas::STRUCTURED_OUTPUTS.to_string()],
            );
        }
    }

    fn extra_replaces_message_betas(prepared: &crate::PreparedLlmCall) -> bool {
        matches!(
            prepared.route.protocol,
            crate::ProtocolFamily::AnthropicMessages
                | crate::ProtocolFamily::FoundryClaude
                | crate::ProtocolFamily::BedrockClaude
                | crate::ProtocolFamily::VertexClaude
        ) && prepared
            .extra_body
            .as_ref()
            .is_some_and(|extra| extra.contains_key("betas"))
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
        if env_truthy("_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL") {
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

    /// Native XMe/hYe: recovery uses v2p for the remainder of this query.
    fn apply_dispatch_header(
        prepared: &mut crate::PreparedLlmCall,
        state: DispatchHeaderState,
    ) -> DispatchAttempt {
        let first_party = Self::dispatch_first_party(prepared);
        let value = state.header(
            first_party,
            ::telemetry::flag_bool("tengu_dreamy_frost", false),
            Self::dispatch_v2s_opt_in(),
        );
        if let Some(value) = value {
            lingxi_llm_client::providers::anthropic::request_policy::set_header(
                &mut prepared.provider_request.headers,
                DISPATCH_ID_HEADER,
                value,
            );
            tracing::debug!("[dispatch] sent {DISPATCH_ID_HEADER}={value}");
        }
        DispatchAttempt {
            value: value.map(str::to_owned),
            first_party,
        }
    }

    fn note_dispatch_header_failure(
        state: &mut DispatchHeaderState,
        attempt: &DispatchAttempt,
        err: &LlmError,
        response: Option<(u16, bool)>,
    ) -> Option<DispatchFallback> {
        state.on_failure(
            attempt,
            DispatchFailure {
                status: response
                    .map(|(status, _)| status)
                    .or_else(|| Self::status_of(err)),
                connection: Self::is_dispatch_conn_err(err),
                declined: response.is_some_and(|(_, declined)| declined),
            },
        )
    }

    async fn report_dispatch_fallback(
        &self,
        req: &LlmRequest,
        fallback: DispatchFallback,
        request_id: Option<&str>,
    ) {
        let what = fallback
            .status
            .map_or_else(|| "connection error".to_string(), |s| format!("HTTP {s}"));
        let previous = fallback.previous.as_deref().map_or_else(
            || format!("no {DISPATCH_ID_HEADER}"),
            |value| format!("{DISPATCH_ID_HEADER}={value}"),
        );
        tracing::warn!(
            "[dispatch] {what} with {previous}; retrying once with {DISPATCH_ID_HEADER}=v2p"
        );
        telemetry::emit_dispatch_header_fallback(
            &self.analytics,
            telemetry::DispatchFallbackEvent {
                model: &req.input.model,
                dispatch: fallback.previous.as_deref(),
                reason: fallback.reason,
                status: fallback.status,
                query_source: req.execution.query_source.as_deref(),
                request_id,
            },
        )
        .await;
        let delay = fallback.delay(rand::random::<f64>());
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
    }

    /// Native yYe checks APIConnectionError, including its timeout subclass.
    /// Body-phase x2 cause classification remains separate below.
    fn is_dispatch_conn_err(err: &LlmError) -> bool {
        matches!(
            err,
            LlmError::Transport { .. }
                | LlmError::TransportTimeout { .. }
                | LlmError::TlsCert { .. }
        )
    }

    fn inject_headers(
        &self,
        prepared: &mut crate::PreparedLlmCall,
        dispatch: DispatchHeaderState,
    ) -> Result<(DispatchAttempt, bool), LlmError> {
        // Provider-specific tool-search beta: first-party/Foundry use
        // advanced-tool-use, Vertex uses tool-search-tool, and Bedrock carries
        // tool-search-tool in the request body's anthropic_beta array.
        let beta_provider = match prepared.route.protocol {
            crate::ProtocolFamily::AnthropicMessages
                if prepared.route.resolved_route.provider_id
                    == crate::ProviderId::AnthropicFirstParty =>
            {
                Some(Provider::Anthropic)
            }
            crate::ProtocolFamily::FoundryClaude => Some(Provider::Foundry),
            crate::ProtocolFamily::VertexClaude => Some(Provider::Vertex),
            crate::ProtocolFamily::BedrockClaude => Some(Provider::Bedrock),
            _ => None,
        };
        if let Some(provider) =
            beta_provider.filter(|_| !Self::extra_replaces_message_betas(prepared))
        {
            let ctx = self.beta_context(prepared).with_context_hint(
                dispatch.context_hint_beta
                    || prepared
                        .provider_request
                        .body_json
                        .get("context_hint")
                        .is_some(),
            );
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
        Self::apply_structured_output_beta(prepared);
        // User-Agent (Task 3) — provider-aware (see apply_user_agent).
        self.apply_user_agent(prepared);
        // (cc 2.1.219) opt-in dispatch-routing header (see apply_dispatch_header).
        let attempt = Self::apply_dispatch_header(prepared, dispatch);
        Self::apply_refusal_headers(prepared);
        Self::apply_server_fallback(prepared)?;
        let server_fallback_parameter_added = prepared
            .provider_request
            .body_json
            .get("fallbacks")
            .is_some();
        // CLAUDE_CODE_EXTRA_BODY merge — after the beta header is computed from the
        // pre-merge body (claude-code `B0t` spread; 2.1.207).
        self.apply_automatic_thinking_display(prepared);
        prepared.computed_beta_headers =
            lingxi_llm_client::providers::anthropic::beta_repair::request_betas(
                &prepared.provider_request.headers,
            );
        self.merge_extra_body(prepared)?;
        // Final resolved-route guard. `CLAUDE_CODE_EXTRA_BODY` is merged above,
        // so this must run last to prevent it from reintroducing first-party
        // speed/beta fields on custom or unsupported routes.
        self.enforce_fast_route(prepared);
        Ok((attempt, server_fallback_parameter_added))
    }

    /// Same as [`inject_headers`] but for the streaming endpoint.
    fn inject_stream_headers(
        &self,
        prepared: &mut crate::PreparedLlmCall,
        dispatch: DispatchHeaderState,
    ) -> Result<(DispatchAttempt, bool), LlmError> {
        let beta_provider = match prepared.route.protocol {
            crate::ProtocolFamily::AnthropicMessages
                if prepared.route.resolved_route.provider_id
                    == crate::ProviderId::AnthropicFirstParty =>
            {
                Some(Provider::Anthropic)
            }
            crate::ProtocolFamily::FoundryClaude => Some(Provider::Foundry),
            crate::ProtocolFamily::VertexClaude => Some(Provider::Vertex),
            crate::ProtocolFamily::BedrockClaude => Some(Provider::Bedrock),
            _ => None,
        };
        if let Some(provider) =
            beta_provider.filter(|_| !Self::extra_replaces_message_betas(prepared))
        {
            let ctx = self.beta_context(prepared).with_context_hint(
                dispatch.context_hint_beta
                    || prepared
                        .provider_request
                        .body_json
                        .get("context_hint")
                        .is_some(),
            );
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
        Self::apply_structured_output_beta(prepared);
        self.apply_user_agent(prepared);
        // (cc 2.1.219) opt-in dispatch-routing header (see apply_dispatch_header).
        let attempt = Self::apply_dispatch_header(prepared, dispatch);
        Self::apply_refusal_headers(prepared);
        Self::apply_server_fallback(prepared)?;
        let server_fallback_parameter_added = prepared
            .provider_request
            .body_json
            .get("fallbacks")
            .is_some();
        // CLAUDE_CODE_EXTRA_BODY merge — after the beta header is computed from the
        // pre-merge body (claude-code `B0t` spread; 2.1.207).
        self.apply_automatic_thinking_display(prepared);
        prepared.computed_beta_headers =
            lingxi_llm_client::providers::anthropic::beta_repair::request_betas(
                &prepared.provider_request.headers,
            );
        self.merge_extra_body(prepared)?;
        self.enforce_fast_route(prepared);
        Ok((attempt, server_fallback_parameter_added))
    }

    // ── 429 retry-after resolution (reset ladder) ─────────────────────────────

    /// Resolve the 429 retry delay using the server-sent reset ladder:
    /// `retry-after` → `anthropic-ratelimit-unified-reset` → `anthropic-ratelimit-requests-reset` → no server hint.
    fn resolve_retry_after(
        headers: &std::collections::BTreeMap<String, String>,
        protocol: lingxi_llm_client::protocol::ProtocolFamily,
        provider_id: &str,
    ) -> Option<std::time::Duration> {
        let wire_headers: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let now = std::time::SystemTime::now();
        let metadata =
            lingxi_llm_client::providers::response_headers::ProviderResponseHeaders::decode(
                protocol,
                provider_id,
                &wire_headers,
                now,
            );
        // Host retains source priority; the SDK owns provider header names and
        // wire parsing, including Anthropic vs. OpenAI reset formats.
        metadata
            .retry_after
            .or(metadata.anthropic_unified_reset)
            .or(metadata.anthropic_request_reset)
            .or(metadata.openai_reset)
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
    /// | *(llm-runtime only)*    | `FileUploadOutcomeUnknown`         | `"file_upload_outcome_unknown"` |
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
            LlmError::ProviderInternal | LlmError::ProviderTimeout { .. } => "server",
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
            LlmError::RequestDispatchRejected { .. } => "request_dispatch_rejected",
            LlmError::QuotaExceeded => "quota_exceeded",
            LlmError::ModelUnavailable => "model_unavailable",
            LlmError::CostUnavailable { .. } => "cost_unavailable",
            LlmError::FileUploadOutcomeUnknown { .. } => "file_upload_outcome_unknown",
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
    /// when present, else the actual outgoing client-ID fallback (see
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
    /// [`RequestIdOrigin::Client`] when it is the actual outgoing client-ID fallback.
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
    ) -> std::collections::HashMap<lingxi_core::types::MessageId, usize> {
        self.thinking_recovery_scope().messages()
    }

    pub fn set_thinking_stripped_messages(
        &self,
        messages: std::collections::HashMap<lingxi_core::types::MessageId, usize>,
    ) {
        self.thinking_recovery_scope().merge(messages);
    }

    /// Strip thinking blocks after a thinking-signature 400 on any provider.
    /// Returns `true` when the caller should retry immediately.
    async fn handle_thinking_signature_strip(&self, req: &mut crate::LlmRequest) -> bool {
        let (signed, unsigned) =
            crate::model::thinking_signature::count_input_thinking(&req.input.messages);
        if !crate::model::thinking_signature::strip_input_thinking(req) {
            return false;
        }
        tracing::warn!(
            "[thinking] server rejected a thinking block; stripping all thinking blocks and retrying."
        );
        telemetry::emit_thinking_signature_strip_retry(
            &self.analytics,
            req.execution.query_source.as_deref(),
            &req.input.model,
            signed,
            unsigned,
        )
        .await;
        let scope = req
            .execution
            .thinking_recovery_scope
            .clone()
            .unwrap_or_else(|| self.thinking_recovery_scope());
        scope.rejected(
            req.execution
                .thinking_source_message_ids
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

    fn record_rate_limit_from_headers_for_route(
        &self,
        headers: &std::collections::BTreeMap<String, String>,
        client_request_id: Option<&str>,
        protocol: lingxi_llm_client::protocol::ProtocolFamily,
        provider_id: &str,
    ) {
        self.record_rate_limit_from_headers_at_for_route(
            headers,
            client_request_id,
            protocol,
            provider_id,
            Self::now_ms(),
        );
    }

    fn record_rate_limit_from_headers_at_for_route(
        &self,
        headers: &std::collections::BTreeMap<String, String>,
        client_request_id: Option<&str>,
        protocol: lingxi_llm_client::protocol::ProtocolFamily,
        provider_id: &str,
        ts_ms: u128,
    ) {
        let hvec: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        // Capture the request id on every recorded response — the SDK's
        // `response._request_id`, which claude-code persists as the assistant
        // line's top-level `requestId`. Provider-aware: tries each provider's
        // canonical request-id header for the selected route. If absent, use
        // the actual SDK/caller `x-client-request-id`; without one, keep the
        // Host correlation field absent.
        *self.last_request_id.lock().unwrap() =
            match crate::execution::extract_response_request_id(protocol, provider_id, headers) {
                Some(server_id) => Some((server_id, RequestIdOrigin::Server)),
                None if client_request_id.is_some_and(|id| !id.is_empty()) => {
                    let client_request_id = client_request_id.unwrap_or_default();
                    tracing::debug!(
                        client_request_id,
                        "no provider request-id header on response; \
                         falling back to outgoing client request-id for correlation only"
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
        let decoded =
            lingxi_llm_client::providers::response_headers::ProviderResponseHeaders::decode(
                protocol,
                provider_id,
                &hvec,
                std::time::SystemTime::now(),
            )
            .anthropic_rate_limits
            .unwrap_or_default();
        let raw = RawUtilization::from_decoded(&decoded);
        if !stale {
            *self.last_raw_utilization.lock().unwrap() = Some(raw);
        }
        let info = RateLimitInfo::from_decoded_at(&decoded, std::time::SystemTime::now());
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

    #[cfg(test)]
    fn record_rate_limit_from_headers(
        &self,
        headers: &std::collections::BTreeMap<String, String>,
        client_request_id: &str,
    ) {
        self.record_rate_limit_from_headers_for_route(
            headers,
            (!client_request_id.is_empty()).then_some(client_request_id),
            lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages,
            "anthropic",
        );
    }

    #[cfg(test)]
    fn record_rate_limit_from_headers_at(
        &self,
        headers: &std::collections::BTreeMap<String, String>,
        client_request_id: &str,
        ts_ms: u128,
    ) {
        self.record_rate_limit_from_headers_at_for_route(
            headers,
            (!client_request_id.is_empty()).then_some(client_request_id),
            lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages,
            "anthropic",
            ts_ms,
        );
    }

    fn record_prompt_cache_overage_from_headers(
        &self,
        headers: &std::collections::BTreeMap<String, String>,
        prepared: Option<&crate::client::PreparedPromptCacheContext>,
    ) {
        let Some(prepared) = prepared.filter(|facts| facts.is_subscriber) else {
            return;
        };
        let headers = headers
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<Vec<_>>();
        let decoded =
            lingxi_llm_client::providers::anthropic::limits::quota_status_from_headers(&headers);
        if self.prompt_cache_overage.record(
            &prepared.scope,
            prepared.account_epoch,
            decoded.is_using_overage,
            Self::now_ms(),
        ) {
            *prepared
                .pending_overage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        }
    }

    fn stage_prompt_cache_overage_from_429(
        &self,
        headers: &std::collections::BTreeMap<String, String>,
        prepared: Option<&crate::client::PreparedPromptCacheContext>,
    ) {
        let Some(prepared) = prepared.filter(|facts| facts.is_subscriber) else {
            return;
        };
        let headers = headers
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<Vec<_>>();
        let decoded =
            lingxi_llm_client::providers::anthropic::limits::quota_status_from_429_headers(
                &headers,
            );
        *prepared
            .pending_overage
            .lock()
            .unwrap_or_else(|error| error.into_inner()) =
            Some(crate::PendingPromptCacheObservation {
                scope: prepared.scope.clone(),
                account_epoch: prepared.account_epoch,
                is_using_overage: decoded.is_using_overage,
                observed_at_ms: Self::now_ms(),
            });
    }

    fn record_prompt_cache_overage_from_quota_wait(
        &self,
        status_code: u16,
        headers: &std::collections::BTreeMap<String, String>,
        retry_watchdog_enabled: bool,
        prepared: Option<&crate::client::PreparedPromptCacheContext>,
    ) {
        let Some(prepared) = prepared.filter(|facts| facts.is_subscriber) else {
            return;
        };
        let headers = headers
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<Vec<_>>();
        let Some(wait) =
            lingxi_llm_client::providers::anthropic::limits::retry_quota_window_wait_from_headers(
                status_code,
                &headers,
                retry_watchdog_enabled,
                std::time::SystemTime::now(),
            )
        else {
            return;
        };
        if self.prompt_cache_overage.record(
            &prepared.scope,
            prepared.account_epoch,
            wait.status.is_using_overage,
            Self::now_ms(),
        ) {
            *prepared
                .pending_overage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        }
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
    fn record_rate_limit_from_429_for_route(
        &self,
        headers: &std::collections::BTreeMap<String, String>,
        body: Option<&serde_json::Value>,
        model: &str,
        client_request_id: Option<&str>,
        protocol: lingxi_llm_client::protocol::ProtocolFamily,
        provider_id: &str,
    ) -> bool {
        self.record_rate_limit_from_429_at_for_route(
            headers,
            body,
            model,
            client_request_id,
            protocol,
            provider_id,
            Self::now_ms(),
        )
    }

    fn record_rate_limit_from_429_at_for_route(
        &self,
        headers: &std::collections::BTreeMap<String, String>,
        body: Option<&serde_json::Value>,
        model: &str,
        client_request_id: Option<&str>,
        protocol: lingxi_llm_client::protocol::ProtocolFamily,
        provider_id: &str,
        ts_ms: u128,
    ) -> bool {
        let hvec: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        *self.last_request_id.lock().unwrap() =
            match crate::execution::extract_response_request_id(protocol, provider_id, headers) {
                Some(server_id) => Some((server_id, RequestIdOrigin::Server)),
                None if client_request_id.is_some_and(|id| !id.is_empty()) => {
                    let client_request_id = client_request_id.unwrap_or_default();
                    Some((client_request_id.to_string(), RequestIdOrigin::Client))
                }
                None => None,
            };
        // `extractRawUtilization(headersToUse)` (claudeAiLimits.ts:500) runs
        // for ANY error headers, independent of the limits gate below.
        let decoded =
            lingxi_llm_client::providers::response_headers::ProviderResponseHeaders::decode(
                protocol,
                provider_id,
                &hvec,
                std::time::SystemTime::now(),
            )
            .anthropic_rate_limits
            .unwrap_or_default();
        let raw = RawUtilization::from_decoded(&decoded);
        let info = (protocol == lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages)
            .then(|| {
                lingxi_llm_client::providers::response_headers::AnthropicRateLimitHeaders::decode_429_error(
                    &hvec,
                    body,
                )
            })
            .flatten()
            .as_ref()
            .map(RateLimitInfo::from_decoded_429_error);

        // MESSAGE composition ports errors.ts:482-516 (a LOCAL limits object
        // built from the error headers) — kept verbatim; it is NOT staged and
        // is set per-attempt because the terminal error re-map reads it
        // directly (the most-recent-429 slot, cleared on success).
        let composed = info.as_ref().map(|info| {
            // `formatResetTime(…, true)` analogue for both reset headers
            // (`rateLimitMessages.ts:144-148`), formatted at error time.
            let formatted = formatted_reset_times_from_decoded(&decoded);
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
            return false;
        }
        // Stage EVERY non-stale 429 so the terminal promote can also write the
        // EMPTY raw snapshot and thereby clear stale raw-window state.
        *self.pending_429.lock().unwrap() = Some(Pending429 { info, raw });
        true
    }

    #[cfg(test)]
    fn record_rate_limit_from_429(
        &self,
        headers: &std::collections::BTreeMap<String, String>,
        body: Option<&serde_json::Value>,
        model: &str,
    ) -> bool {
        self.record_rate_limit_from_429_for_route(
            headers,
            body,
            model,
            None,
            lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages,
            "anthropic",
        )
    }

    /// Discard any staged 429 snapshot. Called at drive entry and on success so
    /// a retried-then-recovered 429 (or a non-RateLimited terminal that leaves a
    /// staged slot) cannot promote into a LATER drive. Defensive backstop: the
    /// active cross-drive isolation is the per-attempt record stage-or-clear in
    /// [`Self::record_rate_limit_from_429_for_route`] (a fresh attempt always overwrites
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
    fn promote_pending_429(
        &self,
        prompt_cache: Option<&crate::client::PreparedPromptCacheContext>,
    ) {
        if let Some(pending) = self.pending_429.lock().unwrap().take() {
            if let Some(info) = pending.info {
                *self.last_rate_limit.lock().unwrap() = Some(info);
            }
            *self.last_raw_utilization.lock().unwrap() = Some(pending.raw);
        }
        let Some(prepared) = prompt_cache.filter(|facts| facts.is_subscriber) else {
            return;
        };
        let pending = prepared
            .pending_overage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(pending) = pending {
            if pending.scope == prepared.scope && pending.account_epoch == prepared.account_epoch {
                self.prompt_cache_overage.record(
                    &pending.scope,
                    pending.account_epoch,
                    pending.is_using_overage,
                    pending.observed_at_ms,
                );
            }
        }
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
            LlmError::ProviderTimeout { .. } => err.http_status(),
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
            | LlmError::FileUploadOutcomeUnknown { .. }
            | LlmError::UnsupportedCapability { .. }
            | LlmError::MediaDelegationUnavailable { .. }
            | LlmError::MediaDelegationPartial { .. }
            | LlmError::RequestDispatchRejected { .. } => None,
        }
    }

    // ── Non-stream drive (Step 1 + 1b) ───────────────────────────────────────

    /// Shared non-stream retry driver. Accepts an already-built `LlmRequest` so
    /// purpose-specific canonical request entry points can route here.
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
    ) -> Result<HistoryResponse, LlmError> {
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
    /// `fallback_model` (set by [`Self::execute_non_stream_request`]); on
    /// each [`DriveStep::Fallback`] the loop advances `chain_idx` and rebuilds
    /// `retry_control` with `chain[chain_idx]` (or disables fallback when
    /// exhausted).
    // Keep the provider/retry state machine on the heap so every public wrapper
    // does not embed and move its full future on the caller's stack.
    fn drive_non_stream_seeded_with_chain<'a>(
        &'a self,
        req: LlmRequest,
        retry_control: RetryControl,
        initial_consecutive_overloaded: u8,
        chain: &'a [String],
        dispatch: DispatchHeaderState,
    ) -> crate::BoxFuture<'a, Result<HistoryResponse, LlmError>> {
        Box::pin(self.drive_non_stream_inner(
            req,
            retry_control,
            initial_consecutive_overloaded,
            chain,
            dispatch,
        ))
    }

    #[allow(clippy::too_many_lines)]
    async fn drive_non_stream_inner(
        &self,
        mut req: LlmRequest,
        mut retry_control: RetryControl,
        initial_consecutive_overloaded: u8,
        chain: &[String],
        mut dispatch: DispatchHeaderState,
    ) -> Result<HistoryResponse, LlmError> {
        self.capture_request_session_id(&mut req).await;
        if req.execution.request_credentials.is_none()
            && self
                .client
                .native_api_system_route(&req.input.model, req.profile.as_deref())
        {
            req.execution.request_credentials = Some(crate::RequestCredentials::default());
        }
        let mut safety = crate::safety_observation::SafetyObservation::capture(&mut req);
        dispatch.context_hint_beta = req.execution.context_hint_beta;
        if req.execution.thinking_recovery_scope.is_none() {
            req.execution.thinking_recovery_scope = Some(self.thinking_recovery_scope());
        }

        if req.execution.query_source.is_some() {
            dispatch.auxiliary = query_source_category(req.execution.query_source.as_deref())
                == Some(QuerySourceCategory::Auxiliary);
        }
        if req.execution.input_protocol.is_none() {
            req.execution.input_protocol =
                Some(self.protocol_for_model(&req.input.model, req.profile.as_deref())?);
        }
        let request_id = new_telemetry_id();
        let started = Instant::now();
        let allow_replay = allows_automatic_replay(&req);
        // Sibling connections of this model's provider group, captured from the
        // FIRST prepare: once `req.profile` is pinned to one connection a later
        // resolve sees only that one, so the remaining hops must be held here.
        let mut connection_chain: Vec<crate::ConnectionHop> = Vec::new();
        let mut connection_index = 0usize;
        let mut failover = crate::FailoverTriggers::NONE;
        let mut connections_captured = false;
        telemetry::emit_started(&self.analytics, &req.input.model, &request_id, false).await;
        if let Some(query_source) = req.execution.query_source.as_deref() {
            telemetry::emit_query_source(&self.analytics, &req.input.model, query_source).await;
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
            rate_limit_terminal: rate_limit_cannot_clear(req.profile.as_deref(), &req.input.model),
            ..RetryState::default()
        };
        let retry_scope = ModelCallRetryScope::current_or_new();
        retry_control = retry_scope.configure(&retry_control, &mut state);
        // thinking_budget for telemetry: Adaptive → 0, Enabled{b} → b.
        let thinking_budget: u32 = reasoning_budget(req.input.thinking.as_ref());
        // Index into `chain` for the NEXT fallback entry to use.
        // chain_idx=0 means chain[0] is the current fallback in `retry_control`.
        // After a Fallback step, chain_idx advances to point at the next entry.
        // When chain_idx >= chain.len(), the chain is exhausted.
        let mut chain_idx: usize = 0;
        let mut max_tokens_adjusted = false;
        let mut any_dispatched = false;
        // Native `oSe` is scoped to this logical query and survives every
        // prepare/retry within it; a later query gets a fresh one-shot repair.
        let mut server_fallback_beta_repair_attempted = false;
        let timeout = crate::model::request_timeout::non_stream_timeout()?;
        let mut timeout_retries = crate::model::request_timeout::TimeoutRetryCount::default();
        loop {
            // Strip rejected thinking before encode so Gemini / OpenAI-compat
            // thinking models can prepare. DeepSeek / Kimi skip this.
            // prepare → inject headers → execute.
            let preparing = tokio::time::Instant::now();
            let prepared = crate::execution::non_stream_bound(timeout, async {
                self.refresh_effort_settings(&mut req);
                req.execution.resolve_native_effort = true;
                let mut prepared = self.client.prepare_on(&req, self.transport.clone()).await?;
                self.capture_fast_account(&mut prepared).await?;
                prepared.thinking_display_probe = retry_scope.display_probe();
                Self::log_deepseek_prepared_request(&req.input.model, &prepared, false);
                let (dispatch_attempt, server_fallback_parameter_added) =
                    self.inject_headers(&mut prepared, dispatch)?;
                self.client.seal_prepared(&mut prepared).await?;
                Ok((prepared, dispatch_attempt, server_fallback_parameter_added))
            })
            .await;
            let (mut prepared, dispatch_attempt, server_fallback_parameter_added) = match prepared {
                Ok(prepared) => prepared,
                Err(error) => {
                    telemetry::emit_failed(
                        &self.analytics,
                        &req.input.model,
                        &request_id,
                        Self::error_kind(&error),
                        Self::status_of(&error),
                    )
                    .await;
                    return Err(error);
                }
            };
            if !connections_captured {
                connections_captured = true;
                connection_chain.clone_from(&prepared.route.resolved_route.connection_chain);
                failover = prepared.route.resolved_route.failover;
            }
            // Pause the provider deadline while waiting for host admission.
            let prepare_elapsed = preparing.elapsed();
            let remaining = timeout.saturating_sub(prepare_elapsed);
            let mut attempt = self.begin_model_attempt(&req, &prepared).await?;
            safety.inherit_if_missing(attempt.model_safety_observer());
            let dispatch_started = tokio::time::Instant::now();
            let admission = req.execution.request_dispatch_admission.clone();
            let mut admission_rejected = false;
            let mut dispatch_callback_rejected = false;
            let cache_snapshot = crate::prompt_cache::snapshot_prepared(&prepared);
            let call = prepared.wire_call.take().expect("sealed call");
            let pricing =
                (self.estimator.is_some() || req.execution.model_attempt.is_some()).then(|| {
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
            let resp_result = crate::execution::non_stream_bound(remaining, async {
                if req.execution.computer_submission.is_some()
                    && admission
                        .as_ref()
                        .is_some_and(|admission| !admission.is_admitted())
                {
                    admission_rejected = true;
                    return Err(LlmError::InvalidRequest {
                        message: "host rejected request dispatch admission".into(),
                    });
                }
                prepared.before_computer_submit().await?;
                let received = call
                    .dispatch_once_with(|| {
                        if admission
                            .as_ref()
                            .is_some_and(|admission| !admission.is_admitted())
                        {
                            admission_rejected = true;
                            dispatch_callback_rejected = true;
                            return Err(crate::execution::wire_error(LlmError::InvalidRequest {
                                message: "host rejected request dispatch admission".into(),
                            }));
                        }
                        if let Err(error) = attempt.mark_dispatched() {
                            dispatch_callback_rejected = true;
                            return Err(crate::execution::wire_error(error));
                        }
                        any_dispatched = true;
                        if let Some(admission) = &admission {
                            admission.observe_dispatch();
                        }
                        crate::prompt_cache::observe_snapshot(cache_snapshot.clone());
                        Ok(())
                    })
                    .await
                    .map_err(crate::upstream::error)?;
                received.collect().await.map_err(crate::upstream::error)
            })
            .await;

            if dispatch_callback_rejected {
                if let Some(submission) = req.execution.computer_submission.as_ref() {
                    submission.not_submitted().await?;
                }
            }

            match resp_result {
                Err(transport_err) => {
                    safety.error(&transport_err);
                    if admission_rejected {
                        attempt.finish().await?;
                        return Err(LlmError::RequestDispatchRejected {
                            prior_dispatch: any_dispatched,
                        });
                    }
                    let timeout_limit_exhausted = timeout_retries.exhausted_from_environment(
                        req.execution.failed_stream_outlasted_timeout,
                        &transport_err,
                        timeout,
                        prepare_elapsed.saturating_add(dispatch_started.elapsed()),
                    );
                    attempt.finish().await?;
                    if timeout_limit_exhausted {
                        telemetry::emit_failed(
                            &self.analytics,
                            &req.input.model,
                            &request_id,
                            Self::error_kind(&transport_err),
                            None,
                        )
                        .await;
                        return Err(transport_err);
                    }
                    if !allow_replay {
                        telemetry::emit_failed(
                            &self.analytics,
                            &req.input.model,
                            &request_id,
                            Self::error_kind(&transport_err),
                            Self::status_of(&transport_err),
                        )
                        .await;
                        return Err(transport_err);
                    }
                    if let Some(fallback) = Self::note_dispatch_header_failure(
                        &mut dispatch,
                        &dispatch_attempt,
                        &transport_err,
                        None,
                    ) {
                        self.report_dispatch_fallback(&req, fallback, None).await;
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
                    let step = retry_scope.next_step(
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
                        &req.input.model,
                        &request_id,
                        Self::error_kind(&transport_err),
                        None,
                    )
                    .await;
                    return Err(transport_err);
                }
                Ok(collected) => {
                    let provider_resp = crate::execution::response(
                        collected.response(),
                        prepared.route.protocol,
                        crate::execution::response_provider_id(
                            &prepared.route.resolved_route.provider_id,
                        ),
                    );
                    let outgoing_client_request_id =
                        collected.client_request_id().map(str::to_owned);
                    // Step 1b: x-should-retry: false is terminal for retryable 5xx.
                    let x_should_retry_false = provider_resp
                        .headers
                        .get("x-should-retry")
                        .is_some_and(|v| v.as_str() == "false");

                    let server_fallback_facts = collected.anthropic_fallback();
                    let server_fallback_quote =
                        prepared.server_fallback_lane.as_ref().and_then(|lane| {
                            frozen_server_fallback_quote(
                                pricing.as_ref(),
                                &prepared.route.resolved_route.pricing_model,
                                &lane.model,
                                server_fallback_facts.as_ref(),
                                collected.inference_report(),
                            )
                        });
                    let estimate = match &server_fallback_quote {
                        Some(quote) => quote.estimate.clone(),
                        None => frozen_stream_quote(
                            pricing.as_ref(),
                            &prepared.route.resolved_route.pricing_model,
                            collected.usage_report(),
                            collected.inference_report(),
                        ),
                    };
                    let mut extracted_usage = crate::upstream::usage(
                        collected.usage_report(),
                        collected.inference_report(),
                    );
                    if let Some((usage, completeness)) = &mut extracted_usage {
                        usage.cost_estimate = estimate.clone();
                        if let Some(quote) = &server_fallback_quote {
                            crate::history_projection::attach_server_fallback_cost_quote(
                                &mut usage.provider_metadata,
                                quote.metadata.clone(),
                            );
                        }
                        attempt.observe(usage, *completeness);
                    }
                    let decoded = crate::execution::decode(&collected).and_then(|mut decoded| {
                        safety.response(&decoded);
                        let server_event=prepared.server_fallback_lane.as_ref().and_then(|_|lingxi_llm_client::providers::anthropic::fallback_response::project_nonstream(&mut decoded,&prepared.route.resolved_route.request_model,provider_resp.request_id.clone()));
                        let mut response=crate::history_projection::project_response(
                            decoded,
                            provider_resp.clone(),
                            crate::upstream::family(&prepared.route.protocol),
                        )?;
                        response.set_per_turn_effort(Self::prepared_per_turn_effort(&prepared).as_deref());
                        response.usage.cost_estimate = estimate.clone();
                        if let Some(quote) = &server_fallback_quote {
                            crate::history_projection::attach_server_fallback_cost_quote(
                                &mut response.provider_metadata,
                                quote.metadata.clone(),
                            );
                            crate::history_projection::attach_server_fallback_cost_quote(
                                &mut response.usage.provider_metadata,
                                quote.metadata.clone(),
                            );
                        }
                        if let (Some(event),Some(lane))=(server_event,prepared.server_fallback_lane.clone()) {
                            response.provider_metadata["llm_client"]["server_fallback_events"]=serde_json::json!([crate::history::HistoryServerFallback {event,profile:prepared.route.resolved_route.profile_name.clone(),lane}]);
                        }
                        Ok(response)
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
                            if let Some(binding) = prepared.computer_binding.as_ref() {
                                crate::history_projection::attach_computer_binding(
                                    &mut response.provider_metadata,
                                    binding,
                                );
                            }
                            Self::settle_thinking_display_probe(&prepared, &retry_scope);
                            // Feed rate-limit headers from every 2xx success response.
                            self.record_rate_limit_from_headers_for_route(
                                &provider_resp.headers,
                                outgoing_client_request_id.as_deref(),
                                prepared.route.protocol,
                                crate::execution::response_provider_id(
                                    &prepared.route.resolved_route.provider_id,
                                ),
                            );
                            self.record_prompt_cache_overage_from_headers(
                                &provider_resp.headers,
                                prepared.prompt_cache.as_ref(),
                            );
                            // 3c-T3: populate response.cost when an estimator is wired.
                            // Unpriced or unknown models leave response.cost = None — never an error.
                            response.cost = estimate;
                            let elapsed_ms =
                                u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                            telemetry::emit_succeeded(
                                &self.analytics,
                                &req.input.model,
                                &request_id,
                                elapsed_ms,
                                provider_resp.status,
                            )
                            .await;
                            if req.execution.capture_retry_count {
                                if !response.provider_metadata.is_object() {
                                    response.provider_metadata = serde_json::json!({});
                                }
                                let metadata = response
                                    .provider_metadata
                                    .as_object_mut()
                                    .expect("provider metadata normalized to an object");
                                metadata.insert(
                                    "_lingxi_retry_count".to_string(),
                                    serde_json::Value::from(retry_scope.retry_count()),
                                );
                            }
                            // #5: surface this drive's retry count to the cost
                            // path via `last_retry_count()`.
                            *self.last_retry_count.lock().unwrap() = retry_scope.retry_count();
                            return Ok(response);
                        }
                        Err(decode_err) => {
                            safety.error(&decode_err);
                            if Self::repair_server_fallback_beta_rejection(
                                &prepared,
                                provider_resp.status,
                                &provider_resp.body_json,
                                server_fallback_parameter_added,
                                server_fallback_beta_repair_attempted,
                            ) {
                                server_fallback_beta_repair_attempted = true;
                                continue;
                            }
                            if allow_replay
                                && self.probe_thinking_display_error(
                                    &prepared,
                                    &retry_scope,
                                    provider_resp.status,
                                    &provider_resp.body_json,
                                )
                            {
                                continue;
                            }
                            if allow_replay
                                && self.repair_fast_model_rejection(
                                    &mut req,
                                    &prepared,
                                    provider_resp.status,
                                    &provider_resp.body_json,
                                )
                            {
                                continue;
                            }
                            if allow_replay {
                                if let Some(fallback) = Self::note_dispatch_header_failure(
                                    &mut dispatch,
                                    &dispatch_attempt,
                                    &decode_err,
                                    Some((provider_resp.status, x_should_retry_false)),
                                ) {
                                    self.report_dispatch_fallback(
                                        &req,
                                        fallback,
                                        provider_resp.request_id.as_deref(),
                                    )
                                    .await;
                                    continue;
                                }
                            }
                            // Capacity acceptance precedes retry-decline headers in native eWo.
                            if crate::model::retry_scope::http_retry_decline_is_terminal(
                                provider_resp.status,
                                x_should_retry_false,
                                lingxi_llm_client::providers::anthropic::response_policy::has_overload_payload(&provider_resp.body_json),
                                crate::model::retry::retry_watchdog_from_env(),
                            ) {
                                telemetry::emit_failed(
                                    &self.analytics,
                                    &req.input.model,
                                    &request_id,
                                    Self::error_kind(&decode_err),
                                    Self::status_of(&decode_err),
                                )
                                .await;
                                return Err(decode_err);
                            }

                            // Rate-limited: resolve delay from headers.
                            let effective_err =
                                if let LlmError::RateLimited { retry_after, scope } = &decode_err {
                                    // Task 6 (batch 5): capture the 429's OWN
                                    // unified headers (errors.ts:471-516) so a
                                    // terminal 429 can surface the limits copy.
                                    self.record_rate_limit_from_429_for_route(
                                        &provider_resp.headers,
                                        Some(&provider_resp.body_json),
                                        &req.input.model,
                                        outgoing_client_request_id.as_deref(),
                                        prepared.route.protocol,
                                        crate::execution::response_provider_id(
                                            &prepared.route.resolved_route.provider_id,
                                        ),
                                    );
                                    self.stage_prompt_cache_overage_from_429(
                                        &provider_resp.headers,
                                        prepared.prompt_cache.as_ref(),
                                    );
                                    let delay = retry_after.or_else(|| {
                                        Self::resolve_retry_after(
                                            &provider_resp.headers,
                                            prepared.route.protocol,
                                            crate::execution::response_provider_id(
                                                &prepared.route.resolved_route.provider_id,
                                            ),
                                        )
                                    });
                                    if let Some(delay) = delay {
                                        telemetry::emit_rate_limited(
                                            &self.analytics,
                                            &req.input.model,
                                            u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                                        )
                                        .await;
                                    }
                                    LlmError::RateLimited {
                                        retry_after: delay,
                                        scope: scope.clone(),
                                    }
                                } else {
                                    decode_err.clone()
                                };

                            if !allow_replay {
                                if matches!(decode_err, LlmError::RateLimited { .. }) {
                                    self.promote_pending_429(prepared.prompt_cache.as_ref());
                                }
                                telemetry::emit_failed(
                                    &self.analytics,
                                    &req.input.model,
                                    &request_id,
                                    Self::error_kind(&decode_err),
                                    Self::status_of(&decode_err),
                                )
                                .await;
                                return Err(decode_err);
                            }

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
                                if crate::auth::external_aws::is_aws_auth_error(
                                    &decode_err,
                                    &prepared.route.resolved_route.provider_id,
                                ) && retry_scope.take_credential_renewal()
                                {
                                    aws.refresh().await;
                                    continue;
                                }
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
                                retry_scope.next_step(
                                    &mut state,
                                    &retry_control,
                                    &effective_err,
                                    thinking_budget,
                                    self.settings_backoff_ms,
                                ),
                                req.input.max_tokens,
                                max_tokens_adjusted,
                            );
                            match step {
                                DriveStep::RetryAfter(delay) => {
                                    self.record_prompt_cache_overage_from_quota_wait(
                                        provider_resp.status,
                                        &provider_resp.headers,
                                        retry_control.watchdog,
                                        prepared.prompt_cache.as_ref(),
                                    );
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
                                            &req.input.model,
                                            overflow.input_tokens,
                                            overflow.context_limit,
                                            new_max,
                                            state.attempt,
                                        )
                                        .await;
                                    }
                                    max_tokens_adjusted = true;
                                    req.input.max_tokens = Some(new_max);
                                    continue;
                                }
                                DriveStep::StripThinkingSignature => {
                                    if self.handle_thinking_signature_strip(&mut req).await {
                                        continue;
                                    }
                                    telemetry::emit_failed(
                                        &self.analytics,
                                        &req.input.model,
                                        &request_id,
                                        Self::error_kind(&decode_err),
                                        Self::status_of(&decode_err),
                                    )
                                    .await;
                                    return Err(decode_err);
                                }
                                DriveStep::Fallback { fallback_model } => {
                                    if req.execution.model_attempt.is_some() {
                                        return Err(decode_err);
                                    }
                                    // Switch to the fallback model; advance the
                                    // chain index so the next iteration's ctl
                                    // points at chain[chain_idx] (or is
                                    // exhausted → allow_fallback=false).
                                    strip_signature_blocks_for_fallback(&mut req.input.messages);
                                    req.input.model = fallback_model;
                                    chain_idx += 1;
                                    // Reset the consecutive-overload counter so
                                    // the new primary model's 529 budget is fresh.
                                    state.consecutive_overloaded = 0;
                                    retry_scope.reset_model_overloads();
                                    // Rebuild retry_control with the next chain
                                    // entry (None when exhausted).
                                    let next_fallback = chain.get(chain_idx).cloned();
                                    let allow_fallback = next_fallback.is_some();
                                    retry_control = resolve_retry_control_with_settings(
                                        &req.input.model,
                                        next_fallback,
                                        sub.is_subscriber,
                                        &ResolveRetryEnv::from_process_env(),
                                        self.settings_max_retries,
                                    );
                                    if allow_fallback {
                                        retry_control.allow_fallback = true;
                                    }
                                    retry_control =
                                        retry_scope.configure(&retry_control, &mut state);
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
                                        self.promote_pending_429(prepared.prompt_cache.as_ref());
                                    }
                                    telemetry::emit_failed(
                                        &self.analytics,
                                        &req.input.model,
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
                                        &req.input.model,
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

    /// Build a main request without clearing the session's structured-output
    /// or thinking policy, then execute every requested control together.
    pub async fn messages_create(
        &self,
        request: MessagesCreateRequest,
    ) -> Result<HistoryResponse, LlmError> {
        let (req, retry) = self.build_main_message_request(request, false)?;
        self.execute_non_stream_request(req, NonStreamingRequestClass::Main, retry)
            .await
    }

    /// Use the ordinary streaming driver, admitting output only after its full
    /// assistant response completes. Native computer callers share the normal
    /// completed-response dispatcher and never act on partial stream blocks.
    pub async fn messages_create_buffered_stream(
        &self,
        request: MessagesCreateRequest,
    ) -> Result<HistoryResponse, LlmError> {
        let (req, _retry) = self.build_main_message_request(request, true)?;
        let stream = self.drive_stream(req).await?;
        crate::stream_accumulator::accumulate_stream_salvaging(stream)
            .await
            .map_err(|(_, error)| error)
    }

    fn build_main_message_request(
        &self,
        request: MessagesCreateRequest,
        stream: bool,
    ) -> Result<(LlmRequest, NonStreamingRetryOptions), LlmError> {
        let MessagesCreateRequest {
            model,
            profile,
            system,
            messages,
            tools,
            opts,
        } = request;
        let mut req = self.build_main_request(
            &model,
            profile.as_deref(),
            system.as_ref(),
            messages,
            tools,
            stream,
            opts.max_output_tokens,
            opts.skip_global_cache_for_system_prompt,
            prompt_cache_query_source(opts.query_source.as_deref()),
        )?;
        req.input.controls.anthropic.context_hint = opts.context_hint;
        req.execution.context_hint_beta = opts.context_hint_beta;
        req.execution.model_attempt = opts.model_attempt;
        req.execution
            .set_request_dispatch_admission(opts.request_dispatch_admission);
        req.execution.query_source = opts.query_source;
        req.execution.failed_stream_outlasted_timeout = opts.failed_stream_outlasted_timeout;
        req.execution.stream_fallback = opts.initial_consecutive_overloaded.is_some();
        Ok((
            req,
            NonStreamingRetryOptions {
                initial_consecutive_overloaded: opts.initial_consecutive_overloaded,
                fallback: opts.fallback,
            },
        ))
    }

    /// Execute a canonical request through the physical retry, watchdog and
    /// registered-attempt driver without changing its body policy.
    pub async fn execute_non_stream_request(
        &self,
        mut request: LlmRequest,
        class: NonStreamingRequestClass,
        retry: NonStreamingRetryOptions,
    ) -> Result<HistoryResponse, LlmError> {
        request.stream = false;
        let model = &request.input.model;
        let display_model = self
            .alias_to_display
            .get(model)
            .map_or(model.as_str(), String::as_str);
        let chain = match retry.fallback {
            FallbackPolicy::Disabled => Vec::new(),
            FallbackPolicy::Models(models) => models,
            FallbackPolicy::Configured => self
                .fallback_overrides
                .get(display_model)
                .cloned()
                .unwrap_or_else(|| self.fallback_models.clone()),
        };
        let mut control = resolve_retry_control_with_settings(
            model,
            chain.first().cloned(),
            self.effective_subscriber().is_subscriber,
            &ResolveRetryEnv::from_process_env(),
            self.settings_max_retries,
        );
        if !chain.is_empty() {
            control.allow_fallback = true;
        }
        let dispatch = match class {
            NonStreamingRequestClass::Main => DispatchHeaderState::default(),
            NonStreamingRequestClass::Auxiliary => DispatchHeaderState::AUXILIARY,
        };
        self.drive_non_stream_seeded_with_chain(
            request,
            control,
            retry.initial_consecutive_overloaded.unwrap_or(0),
            &chain,
            dispatch,
        )
        .await
    }

    /// Build a scheduled main turn with its own reasoning policy. The turn
    /// retains its computer scope even though scheduled wire policy is auxiliary.
    pub fn build_scheduled_request(
        &self,
        request: MessagesCreateRequest,
        thinking: crate::model::thinking::ThinkingConfig,
        effort: Option<serde_json::Value>,
    ) -> Result<LlmRequest, LlmError> {
        let model = request.model.clone();
        let (mut req, _) =
            crate::thinking_scope::isolated(|| self.build_main_message_request(request, false))?;
        if MOD_REQUEST_EFFORT.try_with(|_| ()).is_ok() {
            if let Some(thinking) = req.input.thinking.as_mut() {
                thinking.effort = None;
            }
        }
        req.set_tool_choice(None);
        req.execution.capture_retry_count = true;
        req.execution.anthropic_request_kind = lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery;
        self.apply_side_query_thinking(&mut req, &model, Some(thinking), None);
        req.set_effort(effort)?;
        Ok(req)
    }

    /// Build the canonical non-strict side-query request used by both
    /// estimation and the live Session dispatch path.
    #[allow(clippy::too_many_arguments)]
    pub fn build_side_query_request_with_thinking(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        skip_global_cache_for_system_prompt: bool,
        messages: Vec<ConversationMessage>,
        tools: Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
        max_tokens: Option<u32>,
        tool_choice: Option<crate::ToolChoice>,
        stop_sequences: Vec<String>,
        thinking: Option<crate::model::thinking::ThinkingConfig>,
        effort: Option<serde_json::Value>,
        temperature: Option<f32>,
        query_source: Option<&str>,
    ) -> Result<LlmRequest, LlmError> {
        let mut req = crate::thinking_scope::isolated(|| {
            self.build_request(
                model,
                profile,
                system,
                messages,
                tools,
                false,
                max_tokens,
                skip_global_cache_for_system_prompt,
                prompt_cache_query_source(query_source),
            )
        })?;
        // A main turn's Mod effort is task-local. Side queries assembled in
        // that same task keep their own explicit/default effort instead.
        if MOD_REQUEST_EFFORT.try_with(|_| ()).is_ok() {
            if let Some(thinking) = req.input.thinking.as_mut() {
                thinking.effort = None;
            }
        }
        req.set_tool_choice(tool_choice);
        req.input.stop_sequences = stop_sequences;
        req.execution.capture_retry_count = true;
        req.execution.anthropic_request_kind = lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery;
        req.execution.query_source = query_source.map(str::to_string);
        self.apply_side_query_thinking(&mut req, model, thinking, temperature);
        req.set_effort(effort)?;
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
        let system = custom_system_prompt(system);
        let mut req = self.build_request(
            model,
            profile,
            system.as_ref(),
            messages,
            Vec::new(),
            true,
            max_tokens,
            false,
            prompt_cache_query_source(query_source),
        )?;
        req.input.tool_choice = lingxi_llm_client::protocol::ToolChoice::Auto;
        req.execution.anthropic_request_kind = lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery;
        req.input.output_format = lingxi_llm_client::protocol::OutputFormat::JsonSchema {
            name: "response".into(),
            schema,
            strict: true,
        };
        self.apply_side_query_thinking(&mut req, model, thinking, temperature);
        req.set_effort(effort)?;
        if query_source.is_some() {
            req.execution.query_source = query_source.map(str::to_string);
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
        tools: Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
        max_tokens: Option<u32>,
        tool_choice: Option<crate::ToolChoice>,
        stop_sequences: Vec<String>,
        temperature: Option<f32>,
        query_source: Option<&str>,
    ) -> Result<HistoryResponse, LlmError> {
        let system = custom_system_prompt(system);
        let mut req = crate::thinking_scope::isolated(|| {
            self.build_request(
                model,
                profile,
                system.as_ref(),
                messages,
                tools,
                false,
                max_tokens,
                false,
                prompt_cache_query_source(query_source),
            )
        })?;

        // `build_request` applies main-turn-only overrides. A forked summary
        // owns these fields independently, so restore its explicit values.
        req.set_tool_choice(tool_choice);
        req.execution.anthropic_request_kind = lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery;
        req.input.stop_sequences = stop_sequences;
        req.input.temperature = temperature;
        req.execution.query_source = query_source.map(str::to_string);

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
        system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        skip_global_cache_for_system_prompt: bool,
        messages: Vec<ConversationMessage>,
        tools: Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
        max_tokens: Option<u32>,
        tool_choice: Option<crate::ToolChoice>,
        stop_sequences: Vec<String>,
        thinking: Option<crate::model::thinking::ThinkingConfig>,
        effort: Option<serde_json::Value>,
        temperature: Option<f32>,
        query_source: Option<&str>,
    ) -> Result<HistoryResponse, LlmError> {
        let req = self.build_side_query_request_with_thinking(
            model,
            profile,
            system,
            skip_global_cache_for_system_prompt,
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
        tools: Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
        max_tokens: Option<u32>,
        tool_choice: Option<crate::ToolChoice>,
        stop_sequences: Vec<String>,
        temperature: Option<f32>,
        query_source: Option<&str>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        let system = custom_system_prompt(system);
        let mut req = crate::thinking_scope::isolated(|| {
            self.build_request(
                model,
                profile,
                system.as_ref(),
                messages,
                tools,
                true,
                max_tokens,
                false,
                prompt_cache_query_source(query_source),
            )
        })?;
        req.set_tool_choice(tool_choice);
        req.execution.anthropic_request_kind = lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery;
        req.input.stop_sequences = stop_sequences;
        req.input.temperature = temperature;
        req.execution.query_source = query_source.map(str::to_string);
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
        tools: Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
        max_tokens: Option<u32>,
        tool_choice: Option<crate::ToolChoice>,
        stop_sequences: Vec<String>,
        thinking: Option<crate::model::thinking::ThinkingConfig>,
        temperature: Option<f32>,
        query_source: Option<&str>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        let system = custom_system_prompt(system);
        let mut req = crate::thinking_scope::isolated(|| {
            self.build_request(
                model,
                profile,
                system.as_ref(),
                messages,
                tools,
                true,
                max_tokens,
                false,
                prompt_cache_query_source(query_source),
            )
        })?;
        req.set_tool_choice(tool_choice);
        req.execution.anthropic_request_kind = lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery;
        req.input.stop_sequences = stop_sequences;
        req.execution.query_source = query_source.map(str::to_string);
        self.apply_side_query_thinking(&mut req, model, thinking, temperature);
        self.drive_stream(req).await
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
        tools: Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    ) -> Result<u64, LlmError> {
        let system = custom_system_prompt(system);
        let req = self.build_request(
            model,
            profile,
            system.as_ref(),
            messages,
            tools,
            false,
            None,
            false,
            PromptCacheQuerySource::Unspecified,
        )?;
        crate::model::count_tokens::count_tokens(self.client.as_ref(), self.transport.clone(), &req)
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
        tools: Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    ) -> Result<Option<u64>, LlmError> {
        let system = custom_system_prompt(system);
        let req = self.build_request(
            model,
            profile,
            system.as_ref(),
            messages,
            tools,
            false,
            None,
            false,
            PromptCacheQuerySource::Unspecified,
        )?;
        crate::model::count_tokens::try_count_tokens_exact(
            self.client.as_ref(),
            self.transport.clone(),
            &req,
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
            .preconnect_websocket(&request, self.transport.clone(), &mut session)
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
        system: Option<&crate::SystemPromptInput>,
        messages: Vec<ConversationMessage>,
        tools: Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
        skip_global_cache_for_system_prompt: bool,
    ) -> Result<(), LlmError> {
        let mut req = self.build_request(
            model,
            profile,
            system,
            messages,
            tools,
            true,
            None,
            skip_global_cache_for_system_prompt,
            PromptCacheQuerySource::Unspecified,
        )?;
        self.refresh_effort_settings(&mut req);
        req.execution.resolve_native_effort = true;
        let mut prepared = self.client.prepare_on(&req, self.transport.clone()).await?;
        // Responses-WebSocket prewarm only — never a first-party Anthropic
        // route, so the dispatch gate is inert here.
        self.inject_stream_headers(&mut prepared, DispatchHeaderState::default())?;
        let mut session = self.responses_ws_session.lock().await;
        self.client
            .prewarm_prepared_websocket(prepared, self.transport.clone(), &mut session)
            .await
    }

    /// Close any reusable Responses WebSocket session held by this service. Backs
    /// the `OrchestratorApiClient::close_responses_websocket_session` trait
    /// override.
    pub async fn close_responses_websocket_session(&self) -> Result<(), LlmError> {
        let mut session = self.responses_ws_session.lock().await;
        session.close().await.map_err(crate::upstream::error)
    }

    // ── Stream drive (Step 2) ─────────────────────────────────────────────────

    /// Drive a streaming call; connect-phase failures retry through the driver.
    ///
    /// Uses `ModelRuntime::execute_stream` which already handles the
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
    // The streaming driver owns the same large provider lifecycle as the
    // non-stream driver; bound its callers' futures at the same layer.
    fn drive_stream(
        &self,
        req: LlmRequest,
    ) -> crate::BoxFuture<'_, Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError>>
    {
        Box::pin(self.drive_stream_inner(req))
    }

    #[allow(clippy::too_many_lines)]
    async fn drive_stream_inner(
        &self,
        mut req: LlmRequest,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        self.capture_request_session_id(&mut req).await;
        if req.execution.request_credentials.is_none()
            && self
                .client
                .native_api_system_route(&req.input.model, req.profile.as_deref())
        {
            req.execution.request_credentials = Some(crate::RequestCredentials::default());
        }
        let mut safety = crate::safety_observation::SafetyObservation::capture(&mut req);
        if req.execution.input_protocol.is_none() {
            req.execution.input_protocol =
                Some(self.protocol_for_model(&req.input.model, req.profile.as_deref())?);
        }
        let request_id = new_telemetry_id();
        let allow_replay = allows_automatic_replay(&req);
        if req.execution.thinking_recovery_scope.is_none() {
            req.execution.thinking_recovery_scope = Some(self.thinking_recovery_scope());
        }

        telemetry::emit_started(&self.analytics, &req.input.model, &request_id, true).await;
        if let Some(query_source) = req.execution.query_source.as_deref() {
            telemetry::emit_query_source(&self.analytics, &req.input.model, query_source).await;
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
            rate_limit_terminal: rate_limit_cannot_clear(req.profile.as_deref(), &req.input.model),
            ..RetryState::default()
        };
        // Stream path uses settings-based retry control (same precedence as non-stream).
        let ctl = resolve_retry_control_with_settings(
            &req.input.model,
            None, // fallback not used on stream connect-phase
            sub.is_subscriber,
            &ResolveRetryEnv::from_process_env(),
            self.settings_max_retries,
        );
        let retry_scope = ModelCallRetryScope::current_or_new();
        let ctl = retry_scope.configure(&ctl, &mut state);
        let thinking_budget: u32 = reasoning_budget(req.input.thinking.as_ref());
        // Connection failover state — see the non-stream drive for why the chain
        // is captured once. The stream connect phase had NO fallback of any kind
        // before this, which is why `routing.fallback` never ran on desktop or
        // mobile (both drive turns through `StreamingTurnDriver`).
        let mut connection_chain: Vec<crate::ConnectionHop> = Vec::new();
        let mut connection_index = 0usize;
        let mut failover = crate::FailoverTriggers::NONE;
        let mut connections_captured = false;
        let mut max_tokens_adjusted = false;
        let mut any_dispatched = false;
        // Native `oSe` is per query, not per HTTP attempt.
        let mut server_fallback_beta_repair_attempted = false;
        // Reset per-query state and classify the actual source independently
        // of stream/body policy: prompt hooks and scheduled streams are auxiliary.
        let mut dispatch =
            DispatchHeaderState::for_query_source(req.execution.query_source.as_deref());
        dispatch.context_hint_beta = req.execution.context_hint_beta;

        loop {
            // Prepare so we can inject headers, then call execute_stream via
            // a thin wrapper transport that uses our already-modified request.
            self.refresh_effort_settings(&mut req);
            req.execution.resolve_native_effort = true;
            let mut prepared = match self.client.prepare_on(&req, self.transport.clone()).await {
                Ok(p) => p,
                Err(e) => return Err(e),
            };
            if !connections_captured {
                connections_captured = true;
                connection_chain.clone_from(&prepared.route.resolved_route.connection_chain);
                failover = prepared.route.resolved_route.failover;
            }
            Self::log_deepseek_prepared_request(&req.input.model, &prepared, true);
            tracing::debug!(model = %req.input.model, event = "request_prepared");
            self.capture_fast_account(&mut prepared).await?;
            prepared.thinking_display_probe = retry_scope.display_probe();
            let (dispatch_attempt, server_fallback_parameter_added) =
                self.inject_stream_headers(&mut prepared, dispatch)?;

            let provider = &prepared.route.resolved_route.provider_id;
            let body_bytes = prepared.provider_request.wire_body_bytes()?.len();
            let first_byte_timeout = self.stream_first_byte_timeout_override.or_else(|| {
                crate::model::stream_watchdog::resolve_stream_first_byte_timeout(
                    provider, body_bytes,
                )
            });

            let mut attempt = crate::model_attempt::WireAttempt::new(None);
            let admission = req.execution.request_dispatch_admission.clone();
            let mut admission_rejected = false;

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
                let mut http_session = ResponsesSession::new();
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
                safety.inherit_if_missing(attempt.model_safety_observer());
                let cache_snapshot = crate::prompt_cache::snapshot_prepared(&prepared);
                crate::execution::first_byte_bound(remaining, async {
                    if req.execution.computer_submission.is_some()
                        && admission
                            .as_ref()
                            .is_some_and(|admission| !admission.is_admitted())
                    {
                        admission_rejected = true;
                        return Err(LlmError::InvalidRequest {
                            message: "host rejected request dispatch admission".into(),
                        });
                    }
                    self.client
                        .open_shared_stream(prepared, responses_ws_session, &mut || {
                            if admission
                                .as_ref()
                                .is_some_and(|admission| !admission.is_admitted())
                            {
                                admission_rejected = true;
                                return Err(LlmError::InvalidRequest {
                                    message: "host rejected request dispatch admission".into(),
                                });
                            }
                            attempt.mark_dispatched()?;
                            any_dispatched = true;
                            if let Some(admission) = &admission {
                                admission.observe_dispatch();
                            }
                            crate::prompt_cache::observe_snapshot(cache_snapshot.clone());
                            Ok(())
                        })
                        .await
                })
                .await
            }
            .await;
            match opened {
                Err(transport_err) => {
                    safety.error(&transport_err);
                    if admission_rejected {
                        attempt.finish().await?;
                        return Err(LlmError::RequestDispatchRejected {
                            prior_dispatch: any_dispatched,
                        });
                    }
                    attempt.finish().await?;
                    if !allow_replay {
                        telemetry::emit_failed(
                            &self.analytics,
                            &req.input.model,
                            &request_id,
                            Self::error_kind(&transport_err),
                            Self::status_of(&transport_err),
                        )
                        .await;
                        return Err(transport_err);
                    }
                    // The oracle permits one `StreamNoResponse` retry across the
                    // whole request, then terminates before generic retry logic.
                    // On the first occurrence it still flows through dispatch
                    // degradation and the normal retry/backoff classifier.
                    if crate::model::stream_watchdog::is_stream_no_response(&transport_err)
                        && !retry_scope.take_no_response_retry()
                    {
                        return Err(transport_err);
                    }
                    if let Some(fallback) = Self::note_dispatch_header_failure(
                        &mut dispatch,
                        &dispatch_attempt,
                        &transport_err,
                        None,
                    ) {
                        self.report_dispatch_fallback(&req, fallback, None).await;
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
                    let step = retry_scope.next_step(
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
                    let outgoing_client_request_id =
                        streaming.client_request_id().map(str::to_owned);
                    let response_headers: std::collections::BTreeMap<String, String> = streaming
                        .headers()
                        .iter()
                        .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
                        .collect();
                    tracing::debug!(
                        model = %req.input.model,
                        event = "stream_opened",
                        status = streaming.status()
                    );
                    // Connect-phase status ≥ 400: drain and decode as error.
                    if streaming.status() >= 400 {
                        let response_status = streaming.status();
                        // Error responses do not enter the event watchdog below.
                        // Bound their body collection too, respecting the host's
                        // explicit watchdog-disable setting.
                        let timeout = self
                            .stream_idle_timeout_override
                            .or_else(crate::model::stream_watchdog::resolve_stream_idle_timeout);
                        let collect =
                            async { streaming.collect().await.map_err(crate::upstream::error) };
                        let result = match timeout {
                            Some(timeout) => tokio::time::timeout(timeout, collect)
                                .await
                                .unwrap_or_else(|_| {
                                    Err(LlmError::TransportTimeout {
                                        message: "Timed out reading provider error response".into(),
                                    })
                                }),
                            None => collect.await,
                        };
                        let collected = match result {
                            Ok(collected) => collected,
                            Err(error) => {
                                safety.error(&error);
                                attempt.finish().await?;
                                return Err(error);
                            }
                        };
                        let body_json = crate::execution::response(
                            collected.response(),
                            prepared.route.protocol,
                            crate::execution::response_provider_id(
                                &prepared.route.resolved_route.provider_id,
                            ),
                        )
                        .body_json;
                        let outgoing_client_request_id =
                            collected.client_request_id().map(str::to_owned);
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
                        safety.error(&decode_err);
                        collected.finish().await;
                        attempt.finish().await?;

                        // Mirror the non-stream path: for 429s, resolve the
                        // actual retry delay from the real response headers
                        // (retry-after / anthropic-ratelimit-*).  Absent hints leave delay selection to the retry policy.
                        let effective_err =
                            if let LlmError::RateLimited { retry_after, scope } = &decode_err {
                                // Task 6 (batch 5): same 429-error-header capture
                                // as the non-stream path (errors.ts:471-516).
                                self.record_rate_limit_from_429_for_route(
                                    &response_headers,
                                    Some(&body_json),
                                    &req.input.model,
                                    outgoing_client_request_id.as_deref(),
                                    prepared.route.protocol,
                                    crate::execution::response_provider_id(
                                        &prepared.route.resolved_route.provider_id,
                                    ),
                                );
                                self.stage_prompt_cache_overage_from_429(
                                    &response_headers,
                                    prepared.prompt_cache.as_ref(),
                                );
                                LlmError::RateLimited {
                                    retry_after: retry_after.or_else(|| {
                                        Self::resolve_retry_after(
                                            &response_headers,
                                            prepared.route.protocol,
                                            crate::execution::response_provider_id(
                                                &prepared.route.resolved_route.provider_id,
                                            ),
                                        )
                                    }),
                                    scope: scope.clone(),
                                }
                            } else {
                                decode_err.clone()
                            };

                        if Self::repair_server_fallback_beta_rejection(
                            &prepared,
                            response_status,
                            &body_json,
                            server_fallback_parameter_added,
                            server_fallback_beta_repair_attempted,
                        ) {
                            server_fallback_beta_repair_attempted = true;
                            continue;
                        }

                        if allow_replay
                            && self.probe_thinking_display_error(
                                &prepared,
                                &retry_scope,
                                response_status,
                                &body_json,
                            )
                        {
                            continue;
                        }

                        let declined = response_headers
                            .get("x-should-retry")
                            .is_some_and(|value| value == "false");
                        // Native server/overload model gates precede retry hints.
                        // SDK stateful execution still requires an explicit host decision.
                        if allow_replay && retry_scope.request_http_model_fallback(response_status, lingxi_llm_client::providers::anthropic::response_policy::has_overload_payload(&body_json))
                        {
                            return Err(decode_err);
                        }
                        if allow_replay
                            && self.repair_fast_model_rejection(
                                &mut req,
                                &prepared,
                                response_status,
                                &body_json,
                            )
                        {
                            continue;
                        }
                        if allow_replay {
                            if let Some(fallback) = Self::note_dispatch_header_failure(
                                &mut dispatch,
                                &dispatch_attempt,
                                &decode_err,
                                Some((response_status, declined)),
                            ) {
                                self.report_dispatch_fallback(
                                    &req,
                                    fallback,
                                    crate::execution::extract_response_request_id(
                                        prepared.route.protocol,
                                        crate::execution::response_provider_id(
                                            &prepared.route.resolved_route.provider_id,
                                        ),
                                        &response_headers,
                                    )
                                    .as_deref(),
                                )
                                .await;
                                continue;
                            }
                        }
                        if !allow_replay
                            || crate::model::retry_scope::http_retry_decline_is_terminal(
                                response_status,
                                declined,
                                lingxi_llm_client::providers::anthropic::response_policy::has_overload_payload(&body_json),
                                crate::model::retry::retry_watchdog_from_env(),
                            )
                        {
                            if matches!(decode_err, LlmError::RateLimited { .. }) {
                                self.promote_pending_429(prepared.prompt_cache.as_ref());
                            }
                            telemetry::emit_failed(
                                &self.analytics,
                                &req.input.model,
                                &request_id,
                                Self::error_kind(&decode_err),
                                Self::status_of(&decode_err),
                            )
                            .await;
                            return Err(decode_err);
                        }

                        // 2.1.198 `V_c`/`G_c`/`s_f` + `Ygf` — stream connect
                        // twin of the non-stream AWS auth-refresh hook (see
                        // `drive_non_stream_seeded_with_chain`).
                        if let Some(aws) = &self.aws_auth {
                            if crate::auth::external_aws::is_aws_auth_error(
                                &decode_err,
                                &prepared.route.resolved_route.provider_id,
                            ) && retry_scope.take_credential_renewal()
                            {
                                aws.refresh().await;
                                continue;
                            }
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
                            retry_scope.next_step(
                                &mut state,
                                &ctl,
                                &effective_err,
                                thinking_budget,
                                self.settings_backoff_ms,
                            ),
                            req.input.max_tokens,
                            max_tokens_adjusted,
                        );
                        match step {
                            DriveStep::RetryAfter(delay) => {
                                self.record_prompt_cache_overage_from_quota_wait(
                                    response_status,
                                    &response_headers,
                                    ctl.watchdog,
                                    prepared.prompt_cache.as_ref(),
                                );
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
                                            &req.input.model,
                                            overflow.input_tokens,
                                            overflow.context_limit,
                                            new_max,
                                            state.attempt,
                                        )
                                        .await;
                                    }
                                }
                                max_tokens_adjusted = true;
                                req.input.max_tokens = Some(new_max);
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
                            self.promote_pending_429(prepared.prompt_cache.as_ref());
                        }
                        // Terminal twin for the connect-phase emit_started
                        // (mirrors the non-stream terminal arms).
                        telemetry::emit_failed(
                            &self.analytics,
                            &req.input.model,
                            &request_id,
                            Self::error_kind(&decode_err),
                            Self::status_of(&decode_err),
                        )
                        .await;
                        return Err(decode_err);
                    }

                    self.record_rate_limit_from_headers_for_route(
                        &response_headers,
                        outgoing_client_request_id.as_deref(),
                        prepared.route.protocol,
                        crate::execution::response_provider_id(
                            &prepared.route.resolved_route.provider_id,
                        ),
                    );
                    self.record_prompt_cache_overage_from_headers(
                        &response_headers,
                        prepared.prompt_cache.as_ref(),
                    );
                    *self.last_retry_count.lock().unwrap() = retry_scope.retry_count();
                    let mut decoder = crate::history_projection::HistoryProjector::projection(
                        crate::upstream::family(&prepared.route.protocol),
                        crate::stream_provider_metadata_from_headers(&response_headers),
                    );
                    if let Some(binding) = prepared.computer_binding.as_ref() {
                        crate::history_projection::attach_computer_binding(
                            &mut decoder.metadata,
                            binding,
                        );
                    }
                    decoder.admit_server_fallback(
                        prepared.server_fallback_lane.clone(),
                        prepared.route.resolved_route.profile_name.clone(),
                        prepared.route.resolved_route.request_model.clone(),
                        crate::execution::extract_response_request_id(
                            prepared.route.protocol,
                            crate::execution::response_provider_id(
                                &prepared.route.resolved_route.provider_id,
                            ),
                            &response_headers,
                        ),
                    );
                    let mut frames = streaming
                        .into_stream()
                        .map_err(|_| LlmError::ProviderInternal)?;
                    let pricing_model = prepared.route.resolved_route.pricing_model.clone();
                    let pricing = frames
                        .pricing_snapshot()
                        .filter(|_| {
                            self.estimator.is_some() || req.execution.model_attempt.is_some()
                        })
                        .map(|snapshot| {
                            self.estimator.as_ref().map_or_else(
                                || snapshot.clone(),
                                |estimator| estimator.capture(snapshot.clone(), &pricing_model),
                            )
                        });

                    Self::settle_thinking_display_probe(&prepared, &retry_scope);

                    // Clone analytics + metadata into the unfold state so
                    // emit_succeeded / emit_failed can fire from inside the async closure.
                    let stream_started = Instant::now();
                    let stream_analytics = self.analytics.clone();
                    let stream_model = req.input.model.clone();
                    let stream_request_id = request_id.clone();
                    // Streaming idle watchdog (cc 2.1.196 default-on): resolve
                    // the per-event idle timeout from the env once at
                    // stream-open. `None` when disabled.
                    let stream_idle_timeout = self
                        .stream_idle_timeout_override
                        .or_else(crate::model::stream_watchdog::resolve_stream_idle_timeout);

                    // Native body-phase recovery requires a carried dispatch header
                    // and a connection failure before the first forwarded event.
                    let mut seed = None;
                    if dispatch_attempt.value.is_some() && allow_replay {
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
                                // An admitted server lane can still resolve to
                                // an ordinary response. Defer its quote until
                                // typed fallback iterations arrive or the
                                // terminal boundary proves there is no cNe
                                // branch; otherwise a later native quote would
                                // be preceded by a false request-model total.
                                usage.cost_estimate = if prepared.server_fallback_lane.is_none() {
                                    frozen_stream_quote(
                                        pricing.as_ref(),
                                        &pricing_model,
                                        &batch.usage,
                                        &batch.inference,
                                    )
                                } else {
                                    None
                                };
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
                            // Native x2 body classification requires a connection cause;
                            // SDK timeout/abort errors alone do not supply that cause.
                            if matches!(
                                first_err,
                                LlmError::Transport { .. } | LlmError::TlsCert { .. }
                            ) {
                                if let Some(fallback) = dispatch.on_body_failure(&dispatch_attempt)
                                {
                                    attempt.finish().await?;
                                    self.report_dispatch_fallback(
                                        &req,
                                        fallback,
                                        crate::execution::extract_response_request_id(
                                            prepared.route.protocol,
                                            crate::execution::response_provider_id(
                                                &prepared.route.resolved_route.provider_id,
                                            ),
                                            &response_headers,
                                        )
                                        .as_deref(),
                                    )
                                    .await;
                                    continue;
                                }
                            }
                        }
                        seed = Some(first);
                    }

                    // Assemble events via a manual unfold that drives next_frame + decode.
                    // We keep a queue of pre-decoded events and drain them first.
                    let stream_state = StreamState {
                        per_turn_effort: Self::prepared_per_turn_effort(&prepared),
                        safety: safety.clone(),
                        attempt,
                        decoder,
                        frames,
                        pricing,
                        pricing_model,
                        server_fallback_lane: prepared.server_fallback_lane.clone(),
                        server_fallback_quote_finalized: false,
                        server_fallback_quote_candidate_seen: false,
                        server_fallback_quote_metadata: None,
                        server_fallback_quote_estimate: None,
                        pending_service_error: None,
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

                    let boxed: BoxStream<'static, Result<HistoryEvent, LlmError>> = Box::pin(
                        futures::stream::unfold(stream_state, |mut s| async move {
                            loop {
                                if let Some(mut event) = s.queue.pop_front() {
                                    if let HistoryEvent::MessageStart { response }
                                    | HistoryEvent::Completed { response } = &mut event
                                    {
                                        response.set_per_turn_effort(s.per_turn_effort.as_deref());
                                    }
                                    // Emit succeed telemetry on the terminal event
                                    // (MessageStop or Completed) — once, guarded by `done`.
                                    let is_terminal = matches!(
                                        event,
                                        HistoryEvent::MessageStop | HistoryEvent::Completed { .. }
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
                                if let Some(error) = s.pending_service_error.take() {
                                    return Some((Err(error), s));
                                }
                                if let Some(error) = s.decoder.take_error() {
                                    let error = s.attempt.finish().await.err().unwrap_or(error);
                                    s.finished = true;
                                    if !s.done {
                                        s.done = true;
                                        telemetry::emit_failed(
                                            &s.analytics,
                                            &s.model,
                                            &s.request_id,
                                            ApiService::error_kind(&error),
                                            ApiService::status_of(&error),
                                        )
                                        .await;
                                    }
                                    return Some((Err(error), s));
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
                                let seed_has_usage = s.server_fallback_lane.is_none()
                                    && s.seed.as_ref().is_some_and(|seed| matches!(seed, Ok(Some(batch)) if batch.usage.usage.is_some()));
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
                                        s.safety.batch(&frame);
                                        let terminal_frame = frame.events.iter().any(|event| {
                                            matches!(event, Ok(lingxi_llm_client::protocol::StreamEvent::End { .. }))
                                        });
                                        let terminal_fallback_boundary = frame.events.iter().any(|event| {
                                            let Ok(lingxi_llm_client::protocol::StreamEvent::NativeControl {
                                                protocol: lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages,
                                                control,
                                            }) = event else {
                                                return false;
                                            };
                                            matches!(
                                                control.decode::<lingxi_llm_client::providers::anthropic::fallback_response::FallbackControl>(),
                                                Ok(lingxi_llm_client::providers::anthropic::fallback_response::FallbackControl::Boundary {
                                                    stop_reason: Some(_),
                                                    ..
                                                })
                                            )
                                        });
                                        let terminal_quote_frame =
                                            terminal_frame || terminal_fallback_boundary;
                                        let iteration_array_cleared = frame.events.iter().any(|event| {
                                            let Ok(lingxi_llm_client::protocol::StreamEvent::NativeControl {
                                                protocol: lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages,
                                                control,
                                            }) = event else {
                                                return false;
                                            };
                                            let Ok(lingxi_llm_client::providers::anthropic::fallback_response::FallbackControl::Boundary {
                                                iterations,
                                                iterations_present: true,
                                                ..
                                            }) = control.decode::<lingxi_llm_client::providers::anthropic::fallback_response::FallbackControl>() else {
                                                return false;
                                            };
                                            iterations.served_fallback_model.is_none()
                                        });
                                        if iteration_array_cleared
                                            && s.server_fallback_quote_candidate_seen
                                        {
                                            // Keep the provider-facing usage snapshot clean from
                                            // this Boundary onward; the stop-reason-bearing
                                            // Boundary decides which quote branch won.
                                            s.decoder.clear_server_fallback_cost_quote();
                                            s.server_fallback_quote_metadata = None;
                                        }
                                        let mut quote_observation = None;
                                        let quote = if let Some(lane) =
                                            s.server_fallback_lane.as_ref()
                                        {
                                            let fallback = s.frames.anthropic_fallback();
                                            let native_branch = fallback
                                                .and_then(|facts| facts.iterations.as_ref())
                                                .is_some_and(|iterations| {
                                                    iterations.served_fallback_model.is_some()
                                                });
                                            if terminal_quote_frame
                                                && !s.server_fallback_quote_finalized
                                                && native_branch
                                            {
                                                // A typed terminal Boundary owns the final stop
                                                // reason and iteration array. Do not let an
                                                // earlier provisional observation lock the quote.
                                                if let Some(ServerFallbackQuoteProjection {
                                                    estimate,
                                                    metadata,
                                                    summary_model,
                                                }) = frozen_server_fallback_quote(
                                                    s.pricing.as_ref(),
                                                    &s.pricing_model,
                                                    &lane.model,
                                                    fallback,
                                                    &frame.inference,
                                                ) {
                                                    s.server_fallback_quote_finalized = true;
                                                    s.server_fallback_quote_estimate =
                                                        estimate.clone();
                                                    quote_observation =
                                                        Some(HistoryEvent::CostQuoteObserved {
                                                            estimate: estimate.clone(),
                                                            native_server_fallback: true,
                                                            summary_model,
                                                        });
                                                    s.decoder.observe_server_fallback_cost_quote(
                                                        metadata.clone(),
                                                    );
                                                    s.server_fallback_quote_metadata =
                                                        Some(metadata);
                                                    estimate
                                                } else {
                                                    None
                                                }
                                            } else if terminal_frame
                                                && !s.server_fallback_quote_finalized
                                                && s.server_fallback_quote_candidate_seen
                                            {
                                                // An explicit final `iterations: []` revokes
                                                // the earlier cNe branch. Clear its reserved
                                                // marker and use the ordinary aggregate quote.
                                                let estimate = frozen_stream_quote(
                                                    s.pricing.as_ref(),
                                                    &s.pricing_model,
                                                    &frame.usage,
                                                    &frame.inference,
                                                );
                                                s.server_fallback_quote_finalized = true;
                                                s.server_fallback_quote_estimate = estimate.clone();
                                                s.server_fallback_quote_metadata = None;
                                                s.decoder.clear_server_fallback_cost_quote();
                                                quote_observation =
                                                    Some(HistoryEvent::CostQuoteObserved {
                                                        estimate: estimate.clone(),
                                                        native_server_fallback: false,
                                                        summary_model: None,
                                                    });
                                                estimate
                                            } else if terminal_quote_frame
                                                && s.server_fallback_quote_finalized
                                            {
                                                s.server_fallback_quote_estimate.clone()
                                            } else if !terminal_quote_frame && !native_branch {
                                                // Keep an earlier candidate until terminal facts
                                                // confirm or revoke it. A partial usage boundary
                                                // can omit the iteration array entirely.
                                                None
                                            } else if !terminal_quote_frame && native_branch {
                                                if let Some(ServerFallbackQuoteProjection {
                                                    estimate: _,
                                                    metadata,
                                                    summary_model,
                                                }) = server_fallback_quote_candidate(
                                                    &lane.model,
                                                    fallback,
                                                ) {
                                                    s.server_fallback_quote_candidate_seen = true;
                                                    if s.server_fallback_quote_metadata.as_ref()
                                                        != Some(&metadata)
                                                    {
                                                        quote_observation =
                                                            Some(HistoryEvent::CostQuoteObserved {
                                                                estimate: None,
                                                                native_server_fallback: true,
                                                                summary_model,
                                                            });
                                                        s.decoder
                                                            .observe_server_fallback_cost_quote(
                                                                metadata.clone(),
                                                            );
                                                        s.server_fallback_quote_metadata =
                                                            Some(metadata);
                                                    }
                                                }
                                                None
                                            } else {
                                                frozen_stream_quote(
                                                    s.pricing.as_ref(),
                                                    &s.pricing_model,
                                                    &frame.usage,
                                                    &frame.inference,
                                                )
                                            }
                                        } else {
                                            frozen_stream_quote(
                                                s.pricing.as_ref(),
                                                &s.pricing_model,
                                                &frame.usage,
                                                &frame.inference,
                                            )
                                        };
                                        s.decoder.observe_model_metadata(&s.frames);
                                        match s.decoder.project_batch(frame) {
                                            Ok(mut events) => {
                                                attach_frozen_stream_quote(
                                                    &mut events,
                                                    quote.as_ref(),
                                                );
                                                if let Some(observation) = quote_observation {
                                                    // The physical response may still carry a
                                                    // billable quote if the host session later
                                                    // declines its controller-facing hop.
                                                    let terminal = events.iter().position(|event| {
                                                        matches!(
                                                            event,
                                                            HistoryEvent::ServerFallback { .. }
                                                                | HistoryEvent::MessageDelta { .. }
                                                                | HistoryEvent::MessageStop
                                                                | HistoryEvent::Completed { .. }
                                                        )
                                                    });
                                                    events.insert(
                                                        terminal.unwrap_or(events.len()),
                                                        observation,
                                                    );
                                                }
                                                if let Some((mut usage, completeness)) =
                                                    s.decoder.observed_usage()
                                                {
                                                    usage.cost_estimate = quote.clone();
                                                    if let Some(metadata) =
                                                        &s.server_fallback_quote_metadata
                                                    {
                                                        crate::history_projection::attach_server_fallback_cost_quote(
                                                            &mut usage.provider_metadata,
                                                            metadata.clone(),
                                                        );
                                                    }
                                                    if !seed_has_usage {
                                                        s.attempt.observe(&usage, completeness);
                                                    }
                                                } else {
                                                    s.attempt.observe_events(&events);
                                                }
                                                s.queue.extend(events);
                                            }
                                            Err(e) => {
                                                s.safety.error(&e);
                                                if let Some((mut usage, completeness)) =
                                                    s.decoder.observed_usage()
                                                {
                                                    usage.cost_estimate = quote.clone();
                                                    if let Some(metadata) =
                                                        &s.server_fallback_quote_metadata
                                                    {
                                                        crate::history_projection::attach_server_fallback_cost_quote(
                                                            &mut usage.provider_metadata,
                                                            metadata.clone(),
                                                        );
                                                    }
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
                                                if let Some(observation) = quote_observation {
                                                    s.pending_service_error = Some(e);
                                                    return Some((Ok(observation), s));
                                                }
                                                return Some((Err(e), s));
                                            }
                                        }
                                    }
                                    Ok(None) => {
                                        s.finished = true;
                                        // EOF may follow an End frame that already
                                        // selected native per-iteration accounting.
                                        // Preserve that quote (including None for an
                                        // incomplete native quote) instead of replacing
                                        // it with the dispatched-model aggregate price.
                                        let quote = if s.server_fallback_quote_finalized
                                            || s.server_fallback_quote_candidate_seen
                                            || s.server_fallback_quote_metadata.is_some()
                                        {
                                            s.server_fallback_quote_estimate.clone()
                                        } else {
                                            frozen_stream_quote(
                                                s.pricing.as_ref(),
                                                &s.pricing_model,
                                                &s.frames.usage_report(),
                                                &s.frames.inference_report(),
                                            )
                                        };
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
                                                    if let Some(metadata) =
                                                        &s.server_fallback_quote_metadata
                                                    {
                                                        crate::history_projection::attach_server_fallback_cost_quote(
                                                            &mut usage.provider_metadata,
                                                            metadata.clone(),
                                                        );
                                                    }
                                                    s.attempt.observe(&usage, completeness);
                                                } else {
                                                    s.attempt.observe_events(&events);
                                                }
                                                s.queue.extend(events);
                                            }
                                            Err(e) => {
                                                s.safety.error(&e);
                                                if let Some((mut usage, completeness)) =
                                                    s.decoder.observed_usage()
                                                {
                                                    usage.cost_estimate = quote.clone();
                                                    if let Some(metadata) =
                                                        &s.server_fallback_quote_metadata
                                                    {
                                                        crate::history_projection::attach_server_fallback_cost_quote(
                                                            &mut usage.provider_metadata,
                                                            metadata.clone(),
                                                        );
                                                    }
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
                                        s.safety.error(&e);
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
        tools: Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
        effort: Option<serde_json::Value>,
        speed: Option<String>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        let system = custom_system_prompt(system);
        let mut req = self.build_main_request(
            model,
            profile,
            system.as_ref(),
            messages,
            tools,
            true,
            None,
            false,
            PromptCacheQuerySource::Unspecified,
        )?;
        req.set_effort(effort)?;
        // (fast mode) `Some("fast")` from the main loop lights the fast-mode
        // beta via `beta_context`; `None` keeps the body byte-identical.
        req.set_speed(speed)?;
        self.drive_stream(req).await
    }

    /// Stream the current host-owned Native source vector without flattening
    /// marker elements or section boundaries in the orchestrator.
    /// The sanitized Native query source selects the SDK's main/subagent TTL
    /// environment branch before provider serialization.
    pub async fn stream_with_system_prompt(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        messages: Vec<ConversationMessage>,
        tools: Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
        effort: Option<serde_json::Value>,
        speed: Option<String>,
        skip_global_cache_for_system_prompt: bool,
        query_source: Option<&str>,
        request_dispatch_admission: Option<crate::RequestDispatchAdmission>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        let mut req = self.build_main_request(
            model,
            profile,
            system,
            messages,
            tools,
            true,
            None,
            skip_global_cache_for_system_prompt,
            prompt_cache_query_source(query_source),
        )?;
        req.set_effort(effort)?;
        req.set_speed(speed)?;
        req.execution
            .set_request_dispatch_admission(request_dispatch_admission);
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
        tools: Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
        forced_tool: Option<&str>,
        effort: Option<serde_json::Value>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        let system = custom_system_prompt(system);
        let mut req = self.build_request(
            model,
            profile,
            system.as_ref(),
            messages,
            tools,
            true,
            None,
            false,
            PromptCacheQuerySource::Subagent,
        )?;
        req.set_effort(effort)?;
        if let Some(name) = forced_tool {
            req.set_tool_choice(Some(crate::ToolChoice::Tool {
                name: name.to_string(),
            }));
        }
        self.drive_stream(req).await
    }

    /// Like [`Self::stream`], but ALSO threads a per-turn output-token ceiling
    /// and a COGS query-source label onto request execution metadata (the agent
    /// crate's `SubagentApiCallOpts` seam — Fusion panels and other opts-aware
    /// subagent callers). The cache policy receives the typed subagent role,
    /// never the COGS label. `max_tokens: None` and `query_source_label: None` keep the
    /// body byte-identical to [`Self::stream`] (auto-computed ceiling, no
    /// label).
    #[allow(clippy::too_many_arguments)]
    pub async fn stream_with_opts(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
        effort: Option<serde_json::Value>,
        max_tokens: Option<u32>,
        query_source_label: Option<&str>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        let system = custom_system_prompt(system);
        let mut req = self.build_request(
            model,
            profile,
            system.as_ref(),
            messages,
            tools,
            true,
            max_tokens,
            false,
            PromptCacheQuerySource::Subagent,
        )?;
        req.set_effort(effort)?;
        req.execution.query_source = query_source_label.map(str::to_string);
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
        tools: Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
        forced_tool: Option<&str>,
        effort: Option<serde_json::Value>,
        max_tokens: Option<u32>,
        query_source_label: Option<&str>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        let system = custom_system_prompt(system);
        let mut req = self.build_request(
            model,
            profile,
            system.as_ref(),
            messages,
            tools,
            true,
            max_tokens,
            false,
            PromptCacheQuerySource::Subagent,
        )?;
        req.set_effort(effort)?;
        if let Some(name) = forced_tool {
            req.set_tool_choice(Some(crate::ToolChoice::Tool {
                name: name.to_string(),
            }));
        }
        req.execution.query_source = query_source_label.map(str::to_string);
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
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        let system = custom_system_prompt(system);
        let mut req = self.build_request(
            model,
            profile,
            system.as_ref(),
            messages,
            Vec::new(),
            true,
            max_tokens,
            false,
            PromptCacheQuerySource::Unspecified,
        )?;
        // JSON-schema side queries are independent structured-output requests,
        // not forced-tool calls. A parent `--json-schema` turn may have set a
        // session-level `forced_tool_choice`; do not send that choice with the
        // empty tool list used by this request.
        req.input.tool_choice = lingxi_llm_client::protocol::ToolChoice::Auto;
        req.execution.anthropic_request_kind = lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery;
        req.set_effort(effort)?;
        req.input.output_format = lingxi_llm_client::protocol::OutputFormat::JsonSchema {
            name: "response".into(),
            schema,
            strict: true,
        };
        self.drive_stream(req).await
    }

    /// [`Self::stream_json_schema`] with explicit thinking and temperature
    /// semantics (F003 round 2), mirroring
    /// [`Self::messages_create_side_query_stream_with_thinking`].
    ///
    /// Plain `stream_json_schema` never touches `req.reasoning` or
    /// `req.input.temperature` at all — it just inherits whatever `build_request`
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
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
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
/// (cc 2.1.219) `Vtp` — the dispatch-routing opt-in header value.
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
fn strip_excess_media(msgs: Vec<ConversationMessage>, limit: usize) -> Vec<ConversationMessage> {
    strip_excess_media_with_sources(ConversationMessagesWithSources::new(msgs), limit).messages
}

fn strip_excess_media_with_sources(
    mut msgs: ConversationMessagesWithSources,
    limit: usize,
) -> ConversationMessagesWithSources {
    let total = count_media(&msgs.messages);
    if total <= limit {
        return msgs;
    }
    let mut to_remove = total - limit;
    for (m, block_sources) in msgs.messages.iter_mut().zip(&mut msgs.block_sources) {
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
        let mut retained_content = Vec::with_capacity(content.len());
        let mut retained_sources = Vec::with_capacity(block_sources.len());
        for (block, sources) in content.drain(..).zip(block_sources.drain(..)) {
            if to_remove > 0
                && (matches!(&block, ContentBlock::Image { .. })
                    || matches!(&block, ContentBlock::Document { .. }))
            {
                to_remove -= 1;
            } else {
                retained_content.push(block);
                retained_sources.push(sources);
            }
        }
        *content = retained_content;
        *block_sources = retained_sources;
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
                citations: None,
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

/// Generate a local Host telemetry ID; provider request IDs belong to the SDK.
#[must_use]
fn new_telemetry_id() -> String {
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

#[cfg(test)]
#[path = "service_hosted_retry_test.rs"]
mod service_hosted_retry_test;

#[cfg(test)]
#[path = "buffered_stream_test.rs"]
mod buffered_stream_test;
