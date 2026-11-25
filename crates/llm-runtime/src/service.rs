//! Host service for routing and executing model requests.

#[cfg(test)]
use crate::dispatch_header::{DispatchHeaderState, DISPATCH_ID_HEADER};
use crate::model::rate_limit::{RateLimitInfo, RawUtilization};
#[cfg(test)]
use crate::model::retry::{resolve_retry_control_with_settings, ResolveRetryEnv, RetryControl};
#[cfg(test)]
use crate::model::retry_scope::ModelCallRetryScope;
use crate::model::user_agent::UserAgentEnv;
use crate::{CostEstimator, ModelRuntime, Transport};
#[cfg(test)]
use crate::{HistoryEvent, LlmError, LlmRequest, PromptCacheQuerySource};
#[cfg(test)]
use lingxi_core::types::{ContentBlock, ConversationMessage};
#[cfg(test)]
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

mod accounts;
mod anthropic;
mod attempt;
mod capabilities;
mod complete;
mod configuration;
mod conversation;
mod count;
mod hosted_search;
mod request;
mod response;
mod retry;
mod side_query;
mod stream;
mod usage;

#[path = "service_native_media.rs"]
mod native_media;

use accounts::ApiServiceAccountObserver;
#[cfg(test)]
use anthropic::endpoint::{native_context_1m_model_key, native_custom_beta_provider_route};
#[cfg(test)]
use anthropic::foundry::native_foundry_model_key;
use anthropic::foundry::FoundryDeploymentKey;
#[cfg(test)]
use anthropic::foundry::{
    NATIVE_DEFERRED_TOOL_PLACEHOLDER_DESCRIPTION, NATIVE_DEFERRED_TOOL_PLACEHOLDER_NAME,
    NATIVE_TOOL_SEARCH_DESCRIPTION, NATIVE_TOOL_SEARCH_NAME,
};
#[cfg(test)]
use anthropic::prompt::apply_anthropic_tool_result_reminder_suffixes;
pub(crate) use anthropic::prompt::extra_body_object;
use anthropic::prompt::CacheEditingInputs;
#[cfg(test)]
use anthropic::prompt::PinnedCacheEdits;
use anthropic::thinking::{NativeThinkingResumptionRefusalLatch, NativeThinkingTypeKey};
pub use attempt::RequestSessionIdSource;
#[cfg(test)]
use complete::NonStreamDrivePolicy;
use configuration::{EffortSettingsSource, PromptCacheTtlSettingsSource};
use conversation::ConversationSlot;
#[cfg(test)]
use lingxi_llm_client::providers::anthropic::response_policy::strip_assistant_signature_blocks as strip_signature_blocks;
#[cfg(test)]
use request::custom_system_prompt;
#[cfg(test)]
use request::{count_media, strip_excess_media, MAX_MEDIA_PER_REQUEST};
pub use request::{
    native_custom_system_prompt, FallbackPolicy, MessagesCreateOptions, MessagesCreateRequest,
    NonStreamingRequestClass, NonStreamingRetryOptions,
};
use response::PromptCacheOverageState;
pub use response::{RequestIdOrigin, SubscriberState};
#[cfg(test)]
use retry::allows_automatic_replay;
pub use retry::{RetryInfo, RetryReporter};
#[cfg(test)]
use usage::attach_frozen_stream_quote;

tokio::task_local! {
    static ACCOUNT_CALL_CLIENT: (usize,Arc<ModelRuntime>);
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

/// Execute model calls with host routing, admission, retries and accounting.
pub struct ApiService {
    anthropic: anthropic::State,
    /// Provider normalization policy owned by this session service.
    tool_search_enabled: std::sync::atomic::AtomicBool,
    account_routing_reporter: RwLock<Option<Arc<dyn crate::accounts::AccountRoutingReporter>>>,
    scope_output_quota: Option<Arc<crate::ScopeOutputQuota>>,
    account_change_observer: Arc<ApiServiceAccountObserver>,
    account_change_observer_keepalive: Vec<Arc<dyn lingxi_core::host::auth::AccountChangeObserver>>,
    effort_settings_source: Option<EffortSettingsSource>,
    inherited_effort_settings: Option<Vec<lingxi_core::host::effort::EffortSettingsLayer>>,
    effort_table_options: lingxi_core::host::effort_table::TableOptions,
    session_effort: RwLock<lingxi_core::host::effort_table::SessionEffort>,
    model_attempt_hooks: RwLock<Option<Arc<dyn crate::ModelAttemptHooks>>>,
    request_session_id_source: RwLock<Option<RequestSessionIdSource>>,
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
    thinking: RwLock<crate::model::thinking_state::ThinkingState>,
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
    /// the raw snapshot for the attempt and promotes it into this
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
    /// One-shot subagent wrap-up hint, armed from a near-limit 2xx snapshot and
    /// consumed by the query loop exactly once for that five-hour window.
    pending_near_limit_wrap_up_hint: Mutex<bool>,
    /// Five-hour reset epoch that already armed or consumed the near-limit
    /// wrap-up hint. This is the per-window dedupe key: the same reset must not
    /// re-arm after consumption; a later reset opens a new window.
    near_limit_wrap_up_window_key: Mutex<Option<u64>>,
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
    /// Main-conversation continuation and transport state. Checked out without
    /// holding a lock across preparation, admission, dispatch or streaming.
    responses_ws_session: Arc<Mutex<ConversationSlot>>,
    /// Bounded projection cache for this conversation; sidequeries bypass it.
    history_projection_cache: Mutex<crate::convert::input_projection::MessageProjectionCache>,
    /// Idle connections only. Every checkout starts without conversation state.
    side_sessions: Arc<Mutex<Vec<lingxi_llm_client::ConversationSession>>>,
}

#[cfg(test)]
#[path = "service_test.rs"]
mod service_test;

#[cfg(test)]
#[path = "service_live_web_tests.rs"]
mod live_web_tests;

#[cfg(test)]
#[path = "service_read_token_count_test.rs"]
mod read_token_count_test;

#[cfg(test)]
#[path = "service_hosted_retry_test.rs"]
mod service_hosted_retry_test;

#[cfg(test)]
#[path = "buffered_stream_test.rs"]
mod buffered_stream_test;

#[cfg(test)]
#[path = "service_native_thinking_test.rs"]
mod service_native_thinking_test;

#[cfg(test)]
#[path = "service_native_thinking_dispatch_test.rs"]
mod service_native_thinking_dispatch_test;
