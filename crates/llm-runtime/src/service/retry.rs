//! Host replay, failover and backoff policy.

use crate::model::retry::{DriveStep, RetryControl, RetryState};
use crate::model::retry_scope::ModelCallRetryScope;
use crate::service::ApiService;
use crate::{LlmError, LlmRequest};
use std::sync::Arc;
use std::time::Duration;

/// Collapse a [`ReasoningConfig`] to a numeric budget for telemetry labels.
/// `Adaptive` → 0, `Enabled{b}` → b.
pub(super) fn reasoning_budget(
    thinking: Option<&lingxi_llm_client::protocol::ThinkingConfig>,
) -> u32 {
    match thinking.and_then(|thinking| thinking.budget) {
        Some(lingxi_llm_client::protocol::ThinkingBudget::Tokens(tokens)) => tokens,
        _ => 0,
    }
}

/// Sibling connections belong to the first successful route selection.
pub(super) struct ConnectionRetry {
    hops: Option<(
        std::vec::IntoIter<crate::ConnectionHop>,
        crate::FailoverTriggers,
    )>,
}

impl ConnectionRetry {
    pub(super) fn new() -> Self {
        Self { hops: None }
    }

    pub(super) fn capture(&mut self, route: &crate::ResolvedRoute, enabled: bool) {
        if enabled && self.hops.is_none() {
            self.hops = Some((route.connection_chain.clone().into_iter(), route.failover));
        }
    }

    /// Advance to another physical endpoint for the same model. The logical
    /// retry budget carries on and the next endpoint starts its own overload count.
    pub(super) fn advance(
        &mut self,
        request: &mut LlmRequest,
        state: &mut RetryState,
        error: &LlmError,
    ) -> Option<String> {
        let (hops, triggers) = self.hops.as_mut()?;
        if !triggers.matches(error) {
            return None;
        }
        let hop = hops.next()?;
        request.profile = Some(hop.profile_name.clone());
        request.input.model = hop.request_model;
        state.consecutive_overloaded = 0;
        Some(hop.profile_name)
    }
}

/// Allow one overflow-driven output reduction that strictly lowers the
/// previously admitted output ceiling.
pub(super) fn guard_max_tokens_adjustment(
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
pub(super) fn allows_automatic_replay(request: &LlmRequest) -> bool {
    if request.execution.computer_submission.is_some()
        || request.execution.expected_computer_binding.is_some()
        || request.execution.input_protocol == Some(crate::ProtocolFamily::GeminiInteractions)
    {
        return false;
    }
    lingxi_llm_client::execution_safety::request_replay_safety(&request.input)
        == lingxi_llm_client::execution_safety::RequestReplaySafety::Stateless
}

pub(super) fn allows_account_switch(request: &LlmRequest) -> bool {
    allows_automatic_replay(request)
        && request.input.continuation.is_none()
        && request.execution.file_account_scope.is_none()
        && !request.input.messages.iter().any(|message| {
            message.content.iter().any(|block| {
                matches!(
                    block,
                    lingxi_llm_client::protocol::ContentBlock::Thinking { .. }
                        | lingxi_llm_client::protocol::ContentBlock::RedactedThinking { .. }
                        | lingxi_llm_client::protocol::ContentBlock::ProviderContent { .. }
                )
            })
        })
}

/// Whether `model` is an OpenRouter FREE-tier variant (`…:free`), whose 429 is a
/// shared-quota exhaustion that does NOT clear within the retry-backoff window —
/// so it fails fast (surfaces the rate limit immediately) instead of burning the
/// ~160s ladder. PAID models (incl. the user's main provider) and Anthropic keep
/// Claude Code's parity 429-retry. `:free` is OpenRouter's free-variant suffix
/// and is unused by other providers, so it targets exactly the flaky free tier.
pub(super) fn is_free_tier_model(model: &str) -> bool {
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
pub(super) fn rate_limit_cannot_clear(profile: Option<&str>, model: &str) -> bool {
    is_free_tier_model(model) || is_subscription_profile(profile)
}

pub(super) fn openrouter_free_rate_limit_message(
    body: Option<&lingxi_llm_client::protocol::JsonPayload>,
) -> String {
    let detail = body
        .and_then(|body| {
            body.string_at("/error/message")
                .or_else(|| body.string_at("/message"))
        })
        .map(|message| message.trim().to_owned())
        .filter(|message| !message.is_empty())
        .map(|message| message.trim_end_matches(['.', '!', '?']).to_owned());

    match detail {
        Some(detail) => format!(
            "OpenRouter free-model rate limit reached: {detail}. Try another free model or retry later."
        ),
        None => "OpenRouter free-model rate limit reached. Try another free model or retry later."
            .to_string(),
    }
}

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

impl ApiService {
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
                        error.classification_source(),
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
    pub(super) async fn report_and_sleep_retry(
        &self,
        error: &LlmError,
        delay: Duration,
        state: &RetryState,
        ctl: &RetryControl,
    ) {
        self.report_retry(error, delay, state, ctl);
        tokio::time::sleep(delay).await;
    }

    /// Consume an SDK-proven proxy-helper 407 through the ordinary request
    /// retry budget. Returning `Some(false)` means the budget is exhausted;
    /// callers must surface the original wrapped error without failover.
    pub(super) async fn retry_proxy_auth_helper(
        &self,
        error: &LlmError,
        retry_scope: &ModelCallRetryScope,
        state: &mut RetryState,
        ctl: &RetryControl,
        thinking_budget: u32,
    ) -> Option<bool> {
        if !error.is_proxy_auth_helper_retry() {
            return None;
        }
        let DriveStep::RetryAfter(delay) =
            retry_scope.next_step(state, ctl, error, thinking_budget, self.settings_backoff_ms)
        else {
            return Some(false);
        };
        self.report_and_sleep_retry(error, delay, state, ctl).await;
        Some(true)
    }

    // ── 429 retry-after resolution (reset ladder) ─────────────────────────────

    /// Resolve the 429 retry delay using the server-sent reset ladder:
    /// `retry-after` → `anthropic-ratelimit-unified-reset` → `anthropic-ratelimit-requests-reset` → no server hint.
    pub(super) fn resolve_retry_after(
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
    pub(super) fn error_kind(err: &LlmError) -> &'static str {
        match err.classification_source() {
            LlmError::AllAccountsUnavailable { .. } => "all_accounts_unavailable",
            LlmError::AccountStorageUnavailable => "account_storage_unavailable",
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
            LlmError::Transport { .. }
            | LlmError::TransportTimeout { .. }
            | LlmError::ProxyTunnelResponse { .. } => "http",
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
            LlmError::InputBudgetExceeded { .. } => "input_budget_exceeded",
            // "request_too_large" — 2.1.212 error classifier: a 413 whose message
            // lacks "context window" → `"request_too_large"` (distinct from the
            // context-window `"prompt_too_long"` above).
            LlmError::RequestTooLarge => "request_too_large",
            // llm-runtime-only classes — no api-client analogue; use descriptive names.
            LlmError::InvalidRequest { .. } => "invalid_request",
            LlmError::RequestDispatchRejected { .. } => "request_dispatch_rejected",
            LlmError::ApiAttemptsExhausted => "api_request_attempts_exhausted",
            LlmError::QuotaExceeded => "quota_exceeded",
            LlmError::ModelUnavailable => "model_unavailable",
            LlmError::CostUnavailable { .. } => "cost_unavailable",
            LlmError::FileUploadOutcomeUnknown { .. } => "file_upload_outcome_unknown",
            LlmError::MediaDelegationUnavailable { .. }
            | LlmError::MediaDelegationPartial { .. } => "media_delegation_unavailable",
            LlmError::SettingsSource { .. } => "settings_source",
            LlmError::UnsupportedCapability { .. } => "unsupported_capability",
            LlmError::ProviderErrorText { .. } | LlmError::ProxyAuthHelperRetry { .. } => {
                unreachable!("classification_source removes carrier wrappers")
            }
        }
    }

    /// HTTP status code approximation for `emit_failed` (best-effort: only the
    /// variants that carry an HTTP status are non-None).
    pub(super) fn status_of(err: &LlmError) -> Option<u16> {
        match err.classification_source() {
            LlmError::Authentication { .. } | LlmError::PermissionDenied { .. } => {
                err.http_status().or(Some(401))
            }
            LlmError::InvalidRequest { .. } | LlmError::ContextOverflow { .. } => Some(400),
            LlmError::RequestTooLarge => Some(413),
            LlmError::RateLimited { .. } | LlmError::QuotaExceeded => Some(429),
            LlmError::ModelUnavailable => Some(404),
            LlmError::ProviderInternal => Some(500),
            LlmError::ProviderTimeout { .. } => err.http_status(),
            LlmError::Overloaded { .. } => Some(529),
            LlmError::ProxyTunnelResponse { status, .. } => Some(*status),
            // No HTTP status: the request never reached the model API. The
            // refresh call to the IdP failed locally, so inventing a 401 here
            // would put a status in the transcript that no server ever sent.
            LlmError::AllAccountsUnavailable { .. }
            | LlmError::AccountStorageUnavailable
            | LlmError::OAuthRefreshDead
            | LlmError::InputBudgetExceeded { .. }
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
            | LlmError::SettingsSource { .. }
            | LlmError::RequestDispatchRejected { .. }
            | LlmError::ApiAttemptsExhausted => None,
            LlmError::ProviderErrorText { .. } | LlmError::ProxyAuthHelperRetry { .. } => {
                unreachable!("classification_source removes carrier wrappers")
            }
        }
    }
}
