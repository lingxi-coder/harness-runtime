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

/// Route-local normalization must not change the next route's logical controls.
pub(super) struct LogicalReasoning {
    thinking: Option<lingxi_llm_client::protocol::ThinkingConfig>,
    temperature: Option<f32>,
    automatic: bool,
    native_budget: Option<lingxi_llm_client::client::NativeLsnManualBudget>,
    native_request_type_disabled: Option<bool>,
    native_mechanically_disabled: Option<bool>,
    context:
        Option<lingxi_llm_client::providers::anthropic::request_policy::AnthropicContextManagement>,
}

impl LogicalReasoning {
    pub(super) fn capture(request: &LlmRequest) -> Self {
        Self {
            thinking: request.input.thinking.clone(),
            temperature: request.input.temperature,
            automatic: request.execution.automatic_thinking_budget,
            native_budget: request.execution.native_lsn_manual_budget,
            native_request_type_disabled: request.execution.native_thinking_request_type_disabled,
            native_mechanically_disabled: request.execution.native_thinking_mechanically_disabled,
            context: request.execution.anthropic_context_management.clone(),
        }
    }

    pub(super) fn restore(&self, request: &mut LlmRequest) {
        request.input.thinking.clone_from(&self.thinking);
        request.input.temperature = self.temperature;
        request.execution.automatic_thinking_budget = self.automatic;
        request.execution.native_lsn_manual_budget = self.native_budget;
        request.execution.native_thinking_request_type_disabled = self.native_request_type_disabled;
        request.execution.native_thinking_mechanically_disabled = self.native_mechanically_disabled;
        request
            .execution
            .anthropic_context_management
            .clone_from(&self.context);
    }
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
        retry_scope: &ModelCallRetryScope,
        error: &LlmError,
    ) -> Option<String> {
        let (hops, triggers) = self.hops.as_mut()?;
        if !triggers.matches(error) {
            return None;
        }
        let hop = hops.next()?;
        request.profile = Some(hop.profile_name.clone());
        request.input.model = hop.request_model;
        request.input.model_source = None;
        state.consecutive_overloaded = 0;
        retry_scope.reset_model_overloads();
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
    use lingxi_llm_client::execution_safety::{request_replay_safety, RequestReplaySafety};
    if request_replay_safety(&request.input) == RequestReplaySafety::Stateless {
        return true;
    }
    // Older SDK revisions still used by the runtime treated every native
    // message option as stateful. Ignore only the typed host effort marker.
    if !request.input.messages.iter().any(is_per_turn_effort_marker) {
        return false;
    }
    let mut input = request.input.clone();
    input
        .messages
        .retain(|message| !is_per_turn_effort_marker(message));
    request_replay_safety(&input) == RequestReplaySafety::Stateless
}

pub(crate) fn is_per_turn_effort_marker(
    message: &lingxi_llm_client::protocol::ConversationMessage,
) -> bool {
    message.role == lingxi_llm_client::protocol::MessageRole::System
        && message.content.is_empty()
        && message.native_options.len() == 1
        && message
            .anthropic_options()
            .is_some_and(|options| options.effort.is_some() && options.clear_at.is_none())
}

/// Inspect the sealed SDK body as well as the canonical input. Profile and
/// per-request body overrides can introduce provider work after SDK validation.
#[derive(Default, serde::Deserialize)]
#[serde(default)]
struct WireReplayControls {
    tools: Vec<lingxi_llm_client::protocol::JsonPayload>,
    mcp_servers: Vec<serde::de::IgnoredAny>,
    plugins: Vec<serde::de::IgnoredAny>,
    web_search_options: Option<serde::de::IgnoredAny>,
    search_parameters: Option<serde::de::IgnoredAny>,
    #[serde(rename = "cachedContent", alias = "cached_content")]
    cached_content: Option<serde::de::IgnoredAny>,
    container: Option<serde::de::IgnoredAny>,
    web_search: bool,
    enable_search: bool,
    background: bool,
    #[serde(deserialize_with = "present_wire_field")]
    thread: Option<lingxi_llm_client::protocol::JsonPayload>,
    #[serde(deserialize_with = "present_wire_field")]
    previous_interaction_id: Option<serde::de::IgnoredAny>,
    #[serde(deserialize_with = "present_wire_field")]
    previous_response_id: Option<serde::de::IgnoredAny>,
    #[serde(deserialize_with = "present_wire_field")]
    conversation: Option<serde::de::IgnoredAny>,
}

// An explicitly supplied reference, including null, follows the same policy
// as its original source-backed field-presence check.
fn present_wire_field<'de, T: serde::Deserialize<'de>, D: serde::Deserializer<'de>>(
    de: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(de).map(Some)
}

impl WireReplayControls {
    fn has_provider_work(&self, request: &LlmRequest, protocol: crate::ProtocolFamily) -> bool {
        self.enable_search
            || self.web_search
            || self.background
            || self.web_search_options.is_some()
            || self.search_parameters.is_some()
            || (matches!(
                protocol,
                crate::ProtocolFamily::GeminiGenerateContent | crate::ProtocolFamily::VertexGemini
            ) && self.cached_content.is_some())
            || self.container.is_some()
            || !self.mcp_servers.is_empty()
            || !self.plugins.is_empty()
            || self.tools.iter().any(|tool| {
                if matches!(
                    protocol,
                    crate::ProtocolFamily::GeminiGenerateContent
                        | crate::ProtocolFamily::VertexGemini
                ) {
                    // A Tool may combine functions with grounding, code execution or
                    // other hosted facilities. Only a pure declaration is local.
                    return tool.object_len("") != Some(1)
                        || (tool.array_len("/functionDeclarations").is_none()
                            && tool.array_len("/function_declarations").is_none());
                }
                let kind = tool.string_at("/type");
                if protocol.is_anthropic_family() {
                    return match kind.as_deref() {
                        None | Some("custom") => tool.string_at("/name").is_none(),
                        Some(kind) => !request
                            .input
                            .anthropic_client_toolsets()
                            .iter()
                            .any(|local| local.wire_type() == kind),
                    };
                }
                match kind.as_deref() {
                    Some("function" | "custom") => tool
                        .string_at("/function/name")
                        .or_else(|| tool.string_at("/name"))
                        .is_none_or(|name| name.starts_with('$')),
                    Some("tool_search") => {
                        tool.string_at("/execution").as_deref() != Some("client")
                    }
                    _ => true,
                }
            })
    }
}

#[cfg(test)]
#[test]
fn gemini_cached_content_and_hosted_tools_require_explicit_replay() {
    let request = LlmRequest::new("model");
    for protocol in [
        crate::ProtocolFamily::GeminiGenerateContent,
        crate::ProtocolFamily::VertexGemini,
    ] {
        for (body, expected) in [
            (r#"{"cachedContent":"cachedContents/prompt"}"#, true),
            (r#"{"cached_content":"cachedContents/prompt"}"#, true),
            (
                r#"{"tools":[{"functionDeclarations":[{"name":"lookup"}]}]}"#,
                false,
            ),
            (
                r#"{"tools":[{"functionDeclarations":[],"googleSearch":{}}]}"#,
                true,
            ),
        ] {
            let wire: WireReplayControls = serde_json::from_str(body).unwrap();
            assert_eq!(
                wire.has_provider_work(&request, protocol),
                expected,
                "{body}"
            );
        }
    }
}

fn prepared_replay_allowed(
    request: &LlmRequest,
    prepared: &crate::PreparedLlmCall,
    recover_continuation: bool,
) -> bool {
    let protocol = prepared.route.protocol;
    if protocol == crate::ProtocolFamily::GeminiInteractions || !allows_automatic_replay(request) {
        return false;
    }
    // Deserialize only the controls. Arbitrary transcripts and tool schemas
    // retain their exact JSON source and need no recursive Value conversion.
    let Ok(wire) = serde_json::from_str::<WireReplayControls>(
        prepared.provider_request.body_json.as_json_source(),
    ) else {
        return false;
    };
    if wire.has_provider_work(request, protocol) {
        return false;
    }
    use lingxi_llm_client::ConversationRequestMode;
    let mode = prepared.continuation_snapshot.mode;
    if recover_continuation && mode == ConversationRequestMode::Continue {
        return true;
    }
    // The SDK's fresh Anthropic thread is an ephemeral cache optimization.
    let implicit_fresh_thread = mode == ConversationRequestMode::Create
        && wire
            .thread
            .as_ref()
            .is_some_and(|thread| thread.string_at("/type").as_deref() == Some("create"));
    (wire.thread.is_none() || implicit_fresh_thread)
        && wire.previous_interaction_id.is_none()
        && wire.previous_response_id.is_none()
        && wire.conversation.is_none()
}

pub(super) fn allows_route_replay(request: &LlmRequest, prepared: &crate::PreparedLlmCall) -> bool {
    prepared_replay_allowed(request, prepared, false)
}

/// A definitive rejection can recover an implicit SDK continuation on the same
/// route. Provider-executed tools still require their own recovery policy.
pub(super) fn allows_continuation_recovery(
    request: &LlmRequest,
    prepared: &crate::PreparedLlmCall,
) -> bool {
    prepared_replay_allowed(request, prepared, true)
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

/// Whether a 429 on this route is known not to clear inside the retry-backoff
/// window. Use the selected credential and wire model rather than aliases.
pub(super) fn rate_limit_cannot_clear(auth: crate::AuthStrategy, wire_model: &str) -> bool {
    is_free_tier_model(wire_model)
        || matches!(
            auth,
            crate::AuthStrategy::ChatGptPlan | crate::AuthStrategy::ChatGptOAuth
        )
}

pub(super) fn bind_rate_limit_retry_policy(
    state: &mut crate::model::retry::RetryState,
    subscriber: super::SubscriberState,
    prepared: &crate::PreparedLlmCall,
) {
    state.is_subscriber = prepared.uses_session_subscription && subscriber.is_subscriber;
    state.is_enterprise = state.is_subscriber && subscriber.is_enterprise;
    state.rate_limit_terminal = rate_limit_cannot_clear(
        prepared.request_auth,
        &prepared.route.resolved_route.request_model,
    );
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
            LlmError::ResponseBodyLimitExceeded { .. } => "response_body_limit_exceeded",
            LlmError::ResponseBodyInterrupted { .. } => "response_body_interrupted",
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
            LlmError::ProviderErrorText { .. }
            | LlmError::ProxyAuthHelperRetry { .. }
            | LlmError::ResponseDiagnostics { .. } => {
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
            LlmError::ProviderTimeout { .. }
            | LlmError::ResponseBodyInterrupted { .. }
            | LlmError::ResponseBodyLimitExceeded { .. } => err.http_status(),
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
            LlmError::ProviderErrorText { .. }
            | LlmError::ProxyAuthHelperRetry { .. }
            | LlmError::ResponseDiagnostics { .. } => {
                unreachable!("classification_source removes carrier wrappers")
            }
        }
    }
}
