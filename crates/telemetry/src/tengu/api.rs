//! `tengu_api_*` event schemas — 25 events emitted by the Anthropic API
//! client middleware (M3-03 owner; M3-06 schema lock).
//!
//! Field set sourced from M3-03 plan (the four core events: started /
//! succeeded / failed / `rate_limited`) and from `claude-code` @ 6a25909's
//! `src/services/api/middleware/*.ts` emitters. All user-derived string
//! fields are typed [`Verified`](crate::Verified); none of these events
//! carries PII-tagged content (request IDs and provider names are safe).

use crate::pii::Verified;
use serde::{Deserialize, Serialize};

// -- Event name constants (byte-locked) ---------------------------------------

/// `tengu_api_request_started` — fired before any HTTP send.
pub const REQUEST_STARTED: &str = "tengu_api_request_started";
/// `tengu_api_request_succeeded` — fired on 2xx response.
pub const REQUEST_SUCCEEDED: &str = "tengu_api_request_succeeded";
/// `tengu_api_request_failed` — fired on 4xx/5xx response after retries.
pub const REQUEST_FAILED: &str = "tengu_api_request_failed";
/// `tengu_api_rate_limited` — fired on 429.
pub const RATE_LIMITED: &str = "tengu_api_rate_limited";
/// `tengu_api_retry_started` — fired before a retry attempt.
pub const RETRY_STARTED: &str = "tengu_api_retry_started";
/// `tengu_api_retry_succeeded` — fired when a retry returned 2xx.
pub const RETRY_SUCCEEDED: &str = "tengu_api_retry_succeeded";
/// `tengu_api_retry_exhausted` — all retry attempts failed.
pub const RETRY_EXHAUSTED: &str = "tengu_api_retry_exhausted";
/// `tengu_api_count_tokens_requested` — `/v1/messages/count_tokens` POST.
pub const COUNT_TOKENS_REQUESTED: &str = "tengu_api_count_tokens_requested";
/// `tengu_api_count_tokens_succeeded` — `count_tokens` 200 response.
pub const COUNT_TOKENS_SUCCEEDED: &str = "tengu_api_count_tokens_succeeded";
/// `tengu_api_count_tokens_failed` — `count_tokens` error.
pub const COUNT_TOKENS_FAILED: &str = "tengu_api_count_tokens_failed";
/// `tengu_api_oauth_refresh_triggered` — middleware decided to refresh.
pub const OAUTH_REFRESH_TRIGGERED: &str = "tengu_api_oauth_refresh_triggered";
/// `tengu_api_oauth_refresh_succeeded` — refresh returned new tokens.
pub const OAUTH_REFRESH_SUCCEEDED: &str = "tengu_api_oauth_refresh_succeeded";
/// `tengu_api_oauth_refresh_failed` — refresh call errored.
pub const OAUTH_REFRESH_FAILED: &str = "tengu_api_oauth_refresh_failed";
/// `tengu_api_oauth_401_reauth_started` — 401 triggered reactive refresh.
pub const OAUTH_401_REAUTH_STARTED: &str = "tengu_api_oauth_401_reauth_started";
/// `tengu_api_oauth_401_reauth_succeeded` — reactive refresh + retry succeeded.
pub const OAUTH_401_REAUTH_SUCCEEDED: &str = "tengu_api_oauth_401_reauth_succeeded";
/// `tengu_api_streaming_started` — first SSE chunk received.
pub const STREAMING_STARTED: &str = "tengu_api_streaming_started";
/// `tengu_api_streaming_chunk_received` — fired per SSE event (sampled).
pub const STREAMING_CHUNK_RECEIVED: &str = "tengu_api_streaming_chunk_received";
/// `tengu_api_streaming_completed` — `message_stop` SSE event.
pub const STREAMING_COMPLETED: &str = "tengu_api_streaming_completed";
/// `tengu_api_streaming_failed` — SSE stream errored mid-response.
pub const STREAMING_FAILED: &str = "tengu_api_streaming_failed";
/// `tengu_api_beta_header_attached` — `anthropic-beta` header set on request.
pub const BETA_HEADER_ATTACHED: &str = "tengu_api_beta_header_attached";
/// `tengu_api_provider_selected` — chose Anthropic / Vertex / Bedrock.
pub const PROVIDER_SELECTED: &str = "tengu_api_provider_selected";
/// `tengu_api_model_resolved` — alias (e.g. `sonnet`) resolved to concrete model.
pub const MODEL_RESOLVED: &str = "tengu_api_model_resolved";
/// `tengu_api_extended_thinking_requested` — request used the extended thinking beta.
pub const EXTENDED_THINKING_REQUESTED: &str = "tengu_api_extended_thinking_requested";
/// `tengu_api_request_cancelled` — caller cancelled mid-flight.
pub const REQUEST_CANCELLED: &str = "tengu_api_request_cancelled";
/// `tengu_api_circuit_breaker_opened` — repeated 5xx tripped the breaker.
pub const CIRCUIT_BREAKER_OPENED: &str = "tengu_api_circuit_breaker_opened";

/// Order-locked array of all 25 names; consumed by `tengu::ALL_EVENT_NAMES`.
pub(crate) const NAMES: &[&str] = &[
    REQUEST_STARTED,
    REQUEST_SUCCEEDED,
    REQUEST_FAILED,
    RATE_LIMITED,
    RETRY_STARTED,
    RETRY_SUCCEEDED,
    RETRY_EXHAUSTED,
    COUNT_TOKENS_REQUESTED,
    COUNT_TOKENS_SUCCEEDED,
    COUNT_TOKENS_FAILED,
    OAUTH_REFRESH_TRIGGERED,
    OAUTH_REFRESH_SUCCEEDED,
    OAUTH_REFRESH_FAILED,
    OAUTH_401_REAUTH_STARTED,
    OAUTH_401_REAUTH_SUCCEEDED,
    STREAMING_STARTED,
    STREAMING_CHUNK_RECEIVED,
    STREAMING_COMPLETED,
    STREAMING_FAILED,
    BETA_HEADER_ATTACHED,
    PROVIDER_SELECTED,
    MODEL_RESOLVED,
    EXTENDED_THINKING_REQUESTED,
    REQUEST_CANCELLED,
    CIRCUIT_BREAKER_OPENED,
];

// -- Payload structs (deny_unknown_fields locked) -----------------------------

/// Payload for [`REQUEST_STARTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestStartedPayload {
    /// Resolved model identifier (e.g. `claude-sonnet-4-5`).
    pub model: Verified,
    /// Provider tag (e.g. `anthropic` / `vertex` / `bedrock`).
    pub provider: Verified,
    /// Endpoint path (e.g. `/v1/messages`).
    pub endpoint: Verified,
    /// Client-side request id used to correlate started/succeeded/failed events.
    pub request_id: Verified,
    /// `true` if the request is in streaming mode.
    pub is_stream: bool,
    /// Optional caller-supplied token count estimate (filled in by M3-03 if
    /// `count_tokens` was called first; `None` otherwise).
    pub input_tokens_estimate: Option<u64>,
}

/// Payload for [`REQUEST_SUCCEEDED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestSucceededPayload {
    /// Resolved model identifier.
    pub model: Verified,
    /// Provider tag.
    pub provider: Verified,
    /// Endpoint path.
    pub endpoint: Verified,
    /// Same id as the matching `REQUEST_STARTED` event.
    pub request_id: Verified,
    /// HTTP status code (2xx).
    pub status: u32,
    /// Wall-clock duration of the request in milliseconds.
    pub duration_ms: u64,
    /// Final input tokens reported by the API response usage field.
    pub input_tokens: u64,
    /// Final output tokens reported by the API response usage field.
    pub output_tokens: u64,
}

/// Payload for [`REQUEST_FAILED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestFailedPayload {
    /// Resolved model identifier.
    pub model: Verified,
    /// Provider tag.
    pub provider: Verified,
    /// Endpoint path.
    pub endpoint: Verified,
    /// Same id as the matching `REQUEST_STARTED` event.
    pub request_id: Verified,
    /// HTTP status code (4xx/5xx; 0 for transport errors).
    pub status: u32,
    /// Wall-clock duration of the request in milliseconds.
    pub duration_ms: u64,
    /// Failure category (terminal classification).
    pub failure_kind: FailureKind,
    /// Server-supplied error message body — PII-tagged (may include user text).
    pub error_body: Option<crate::pii::PiiTagged>,
}

/// Failure classification for [`RequestFailedPayload`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FailureKind {
    /// 400 / 422 — request was malformed.
    BadRequest,
    /// 401 / 403 — auth failed even after refresh attempt.
    Unauthorized,
    /// 429 — rate-limited (also fires [`RATE_LIMITED`] separately).
    RateLimited,
    /// 5xx — server-side error.
    ServerError,
    /// Transport-layer error (DNS, TLS, socket).
    Transport,
    /// Caller cancelled before response.
    Cancelled,
}

/// Payload for [`RATE_LIMITED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitedPayload {
    /// Resolved model identifier.
    pub model: Verified,
    /// Provider tag.
    pub provider: Verified,
    /// Server-supplied retry-after seconds.
    pub retry_after_secs: u64,
    /// Which rate-limit bucket tripped.
    pub bucket: RateLimitBucket,
}

/// Anthropic rate-limit buckets per `anthropic-ratelimit-*` response headers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RateLimitBucket {
    /// Requests-per-minute bucket.
    RequestsPerMinute,
    /// Input-tokens-per-minute bucket.
    InputTokensPerMinute,
    /// Output-tokens-per-minute bucket.
    OutputTokensPerMinute,
    /// Concurrent-requests bucket.
    Concurrent,
}

/// Payload for [`RETRY_STARTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryStartedPayload {
    /// Same id as the original `REQUEST_STARTED` event.
    pub request_id: Verified,
    /// 1-based retry attempt number (1 means "first retry after the initial failure").
    pub attempt: u32,
    /// Computed backoff delay in milliseconds, including jitter.
    pub backoff_ms: u64,
}

/// Payload for [`RETRY_SUCCEEDED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetrySucceededPayload {
    /// Same id as the original `REQUEST_STARTED` event.
    pub request_id: Verified,
    /// Total attempts including the original request (1-based).
    pub total_attempts: u32,
}

/// Payload for [`RETRY_EXHAUSTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryExhaustedPayload {
    /// Same id as the original `REQUEST_STARTED` event.
    pub request_id: Verified,
    /// Total attempts before giving up.
    pub total_attempts: u32,
    /// Final failure classification (matches the terminal `REQUEST_FAILED`).
    pub final_failure_kind: FailureKind,
}

/// Payload for [`COUNT_TOKENS_REQUESTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CountTokensRequestedPayload {
    /// Resolved model identifier.
    pub model: Verified,
    /// Provider tag.
    pub provider: Verified,
}

/// Payload for [`COUNT_TOKENS_SUCCEEDED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CountTokensSucceededPayload {
    /// Resolved model identifier.
    pub model: Verified,
    /// Server-reported input token count.
    pub input_tokens: u64,
}

/// Payload for [`COUNT_TOKENS_FAILED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CountTokensFailedPayload {
    /// Resolved model identifier.
    pub model: Verified,
    /// Failure classification.
    pub failure_kind: FailureKind,
}

/// Payload for [`OAUTH_REFRESH_TRIGGERED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OauthRefreshTriggeredPayload {
    /// Reason the refresh was scheduled.
    pub reason: OauthRefreshReason,
}

/// Why an OAuth refresh was triggered.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OauthRefreshReason {
    /// Proactive refresh window opened (token within `min(remaining/2, 5min)` of expiry).
    Proactive,
    /// Reactive — the API responded 401.
    Reactive,
    /// Caller requested an upgraded scope.
    ScopeUpgrade,
}

/// Payload for [`OAUTH_REFRESH_SUCCEEDED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OauthRefreshSucceededPayload {
    /// Trigger reason.
    pub reason: OauthRefreshReason,
    /// New access-token lifetime in seconds (server-reported).
    pub expires_in_secs: u64,
}

/// Payload for [`OAUTH_REFRESH_FAILED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OauthRefreshFailedPayload {
    /// Trigger reason.
    pub reason: OauthRefreshReason,
    /// Server-supplied error code or transport-layer description.
    pub error: Verified,
}

/// Payload for [`OAUTH_401_REAUTH_STARTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Oauth401ReauthStartedPayload {
    /// Request that received the 401.
    pub request_id: Verified,
}

/// Payload for [`OAUTH_401_REAUTH_SUCCEEDED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Oauth401ReauthSucceededPayload {
    /// Request that completed after reactive refresh.
    pub request_id: Verified,
}

/// Payload for [`STREAMING_STARTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamingStartedPayload {
    /// Resolved model identifier.
    pub model: Verified,
    /// Same id as the matching `REQUEST_STARTED` event.
    pub request_id: Verified,
}

/// Payload for [`STREAMING_CHUNK_RECEIVED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamingChunkReceivedPayload {
    /// Same id as the matching `REQUEST_STARTED` event.
    pub request_id: Verified,
    /// SSE event type (e.g. `content_block_delta`, `message_delta`).
    pub event_type: Verified,
    /// Sampled chunk size in bytes.
    pub size_bytes: u64,
}

/// Payload for [`STREAMING_COMPLETED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamingCompletedPayload {
    /// Same id as the matching `REQUEST_STARTED` event.
    pub request_id: Verified,
    /// Total chunks observed.
    pub total_chunks: u64,
    /// Total stream duration in milliseconds.
    pub duration_ms: u64,
}

/// Payload for [`STREAMING_FAILED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamingFailedPayload {
    /// Same id as the matching `REQUEST_STARTED` event.
    pub request_id: Verified,
    /// Chunks received before failure.
    pub chunks_received: u64,
    /// Failure classification.
    pub failure_kind: FailureKind,
}

/// Payload for [`BETA_HEADER_ATTACHED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BetaHeaderAttachedPayload {
    /// Same id as the matching `REQUEST_STARTED` event.
    pub request_id: Verified,
    /// Comma-separated beta header value (the verbatim `anthropic-beta` body).
    pub beta_value: Verified,
}

/// Payload for [`PROVIDER_SELECTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSelectedPayload {
    /// Provider tag.
    pub provider: Verified,
    /// Why this provider was selected.
    pub reason: ProviderSelectionReason,
}

/// Why a specific provider was selected for a request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProviderSelectionReason {
    /// User-configured default in settings.
    DefaultSetting,
    /// Explicit per-request override (e.g. `--provider vertex`).
    Override,
    /// Fallback when the primary provider failed.
    Fallback,
}

/// Payload for [`MODEL_RESOLVED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelResolvedPayload {
    /// Original alias the caller supplied (e.g. `sonnet`).
    pub alias: Verified,
    /// Concrete model identifier the alias resolved to (e.g. `claude-sonnet-4-5`).
    pub resolved_model: Verified,
}

/// Payload for [`EXTENDED_THINKING_REQUESTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtendedThinkingRequestedPayload {
    /// Same id as the matching `REQUEST_STARTED` event.
    pub request_id: Verified,
    /// Caller-supplied budget tokens for the thinking step.
    pub budget_tokens: u64,
}

/// Payload for [`REQUEST_CANCELLED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestCancelledPayload {
    /// Same id as the matching `REQUEST_STARTED` event.
    pub request_id: Verified,
    /// Cancellation source.
    pub source: CancellationSource,
}

/// Who or what cancelled the request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CancellationSource {
    /// User-initiated (Ctrl-C, UI cancel, killswitch).
    User,
    /// Engine-initiated (compaction triggered, agent halted).
    Engine,
    /// Timeout exceeded.
    Timeout,
}

/// Payload for [`CIRCUIT_BREAKER_OPENED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CircuitBreakerOpenedPayload {
    /// Provider whose breaker tripped.
    pub provider: Verified,
    /// Number of consecutive failures before the trip.
    pub consecutive_failures: u32,
    /// Cool-down window in seconds.
    pub cooldown_secs: u64,
}
