//! Conversation orchestrator.
//!
//! Owns the session and every per-turn service the drivers call. The turn loops
//! themselves live in `conversation/drivers/`. See module-level docs in
//! `lib.rs`.

use crate::config::OrchestratorConfig;
use crate::error::OrchestratorError;
use crate::test_support::{HookExecutor, PermissionGate};
use crate::token_budget::{check_token_budget, BudgetTracker, TokenBudgetDecision};
use crate::turn_loop::{
    execute_one_turn_with_recovery_tracked, RecoveryState, TurnStepOutcome,
    MALFORMED_TOOL_USE_RETRY_FAILED, MALFORMED_TOOL_USE_RETRY_NUDGE,
    MAX_OUTPUT_TOKENS_RECOVERY_LIMIT, MAX_OUTPUT_TOKENS_RECOVERY_NUDGE, THINKING_ONLY_NUDGE,
};
use async_trait::async_trait;
use hooks::events::HookEvent;
use hooks::registry::HookContext;
use lingxi_core::types::{ConversationMessage, HookId, MessageId, SessionId};
use lingxi_core::SessionState;
use llm_runtime::{HistoryEvent, HistoryResponse, LlmError};
use session::JsonlWriter;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::time::Duration;

use lingxi_core::host::orchestrator::ModelListing;
use lingxi_core::host::OutputStream;
/// Re-export of the canonical image-source shape (FROZEN in `protocol`) so callers
/// that do NOT depend on the `protocol` crate — notably the desktop bridge's
/// `OrchestratorTurnDriver` — can construct the already-decoded sources handed to
/// [`ConversationOrchestrator::run_turn_streaming_with_cancel_image_sources`].
pub use lingxi_core::types::ImageSource;
use std::sync::Arc;
use telemetry::tengu::orchestrator as orch_events;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tool_api::registry::ToolRegistry;
use tool_api::ToolRegistryView as _;

/// Current owned request for the conversation API. Each implementation must
/// explicitly execute the selected policy and every main-request option.
#[derive(Debug, Clone)]
pub enum OrchestratorApiRequest {
    Main(llm_runtime::MessagesCreateRequest),
    HookPrompt(HookPromptRequest),
}

/// Isolated structured prompt-hook evaluation, independent of main settings.
#[derive(Debug, Clone)]
pub struct HookPromptRequest {
    pub model: String,
    pub profile: Option<String>,
    pub system: String,
    pub messages: Vec<ConversationMessage>,
}

impl HookPromptRequest {
    #[must_use]
    pub fn new(
        model: &str,
        profile: Option<&str>,
        system: &str,
        messages: Vec<ConversationMessage>,
    ) -> Self {
        Self {
            model: model.to_owned(),
            profile: profile.map(str::to_owned),
            system: system.to_owned(),
            messages,
        }
    }
}

/// Minimal current contract the orchestrator needs from the API client.
#[async_trait]
pub trait OrchestratorApiClient: Send + Sync {
    /// Exact native route identity. Unavailable route metadata admits no Fast mode.
    fn is_first_party_route(&self, _model: &str, _profile: Option<&str>) -> bool {
        false
    }

    /// Exact selected wire protocol for this model/profile route. Unknown
    /// routes do not admit Native-only request context.
    fn protocol_for_model(
        &self,
        _model: &str,
        _profile: Option<&str>,
    ) -> Option<llm_runtime::ProtocolFamily> {
        None
    }

    /// Native QTe model-option observation for the selected request route.
    /// `None` means no local override was observed; callers then apply the
    /// route's baked Native default.
    fn native_model_option_capability_for_request(
        &self,
        _model: &str,
        _profile: Option<&str>,
        _capability: &str,
    ) -> Option<bool> {
        None
    }

    /// Service-lifetime Native thinking-resumption refusal latch. Clients
    /// without the SDK error consumer leave this Unknown.
    fn native_thinking_resumption_refused(&self) -> Option<bool> {
        None
    }

    /// Native YKe capability for a resolved Foundry deployment. `Some(false)`
    /// is an observed unsupported `tool_search_server` or `tool_search`;
    /// `Some(true)` includes the Native default for an unobserved deployment.
    /// `None` means this client cannot authoritatively resolve a Foundry route.
    fn foundry_tool_search_supported(
        &self,
        _model: &str,
        _profile: Option<&str>,
        _model_source: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>,
    ) -> Option<bool> {
        None
    }

    /// Current exact Native model option, when supplied by the host environment.
    fn model_validation_custom_model_option(&self) -> Option<Vec<u16>> {
        None
    }

    /// Account epoch for model-validation probe fencing. Implementations that
    /// do not own an account observer may leave this unavailable.
    fn model_validation_account_generation(&self) -> Option<u64> {
        None
    }

    /// Native hQ capability for the selected current route, including its
    /// settings and account exclusions. Unavailable host facts admit no support.
    fn supports_native_mid_conversation_system(
        &self,
        _model: &str,
        _profile: Option<&str>,
    ) -> bool {
        false
    }

    /// Native `mSe(model)` capability after route/model gates. `None` means
    /// the selected route cannot provide an authoritative answer; a known
    /// unsupported route/model returns `Some(false)`.
    fn native_mid_conversation_tool_change_supported(
        &self,
        _model: &str,
        _profile: Option<&str>,
    ) -> Option<bool> {
        None
    }

    /// Process-local Native MCTC and inline-tools gates sampled by the
    /// selected host service for this request. Unwired clients fail closed.
    fn native_tool_change_feature_gates(&self) -> (bool, bool) {
        (false, false)
    }

    /// Inspect the selected credential's redacted origin without executing auth.
    async fn credential_source(
        &self,
        _model: &str,
        _profile: Option<&str>,
    ) -> Result<llm_runtime::CredentialSource, LlmError> {
        Ok(llm_runtime::CredentialSource::Unknown)
    }

    /// Redacted native classifier model gates for the exact selected route.
    async fn classifier_model_route_facts(
        &self,
        _model: &str,
        _profile: Option<&str>,
    ) -> Result<llm_runtime::model::classifier_model_policy::ClassifierModelRouteFacts, LlmError>
    {
        Ok(Default::default())
    }

    /// Provider wire chosen by the host route, independent of model name guesses.
    fn native_computer_provider(
        &self,
        _model: &str,
        _profile: Option<&str>,
    ) -> Option<lingxi_llm_client::protocol::computer::NativeComputerProvider> {
        None
    }

    /// Project host-owned static prompt sections into the Native source-vector
    /// contract for the selected model/profile. Non-routing clients fail closed
    /// and preserve the grouped source strings without a provider marker.
    fn prompt_snapshot_source_vector(
        &self,
        _model: &str,
        _profile: Option<&str>,
        sections: &[lingxi_llm_client::providers::anthropic::system_prompt::SourceSection],
    ) -> Vec<lingxi_llm_client::providers::anthropic::system_prompt::PromptText> {
        lingxi_llm_client::providers::anthropic::system_prompt::snapshot_source_vector(
            sections,
            None,
            lingxi_llm_client::providers::anthropic::system_prompt::GatePolicy::default(),
        )
    }

    /// Embeddings own any provider-specific enable admission. The production
    /// provider adapter checks native organization and policy state.
    async fn validate_fast_enable(
        &self,
        _model: &str,
        _profile: Option<&str>,
    ) -> Result<(), LlmError> {
        Ok(())
    }

    /// Probe an exact model source on a known route without inheriting turn policy.
    ///
    /// Non-routing API clients fail explicitly instead of treating an unperformed
    /// probe as successful validation.
    async fn probe_model(
        &self,
        _route_model: &str,
        _profile: Option<&str>,
        _source_model: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) -> Result<(), LlmError> {
        Err(LlmError::InvalidRequest {
            message: "model validation is unsupported by this API client".to_owned(),
        })
    }

    /// Execute an owned main request or isolated structured hook evaluation.
    async fn messages_create(
        &self,
        request: OrchestratorApiRequest,
    ) -> Result<HistoryResponse, LlmError>;

    /// Buffer the provider stream until a terminal response before desktop dispatch.
    async fn messages_create_buffered_stream(
        &self,
        request: llm_runtime::MessagesCreateRequest,
    ) -> Result<HistoryResponse, LlmError> {
        self.messages_create(OrchestratorApiRequest::Main(request))
            .await
    }

    /// Count the input tokens a `messages.create` for `(model, system, msgs,
    /// tools)` would consume on its resolved route.
    ///
    /// [`ProviderApiAdapter`](crate::provider_adapter::ProviderApiAdapter)
    /// overrides this to call the real `/v1/messages/count_tokens` endpoint on
    /// Anthropic routes (with the `count_tokens` beta) and a byte-length/4
    /// approximation elsewhere (see [`crate::model::count_tokens`]). The default
    /// here is that same byte/4 approximation computed directly from the
    /// conversation text, so mocks and non-routing impls return a sane estimate
    /// without a network call.
    async fn count_tokens(
        &self,
        _model: &str,
        _profile: Option<&str>,
        system: Option<&str>,
        msgs: Vec<ConversationMessage>,
        _tools: Vec<lingxi_core::host::SourceToolWireDefinition>,
    ) -> Result<u64, LlmError> {
        let mut bytes = system.map_or(0u64, |s| s.len() as u64);
        bytes += msgs
            .iter()
            .map(lingxi_core::types::text_byte_size)
            .sum::<u64>();
        Ok((bytes / crate::model::count_tokens::APPROX_CHARS_PER_TOKEN).max(1))
    }

    /// Return an exact provider count when available. The default is `None`
    /// so mocks and providers without Anthropic's count endpoint select their
    /// caller-specific fallback instead of receiving the text-only estimate.
    async fn count_tokens_exact(
        &self,
        _model: &str,
        _profile: Option<&str>,
        _system: Option<&str>,
        _msgs: Vec<ConversationMessage>,
        _tools: Vec<lingxi_core::host::SourceToolWireDefinition>,
    ) -> Result<Option<u64>, LlmError> {
        Ok(None)
    }

    /// Host-validated beta additions applied to provider requests. The
    /// orchestrator consumes these for the same context-window/compaction
    /// calculations; default clients have none.
    /// None is reserved for hosts without model routing. Production returns a
    /// route-specific snapshot even when its limits are unknown.
    fn context_budget(
        &self,
        _model: &str,
        _profile: Option<&str>,
    ) -> Result<Option<llm_runtime::ResolvedContext>, LlmError> {
        Ok(None)
    }

    /// Return the raw context-window size, distinct from the effective input
    /// budget after policy limits, output reservations, and safety margins.
    /// An unknown route or provider window remains unknown.
    fn context_window_tokens(&self, model: &str, profile: Option<&str>) -> Option<u64> {
        self.context_budget(model, profile)
            .ok()
            .flatten()
            .and_then(|context| context.context_window_tokens)
    }

    fn input_context_budget(&self, model: &str, profile: Option<&str>) -> Option<u64> {
        match self.context_budget(model, profile) {
            Ok(Some(context)) => context.effective_input_budget,
            Ok(None) => Some(compaction::context_window::context_window_for_model(
                model,
                &self.active_betas(),
            )),
            Err(error) => {
                tracing::warn!(%error, "context budget is unavailable");
                None
            }
        }
    }

    fn context_warning(
        &self,
        tokens: u64,
        model: &str,
        profile: Option<&str>,
    ) -> compaction::thresholds::TokenWarningState {
        match self.context_budget(model, profile) {
            Ok(Some(context)) => compaction::thresholds::warning_for_input_budget(
                tokens,
                context.effective_input_budget,
            ),
            Ok(None) => {
                compaction::calculate_token_warning_state(tokens, model, &self.active_betas(), true)
            }
            Err(error) => {
                tracing::warn!(%error, "context pressure is unavailable");
                compaction::thresholds::warning_for_input_budget(tokens, None)
            }
        }
    }

    fn active_betas(&self) -> Vec<String> {
        Vec::new()
    }

    /// Enumerate available `provider/model` ids + `@aliases` for `/model`'s
    /// list mode. Default returns empty so non-routing impls (mocks / the
    /// no-streaming stub) need no override; `ProviderApiAdapter` overrides it
    /// to delegate to the router.
    fn available_models(&self) -> Vec<String> {
        Vec::new()
    }

    /// Resolve the selected main route plus an optional same-profile vision delegate.
    fn resolve_media_route(
        &self,
        _model: &str,
        _profile: Option<&str>,
    ) -> Result<llm_runtime::MediaRoute, LlmError> {
        Err(LlmError::ModelUnavailable)
    }

    /// Resolve a bounded retry budget using the exact serving route. Missing
    /// accounting or route facts prohibit an invented increase.
    fn next_output_budget(
        &self,
        _model: &str,
        _profile: Option<&str>,
        _usage: &llm_runtime::ExecutionUsage,
    ) -> Result<Option<u32>, LlmError> {
        Ok(None)
    }

    /// Run the internal vision-delegation side query for a non-vision main route.
    async fn analyze_vision_delegation(
        &self,
        _packet: sidequery::VisionPacket,
    ) -> Result<sidequery::VisionDelegationResult, LlmError> {
        Err(LlmError::MediaDelegationUnavailable {
            message: "vision delegation is unavailable for this provider".to_string(),
        })
    }

    /// Replace the thinking policy used for subsequent provider requests.
    /// Implementations without a mutable request layer may keep the default
    /// no-op; the production provider adapter overrides it.
    fn set_thinking_config(&self, _thinking: llm_runtime::model::thinking::ThinkingConfig) {}

    /// Set or clear Native's nullable max-thinking-token overlay without
    /// replacing the session's base thinking policy.
    fn set_max_thinking_tokens_override(
        &self,
        _value: Option<llm_runtime::model::thinking_state::NativeIntegerNumber>,
    ) {
    }

    /// Replace the main-loop effort used for subsequent provider requests.
    /// Inherit, explicit automatic default and a pinned value stay distinct.
    fn set_effort(&self, _effort: lingxi_core::host::effort_table::SessionEffort) {}

    /// Native command inputs from the current selected SDK route. Non-native
    /// routes return None and continue through provider-neutral controls.
    fn effort_command_snapshot(
        &self,
        _model: &str,
        _profile: Option<&str>,
    ) -> Result<Option<lingxi_core::host::effort::EffortCommandSnapshot>, LlmError> {
        Err(LlmError::ModelUnavailable)
    }

    /// Query-owned refusal text facts resolved by the embedding host. An
    /// absent source stays absent; managed model admission is not the native
    /// refusal-model eligibility decision.
    fn refusal_api_text_snapshot(
        &self,
        _model: &str,
        _profile: Option<&str>,
    ) -> Result<Option<lingxi_core::host::refusal_api_text::RefusalApiTextSnapshot>, LlmError> {
        Ok(None)
    }

    /// Richer catalog listing for the grouped `/model` picker. Default returns
    /// empty (mocks / non-routing impls); `ProviderApiAdapter` overrides it.
    fn list_model_listings(&self) -> Vec<ModelListing> {
        Vec::new()
    }

    /// Return the most recently observed rate-limit header snapshot, if any.
    ///
    /// Default returns `None`.  `ProviderApiAdapter` overrides this to delegate
    /// to [`crate::provider_adapter::ProviderApiAdapter::last_rate_limit_info`],
    /// which is populated from every successful 2xx response's headers.
    ///
    /// Returns a [`lingxi_core::host::RateLimitSnapshot`] carrying all three header-derived
    /// fields (`rate_limit_type`, `overage_status`, `overage_disabled_reason`).
    /// Using the public snapshot type avoids leaking the orchestrator-internal
    /// `RateLimitInfo` struct through the trait.
    fn last_rate_limit_info(&self) -> Option<lingxi_core::host::RateLimitSnapshot> {
        None
    }

    /// The Anthropic `request-id` response header (`req_…`) of the most
    /// recently completed call — recorded by the adapter from the stream
    /// connect-success / non-stream response headers (the same pass that records
    /// the rate-limit snapshot). Used to stamp the persisted assistant line's
    /// top-level `requestId` (claude-code's `response._request_id`). Default
    /// `None` for mocks / non-recording impls.
    fn last_request_id(&self) -> Option<String> {
        None
    }

    /// Number of budget-consuming retry attempts the most recent API call
    /// performed before succeeding. Recorded by the adapter from its retry
    /// driver's `RetryState`. Used by the cost-recording call sites to pass the
    /// real retry count to `CostTracker::record_api_response_v2` instead of the
    /// previous hardcoded `0` (#5 main-loop parity). Default `0` for mocks /
    /// non-retrying impls.
    fn last_retry_count(&self) -> u32 {
        0
    }

    /// Sticky thinking-signature strip latch. Default `false`.
    fn thinking_signature_stripped(&self) -> bool {
        false
    }

    /// Restore or arm the thinking-signature strip latch for later thinking
    /// turns. No-op on mocks.
    fn set_thinking_signature_stripped(&self, _stripped: bool) {}

    /// Rejected historical thinking ranges; newly generated blocks are unmarked.
    fn thinking_stripped_messages(&self) -> std::collections::HashMap<MessageId, usize> {
        std::collections::HashMap::new()
    }

    fn set_thinking_stripped_messages(
        &self,
        _messages: std::collections::HashMap<MessageId, usize>,
    ) {
    }

    /// Return the FULL most recently observed rate-limit header snapshot.
    ///
    /// Task 8 (llm-runtime future-work batch 3): unlike
    /// [`Self::last_rate_limit_info`] — whose signature is kept untouched and
    /// projects the three-field public `lingxi_core::host::RateLimitSnapshot` — this
    /// returns the orchestrator-internal nine-field
    /// [`crate::model::rate_limit::RateLimitInfo`] so the turn drivers can
    /// forward every unified header value to
    /// `lingxi_core::host::OutputStream::emit_rate_limit`.
    ///
    /// Default returns `None` (mocks / non-Anthropic impls compile
    /// unchanged); `ProviderApiAdapter` overrides it to expose its cached
    /// per-response snapshot.
    fn last_rate_limit_full(&self) -> Option<crate::model::rate_limit::RateLimitInfo> {
        None
    }

    /// Return the most recently observed RAW per-window utilization snapshot.
    ///
    /// Task 2 (llm-runtime future-work batch 5): the parallel accessor to
    /// [`Self::last_rate_limit_full`] for claude-code's `rawUtilization`
    /// tracking (`extractRawUtilization`, `claudeAiLimits.ts:164-179`) —
    /// per-window 5h/7d values recorded on every unified-headers response,
    /// independent of the warning-gated [`Self::last_rate_limit_full`]
    /// fields.
    ///
    /// Default returns `None` (mocks / non-Anthropic impls compile
    /// unchanged); `ProviderApiAdapter` overrides it to expose the snapshot
    /// cached alongside the rate-limit parse.
    fn last_raw_utilization(&self) -> Option<crate::model::rate_limit::RawUtilization> {
        None
    }

    /// Return the user-facing copy composed from the most recent 429 **error**
    /// response, if any.
    ///
    /// Task 6 (llm-runtime future-work batch 5): claude-code builds the
    /// rejected-limits view from the terminal 429's own headers and renders
    /// `getRateLimitErrorMessage` as the user-visible error content
    /// (`errors.ts:480-524`). `ProviderApiAdapter` overrides this to expose the
    /// copy it composed when it decoded the 429: Anthropic limits text from
    /// unified headers, or an actionable OpenRouter free-tier message derived
    /// from the response body. The public turn drivers consult it to
    /// re-map a terminal `RateLimited` error into
    /// [`OrchestratorError::RateLimitRejected`].
    ///
    /// Default returns `None` (mocks / non-Anthropic impls keep the generic
    /// `"api call failed: rate limited"` surface).
    fn last_rate_limit_error_message(&self) -> Option<String> {
        None
    }

    /// Best-effort startup prewarm for OpenAI Responses WebSocket providers.
    ///
    /// Default is a no-op so non-routing mocks and non-WebSocket clients keep
    /// their existing behavior. Production [`ProviderApiAdapter`] sends the
    /// provided empty-history request with `generate=false`.
    async fn prewarm_responses_websocket(
        &self,
        _model: &str,
        _profile: Option<&str>,
        _system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        _messages: Vec<ConversationMessage>,
        _tools: Vec<lingxi_core::host::SourceToolWireDefinition>,
        _skip_global_cache_for_system_prompt: bool,
    ) -> Result<(), LlmError> {
        Ok(())
    }

    /// Close any reusable Responses WebSocket session held by this API client.
    async fn close_responses_websocket_session(&self) -> Result<(), LlmError> {
        Ok(())
    }
}

/// Re-map a terminal `RateLimited` turn error onto the user-facing copy
/// composed from the 429 response (claude-code `errors.ts:480-524`):
/// when the turn died on a 429 AND the API client recorded a composed
/// rejected-limits message, the user-visible error becomes that copy
/// ([`OrchestratorError::RateLimitRejected`]); otherwise the error passes
/// through untouched. Covers both wrappers a 429 can ride in on — the
/// batched `ApiCall` and the streaming connect-phase `Streaming`.
fn enrich_rate_limited_error(
    err: OrchestratorError,
    composed: Option<String>,
) -> OrchestratorError {
    let is_rate_limited = match &err {
        OrchestratorError::ApiCall(error) | OrchestratorError::Streaming(error) => {
            matches!(error.classification_source(), LlmError::RateLimited { .. })
        }
        _ => false,
    };
    if !is_rate_limited {
        return err;
    }
    match composed {
        Some(message) => OrchestratorError::RateLimitRejected { message },
        None => err,
    }
}

/// Streaming-API surface used by the orchestrator's streaming turn loop.
///
/// Mirrors [`OrchestratorApiClient`] but returns a typed
/// `BoxStream<'static, Result<HistoryEvent, LlmError>>` instead of a
/// single `HistoryResponse`. The orchestrator owns the stream and drives
/// it to completion (or `message_stop` / `Completed`).
///
/// Production: [`ProviderApiAdapter`] (Task 6) drives `ModelRuntime`
/// directly. Tests: `MockStreamingApiClient` in `test_support_stream.rs`.
#[async_trait]
pub trait StreamingApiClient: Send + Sync {
    /// Open a streaming `messages.create` request. The returned stream
    /// yields wire-decoded `HistoryEvent` values until the server emits
    /// `message_stop` or a `Completed` event. The implementation is
    /// responsible for HTTP, SSE chunk buffering, and JSON-decoding the
    /// `data:` lines into typed `HistoryEvent` values.
    ///
    /// `profile` — optional provider profile name (e.g. `"github-copilot"`).
    /// Mirrors the `profile` parameter on the batched `messages_create*`
    /// methods so the streaming path can thread `SessionState::model_profile`
    /// through to `build_request` / `ModelRuntime::prepare`.
    async fn stream(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        messages: Vec<ConversationMessage>,
        tools: Vec<llm_runtime::ToolDeclaration>,
        query_source: &str,
        skip_global_cache_for_system_prompt: bool,
        request_dispatch_admission: Option<llm_runtime::RequestDispatchAdmission>,
    ) -> Result<futures::stream::BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError>;

    /// A single `turn.step` hook may override effort for just this physical
    /// request. Other callers retain the session effort through `stream`.
    async fn stream_with_effort_override(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        messages: Vec<ConversationMessage>,
        tools: Vec<llm_runtime::ToolDeclaration>,
        max_tokens: Option<u32>,
        effort: Option<&str>,
        tool_result_reminder_suffixes: Vec<llm_runtime::convert::AnthropicToolResultReminderSuffix>,
        query_source: &str,
        skip_global_cache_for_system_prompt: bool,
        request_dispatch_admission: Option<llm_runtime::RequestDispatchAdmission>,
        request_dispatch_observer: Option<llm_runtime::RequestDispatchObserver>,
        native_anthropic_request_context: Option<lingxi_llm_client::NativeAnthropicRequestContext>,
        typed_attachment_ids: std::collections::HashSet<MessageId>,
        task_deliveries: std::collections::HashMap<
            lingxi_core::types::MessageId,
            lingxi_core::host::task_delivery::TaskDeliveryRequest,
        >,
        model_source: Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    ) -> Result<futures::stream::BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        if max_tokens.is_some() {
            return Err(LlmError::UnsupportedCapability {
                capability: "streaming output budget override".into(),
            });
        }
        let _ = effort;
        let _ = tool_result_reminder_suffixes;
        let _ = request_dispatch_observer;
        let _ = native_anthropic_request_context;
        let _ = typed_attachment_ids;
        let _ = task_deliveries;
        let _ = model_source;
        self.stream(
            model,
            profile,
            system,
            messages,
            tools,
            query_source,
            skip_global_cache_for_system_prompt,
            request_dispatch_admission,
        )
        .await
    }

    /// Connect-phase retry count of the most recent `stream` call (the value
    /// the adapter knows when it returns the stream). Used by the streaming
    /// cost-recording site to pass the real retry count to
    /// `CostTracker::record_api_response_v2` instead of `0` (#5 main-loop
    /// parity). Default `0` for mocks / non-retrying impls.
    fn last_retry_count(&self) -> u32 {
        0
    }

    /// Sticky thinking-signature strip latch. Default `false`.
    fn thinking_signature_stripped(&self) -> bool {
        false
    }

    /// Restore or arm the thinking-signature strip latch. No-op on mocks.
    fn set_thinking_signature_stripped(&self, _stripped: bool) {}

    /// Rejected historical thinking ranges; newly generated blocks are unmarked.
    fn thinking_stripped_messages(&self) -> std::collections::HashMap<MessageId, usize> {
        std::collections::HashMap::new()
    }

    fn set_thinking_stripped_messages(
        &self,
        _messages: std::collections::HashMap<MessageId, usize>,
    ) {
    }
}

/// Outcome of a single REPL turn driven by
/// [`ConversationOrchestrator::run_turn_with_cancel`]. (M5-13)
///
/// Distinct from [`ConversationOutcome`] because the REPL needs to react
/// differently to each variant without inspecting the `stop_reason` string:
/// - `EndTurn` → silent, loop back to prompt.
/// - `MaxTurns` → print `[turn ended: reached MAX_TURNS_PER_CONVERSATION]`.
/// - `Cancelled` → print the SIGINT feedback line + loop back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    /// Model returned `end_turn` (or any other natural stop reason).
    EndTurn,
    /// The orchestrator's `max_turns` limit was reached before `end_turn`.
    MaxTurns,
    /// A `CancellationToken` passed to [`ConversationOrchestrator::run_turn_with_cancel`]
    /// was cancelled mid-turn (SIGINT / external cancel). The orchestrator
    /// unwound the current API call and returned early.
    Cancelled,
}

/// Result of `ConversationOrchestrator::run_turn` on success.
///
/// Only one variant in M5-02; M5-04 may add `Cancelled { ... }` later.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ConversationOutcome {
    /// The query completed after `turn_count` admitted query cycles.
    /// `final_message_id` is the id of the final assistant message
    /// appended to the session.
    EndTurn {
        /// Admitted query cycles, including a StructuredOutput terminal cycle
        /// that finishes from the accepted tool result without another API call.
        turn_count: u32,
        /// Stable identifier of the final assistant message.
        final_message_id: MessageId,
    },
    /// A `Stop` lifecycle hook requested *preventContinuation* (`continue:false`)
    /// — the turn loop terminated the agent rather than continuing to work
    /// (hooks B4, TS `query.ts:1278`). Distinct from [`Self::EndTurn`] so callers
    /// can tell a hook-forced stop from a natural `end_turn`.
    StopHookPrevented {
        /// Admitted query cycles before the Stop hook forced termination.
        turn_count: u32,
        /// Stable identifier of the final assistant message, if any.
        final_message_id: MessageId,
    },
}

/// Outcome of firing the `Stop` lifecycle hooks at end-of-turn (hooks B4).
///
/// Mirrors the three-way branch in TS `query.ts:1267-1306`: a Stop hook can
/// force termination (`preventContinuation`), ask the agent to keep working
/// (a bare `Block` / exit-2), or pass (no Stop hook, or it allowed the stop).
enum StopHookDisposition {
    /// No Stop hook fired, or it allowed the stop — proceed to the normal
    /// end-of-turn (token-budget check then `emit_end_turn`).
    Pass,
    /// A Stop hook blocked the stop (wants the agent to keep working). The turn
    /// loop appends the carried blocking reason as a meta user message wrapped by
    /// `getStopHookMessage` (TS `utils/hooks.ts:1895`), sets
    /// `stop_hook_active = true`, and runs one more turn step. The re-entry
    /// guard converts a *second* such block into [`Self::Pass`] so a hook that
    /// always blocks cannot loop forever (TS `query.ts:1297`). The carried
    /// `String` is the hook's blocking reason (`blockingError.blockingError`,
    /// TS `query/stopHooks.ts:257-262`), NOT the transcript-only systemMessage.
    Continue(lingxi_core::types::utf16_json::Utf16JsonProjection),
    /// A session-scoped `/goal` Stop Prompt hook blocked natural completion.
    /// Shares the Stop-hook block cap; the distinct variant lets a capped
    /// goal announce why it paused without clearing the condition.
    GoalContinue(lingxi_core::types::utf16_json::Utf16JsonProjection),
    /// A Stop hook requested `continue: false` — terminate the agent loop
    /// (TS `query.ts:1278`); the turn ends as `StopHookPrevented`. The carried
    /// `String` is the hook's `stopReason` (defaulted to
    /// `"Stop hook prevented continuation"`, TS `query/stopHooks.ts:271`) —
    /// persisted as a `hook_stopped_continuation` meta message before the turn
    /// terminates (FIX C).
    Prevent(lingxi_core::types::utf16_json::Utf16JsonProjection),
}

const GOAL_PROMPT_TIMEOUT_SECS: u64 = 30;
const GOAL_STOP_HOOK_NAME: &str = "__session_goal_stop";
const GOAL_STOP_HOOK_PRIORITY: i32 = 1_000_000;

/// Driver control-flow directive produced by `handle_stop_at_end` (hooks B4) so
/// the three turn drivers (batched / streaming / cancelable) translate the Stop
/// disposition into their own loop mechanics uniformly.
enum StopHookFlow {
    /// Terminate the turn loop, returning this outcome (`emit_end_turn` already
    /// fired inside the helper).
    Terminate(ConversationOutcome),
    /// A Stop hook blocked the turn from ending, but the next turn would exceed
    /// `max_turns`, so end NOW on the max-turns terminal instead of looping
    /// (binary blocking-branch `if(c&&dt>c) … {reason:"max_turns",turnCount:dt}`).
    /// The caller converts this to `OrchestratorError::MaxTurnsReached`
    /// (→ `TurnOutcome::MaxTurns`); the `tengu_stop_hook_block_count`
    /// `{hit_max_turns:true}` event is fired inside the helper before returning.
    TerminateMaxTurns,
    /// A Stop hook asked the agent to keep working — loop one more turn step.
    LoopAgain,
    /// No Stop hook intervened — fall through to the driver's normal end
    /// (token-budget check, then `emit_end_turn` + break).
    FallThrough,
}

/// `getEntrypoint()` (`sessionStorage.ts:1058`) — the CLI entrypoint stamped on
/// every JSONL line. claude-code reads `process.env.CLAUDE_CODE_ENTRYPOINT`
/// (defaulting to `"cli"`); we mirror that (same env the UA builder reads,
/// `model/user_agent.rs:73`) so an embedder can override it, but the parity
/// default is the hardcoded `"cli"`.
fn entrypoint_value() -> String {
    std::env::var("CLAUDE_CODE_ENTRYPOINT")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "cli".to_string())
}

/// The top-level api-error envelope fields claude-code stamps on a synthetic
/// assistant line built via `createAssistantAPIErrorMessage` (`ql`/`tc`) or the
/// refusal builder (`fje`). On disk these are SIBLINGS of `message` —
/// `isApiErrorMessage` (always `true`), an optional `error` category string,
/// optional `truncatedAfterOutput` (cc `tZo`, omitted when false), and an
/// optional `apiErrorStatus` HTTP status — read back by the loader's
/// transcript reconstruction (`ht=Ie.isApiErrorMessage===!0, st=Ie.apiErrorStatus`).
///
/// Field shapes are pinned against the 2.1.195 binary + real transcripts:
/// - `error` is the category passed to the builder; it is OMITTED (not `null`)
///   when the builder is called with no `error:` arg — e.g. the top-level
///   `model_error` catch (`createAssistantAPIErrorMessage({content})`) and the
///   malformed-tool-use terminal (`ql({content})`).
/// - `api_error_status` mirrors `KNn`'s `r.apiErrorStatus=e.status` — set only
///   when the error was an `APIError` with a numeric status; otherwise omitted.
/// - `inner_stop_reason` overrides the synthetic inner `message.stop_reason`.
///   The `ql`/`tc` path leaves it `"stop_sequence"` (the default); the refusal
///   `fje` path keeps `"refusal"` (verified on disk: the sole refusal line
///   carries `stop_reason:"refusal", error:"invalid_request"`).
#[derive(Clone, Debug, Default)]
pub(crate) struct ApiErrorEnvelope {
    pub error: Option<&'static str>,
    pub api_error_status: Option<u16>,
    /// Native terminal reason when the classified API failure has a specialized exit.
    pub terminal_reason: Option<&'static str>,
    pub inner_stop_reason: Option<&'static str>,
    /// `truncatedAfterOutput` on the api-error assistant (cc `Co` / `tZo`).
    /// Set when the incomplete-response notice follows a partial that already
    /// yielded real output. Omitted from JSONL when false.
    pub truncated_after_output: bool,
}

/// Per-request api-error classifier — the port of claude-code's `Flp`/`KNn`
/// (the message + CATEGORY producer and its `apiErrorStatus` decorator). Maps a
/// model/runtime error that escaped the API layer to the top-level api-error
/// envelope (`error` category string + optional `apiErrorStatus`) that the
/// graceful `model_error` catch stamps on the persisted assistant line.
///
/// Recovered from the 2.1.195 binary (`Flp` if-chain + `KNn`):
/// `Flp` returns `ql({content,error:<CATEGORY>})` per branch, and `KNn` adds
/// `r.apiErrorStatus=e.status` ONLY when the error is an `APIError` with a
/// numeric status (otherwise it is OMITTED, not `null`). The category strings
/// (`rate_limit`/`invalid_request`/`server_error`/`authentication_failed`/
/// `model_not_found`/`billing_error`/`unknown`) are byte-lockable file-format
/// values read back by the loader's transcript reconstruction, so they are kept
/// verbatim from the binary.
///
/// MULTI-PROVIDER CARVE-OUT: claude's `Flp` dispatches on `e instanceof $o &&
/// e.status===N` (Anthropic `APIError`). This port instead dispatches on the
/// provider-NEUTRAL [`LlmError`] semantic enum, so the classifier works
/// identically for every provider (an OpenAI/Gemini rate-limit decoded into
/// `LlmError::RateLimited` still maps to `rate_limit`/429). Because the raw HTTP
/// status was already collapsed into the semantic variant, exact per-request
/// status recovery for the long tail is unavailable; each variant maps to its
/// CANONICAL status and the status is OMITTED (`None`) where the port has no
/// confident canonical value — mirroring claude omitting `apiErrorStatus` when
/// the error is not an `APIError`-with-numeric-status (a wrong status would be
/// worse than an omitted one). On-disk 529 lines carry `error:"server_error"`
/// (the `Flp` tail `status>=500` branch), which is authoritative over the `YNn`
/// statusline classifier's `529→"overloaded"`.
///
/// `inner_stop_reason` is always `None` here: the `ql` path leaves the synthetic
/// inner `message.stop_reason` at `"stop_sequence"` (verified on disk).
pub(crate) fn classify_api_error(e: &OrchestratorError) -> ApiErrorEnvelope {
    // TRUE status first, canonical table second.
    //
    // This is the half of the carve-out documented above that is now closed.
    // Provider decoders store the SDK's `${status} ${body}` text (see
    // `providers::api_error_message`), so a real 422/424/409 is recoverable
    // instead of being flattened to its variant's canonical status. The table
    // below still runs whenever no prefix is present — every variant that
    // carries no message, and every `InvalidRequest` raised by internal
    // validation rather than a provider decode.
    //
    // Still provider-NEUTRAL: the prefix is written by whichever provider
    // decoded the response, so this does not reintroduce an Anthropic-only path.
    let parsed_status = match e {
        OrchestratorError::ApiCall(inner) | OrchestratorError::Streaming(inner) => {
            inner.http_status()
        }
        _ => None,
    };
    let terminal_reason = match e {
        OrchestratorError::ApiCall(inner) | OrchestratorError::Streaming(inner)
            if matches!(inner.classification_source(), LlmError::RequestTooLarge) =>
        {
            Some("image_error")
        }
        _ => None,
    };
    let (error, api_error_status) = match e {
        OrchestratorError::ApiCall(inner) | OrchestratorError::Streaming(inner) => {
            match inner.classification_source() {
                LlmError::ProviderErrorText { .. } | LlmError::ProxyAuthHelperRetry { .. } => {
                    unreachable!("classification_source removes carrier wrappers")
                }
                // 429 family → "rate_limit" (status 429). Carved out before reaching
                // `surface_model_error` for the non-streaming path; kept for totality
                // and exercised only via a non-carved `Streaming` surface.
                LlmError::RateLimited { .. } => (Some("rate_limit"), Some(429)),
                // 529 overload: the ENVELOPE (`Flp` tail `status>=500`) and on-disk
                // 529 lines tag `server_error` — NOT the `YNn` statusline
                // `"overloaded"`. Carved out for the non-streaming path.
                LlmError::Overloaded { .. } => (Some("server_error"), Some(529)),
                // x-api-key / 401 → "authentication_failed".
                LlmError::Authentication { .. } => (Some("authentication_failed"), Some(401)),
                // 403 → "authentication_failed".
                LlmError::PermissionDenied { .. } => (Some("authentication_failed"), Some(403)),
                // Dead OAuth session (`e instanceof qQt`) → the oracle renders it
                // with `yu({error:"authentication_failed"})` and passes NO status:
                // the refresh call failed against the IdP, so there is no
                // `APIError` status to carry. Omit rather than invent a 401.
                LlmError::OAuthRefreshDead => (Some("authentication_failed"), None),
                // Billing (`Fio`) is an Error-message match in `Flp`, not a status
                // branch → category only, no `apiErrorStatus`.
                LlmError::QuotaExceeded => (Some("billing_error"), None),
                // PTL/context-window (`Nio`/`D9t`) → `ql({error:"invalid_request"})`
                // with NO status set; the port decodes ContextOverflow from the
                // message, so no `APIError` status is available → omit.
                LlmError::ContextOverflow { .. } => (Some("invalid_request"), None),
                LlmError::InputBudgetExceeded { .. } => (Some("invalid_request"), None),
                // 413 request-too-large (`su({content:$Vi(),error:"invalid_request",
                // errorDetails:`request_too_large: …`})`, 2.1.212) → the SAME
                // `invalid_request` category as the context-window branch; the
                // handler passes no `apiErrorStatus` on this `su` call → omit.
                LlmError::RequestTooLarge => (Some("invalid_request"), None),
                LlmError::FileUploadOutcomeUnknown { .. } => {
                    (Some("file_upload_outcome_unknown"), None)
                }
                // 400 invalid-request family → "invalid_request" (status 400).
                LlmError::InvalidRequest { .. } => (Some("invalid_request"), Some(400)),
                // 404 / bedrock model-id → "model_not_found".
                LlmError::ModelUnavailable => (Some("model_not_found"), Some(404)),
                // `Flp` tail `status>=500` → "server_error".
                LlmError::ProviderInternal => (Some("server_error"), Some(500)),
                LlmError::ProviderTimeout { .. } => (Some("server_error"), inner.http_status()),
                // Timeout / transport / connection-lost tail → "server_error", no
                // status (these are not `APIError`-with-numeric-status).
                LlmError::Transport { .. }
                | LlmError::TransportTimeout { .. }
                | LlmError::TlsCert { .. }
                | LlmError::StreamInterrupted { .. } => (Some("server_error"), None),
                LlmError::ProxyTunnelResponse { status, .. } => {
                    (Some("server_error"), Some(*status))
                }
                // Generic `Error` fallthrough in `Flp` → "unknown".
                LlmError::MediaDelegationUnavailable { .. }
                | LlmError::MediaDelegationPartial { .. } => (Some("invalid_request"), None),
                LlmError::MalformedToolInput { .. }
                | LlmError::RequestDispatchRejected { .. }
                | LlmError::ApiAttemptsExhausted
                | LlmError::CostUnavailable { .. }
                | LlmError::UnsupportedCapability { .. }
                | LlmError::SettingsSource { .. } => (Some("unknown"), None),
            }
        }
        // Generic-Error fallthrough (`Flp`: `if(e instanceof $o)→"unknown"`;
        // generic Error → "unknown"). These orchestrator-internal variants never
        // carry a status. Turn/budget/structured-output retry limits are handled
        // upstream and never reach `surface_model_error` — dead arms kept for
        // totality.
        OrchestratorError::Internal(_)
        | OrchestratorError::Settings(_)
        | OrchestratorError::PermissionAbort { .. }
        | OrchestratorError::StreamingProtocol(_)
        | OrchestratorError::StreamEndedWithoutStop
        | OrchestratorError::Compaction(_)
        | OrchestratorError::CompactionCancelled
        | OrchestratorError::VisionDelegationCancelled
        | OrchestratorError::RepeatedOverloaded
        | OrchestratorError::RateLimitRejected { .. }
        | OrchestratorError::MaxTurnsReached { .. }
        | OrchestratorError::MaxStructuredOutputRetries { .. }
        | OrchestratorError::MaxBudgetReached { .. } => (Some("unknown"), None),
    };
    ApiErrorEnvelope {
        error,
        api_error_status: parsed_status.or(api_error_status),
        terminal_reason,
        inner_stop_reason: None,
        truncated_after_output: false,
    }
}

/// claude-code 2.1.212 `Sji` — the maximum request body size (32 MiB). A 413
/// whose message does NOT mention the context window means accumulated
/// image/attachment bytes pushed the raw request past this limit.
pub(crate) const MAX_REQUEST_BYTES: u64 = 33_554_432;

/// The byte-exact `$Vi()` "Request too large" notice claude-code 2.1.212 renders
/// for a 413 that is NOT a context-window overflow. `Ua(Sji)` formats
/// [`MAX_REQUEST_BYTES`] (32 MiB) as `32MB` (`toFixed(1)` then trailing `.0`
/// stripped). The tail differs by interactivity (`un()===!Ht.isInteractive`):
/// a non-interactive (print) session gets the generic advice; an interactive
/// (TUI) session gets the `/compact` + double-esc actions.
pub(crate) fn request_too_large_notice(interactive: bool) -> String {
    debug_assert_eq!(MAX_REQUEST_BYTES, 32 * 1024 * 1024);
    let head = "Request too large (max 32MB). Accumulated images and attachments in the conversation pushed the request over the limit.";
    if interactive {
        format!("{head} Run /compact, or double press esc to go back and remove attachments.")
    } else {
        format!("{head} Remove older images or compact the conversation.")
    }
}

/// Build the persisted assistant-envelope `usage` value from a normalized
/// [`llm_runtime::ExecutionUsage`]. Prefers the raw Anthropic usage object the codec
/// retained on `provider_metadata` (byte-faithful to claude-code's persisted
/// `BetaMessage.usage`); falls back to a reconstruction from the normalized
/// billable buckets only when no raw object is present (unusual).
pub(crate) fn assistant_usage_value(
    usage: &llm_runtime::ExecutionUsage,
) -> lingxi_llm_client::protocol::JsonPayload {
    if usage.provider_metadata.is_root_object() {
        return usage.provider_metadata.clone();
    }
    let b = usage.counts();
    let generated = serde_json::json!({
        "input_tokens": b.input_tokens,
        "cache_creation_input_tokens": b.cache_write_tokens,
        "cache_read_input_tokens": b.cache_read_tokens,
        "output_tokens": b.output_tokens,
    });
    lingxi_llm_client::protocol::JsonPayload::from_generated_value(&generated)
        .expect("generated assistant usage is valid JSON")
}

/// Map Rust's `std::env::consts::OS` to the node `process.platform` value that
/// claude-code's `# Environment` `Platform:` line emits (`je.platform`). Rust
/// uses `macos`/`windows`; node uses `darwin`/`win32`. Other targets
/// (`linux`, `freebsd`, …) share the same token in both, so they pass through.
fn node_platform_name(rust_os: &str) -> &str {
    match rust_os {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

/// Native's Rre/Tre gate for tool search. The unsupported-model list applies
/// to every selected route; the stricter version floor is specific to the
/// actual Vertex Claude wire protocol, never inferred from the profile label.
fn tool_search_supported_for_request(
    api: &dyn OrchestratorApiClient,
    model: &str,
    profile: Option<&str>,
    model_source: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>,
) -> bool {
    let unsupported_models = telemetry::flag_string_list(
        "tengu_tool_search_unsupported_models",
        &[
            "claude-3-opus",
            "claude-3-sonnet",
            "claude-3-haiku",
            "claude-3-5-sonnet",
            "claude-3-5-haiku",
            "claude-3-7-sonnet",
        ],
    );
    let protocol = api.protocol_for_model(model, profile);
    if !tool_search_supported_for_model_and_protocol(model, &unsupported_models, protocol) {
        return false;
    }
    if protocol == Some(llm_runtime::ProtocolFamily::FoundryClaude) {
        return api
            .foundry_tool_search_supported(model, profile, model_source)
            .unwrap_or(true);
    }
    true
}

fn tool_search_supported_for_model_and_protocol(
    model: &str,
    unsupported_models: &[String],
    protocol: Option<llm_runtime::ProtocolFamily>,
) -> bool {
    let model_lower = model.to_ascii_lowercase();
    if unsupported_models
        .iter()
        .any(|unsupported| model_lower.contains(&unsupported.to_ascii_lowercase()))
    {
        return false;
    }
    protocol != Some(llm_runtime::ProtocolFamily::VertexClaude)
        || !vertex_tool_search_model_unsupported(model)
}

fn vertex_tool_search_model_unsupported(model: &str) -> bool {
    let normalized = strip_native_model_formatting(model);
    let model = strip_vertex_model_date(&normalized);
    if model == "claude-3" || model.starts_with("claude-3-") {
        return true;
    }
    let Some((_family, version)) = model.strip_prefix("claude-").and_then(|model| {
        let (family, version) = model.split_once('-')?;
        matches!(family, "opus" | "sonnet" | "haiku").then_some((family, version))
    }) else {
        return false;
    };
    // All three Native families currently share the [4, 5] floor.
    if !version
        .chars()
        .next()
        .is_some_and(|character| character.is_ascii_digit())
    {
        return false;
    }
    let Some(version) = version
        .split('-')
        .map(|component| component.parse::<u64>().ok())
        .collect::<Option<Vec<_>>>()
    else {
        // Native's Dhr returns false for names that match the family/version
        // prefix but do not parse as a complete numeric model id.
        return true;
    };
    let floor = [4_u64, 5_u64];
    for index in 0..version.len().max(floor.len()) {
        let actual = version.get(index).copied().unwrap_or(0);
        let minimum = floor.get(index).copied().unwrap_or(0);
        if actual != minimum {
            return actual < minimum;
        }
    }
    false
}

fn strip_native_model_formatting(model: &str) -> String {
    // Native `Fn` removes the literal formatting markers case-insensitively;
    // it does not remove the ESC byte that may precede one of these markers.
    model
        .replace("[1m]", "")
        .replace("[2m]", "")
        .replace("[1M]", "")
        .replace("[2M]", "")
}

fn strip_vertex_model_date(model: &str) -> &str {
    let bytes = model.as_bytes();
    if bytes.len() < 9 {
        return model;
    }
    let suffix = &bytes[bytes.len() - 8..];
    let separator = bytes[bytes.len() - 9];
    if matches!(separator, b'-' | b'@') && suffix.iter().all(u8::is_ascii_digit) {
        &model[..model.len() - 9]
    } else {
        model
    }
}

/// `getBranch()` (`sessionStorage.ts:1012-1019`) — resolve the cwd's current git
/// branch via `git rev-parse --abbrev-ref HEAD`, or `None` on ANY failure (git
/// missing / not a repo / non-zero exit / empty output). A detached HEAD prints
/// the literal `"HEAD"`; we surface that verbatim (claude-code's `getBranch`
/// returns it too — it does not special-case detached HEAD).
///
/// Uses [`std::process::Command`] (no new dependency), the same shell-git pattern
/// the loader already uses for worktree enumeration. One-shot + cached by the
/// caller, so the synchronous `output()` runs at most once per session.
fn git_branch_for_cwd(cwd: &std::path::Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() {
        None
    } else {
        Some(branch)
    }
}

/// Count `document` and `image` content blocks across a message list, for the
/// fixed-prefix overflow telemetry (`a3p`'s `documentBlockCount` /
/// `imageBlockCount`, `bin/claude.exe` offset 203004969).
///
/// The binary recurses into `tool_result.content` arrays; in this port a
/// [`lingxi_core::types::ContentBlock::ToolResult`] carries a flat `String` (it cannot
/// nest image/document blocks), so counting the top-level blocks of each
/// message is the faithful equivalent. Returns `(document_count, image_count)`.
fn count_document_and_image_blocks(
    messages: &[lingxi_core::types::ConversationMessage],
) -> (u32, u32) {
    let mut documents = 0u32;
    let mut images = 0u32;
    for message in messages {
        let blocks = match message {
            lingxi_core::types::ConversationMessage::User { content, .. }
            | lingxi_core::types::ConversationMessage::Assistant { content, .. } => content,
            lingxi_core::types::ConversationMessage::System { .. } => continue,
        };
        for block in blocks {
            match block {
                lingxi_core::types::ContentBlock::Document { .. } => {
                    documents = documents.saturating_add(1)
                }
                lingxi_core::types::ContentBlock::Image { .. } => images = images.saturating_add(1),
                _ => {}
            }
        }
    }
    (documents, images)
}

/// Bare text injected as a user message when streaming is cancelled (ESC /
/// SIGINT) BEFORE any tool runs in the current turn. 1:1 with claude-code
/// `messages.ts:207` `INTERRUPT_MESSAGE`.
const INTERRUPT_MESSAGE: &str = "[Request interrupted by user]";

/// Clone a message for durable persistence, replacing explicitly ephemeral tool
/// images with their non-sensitive summary. The live in-memory message remains
/// untouched and still carries its image content blocks to the current model.
fn redact_ephemeral_tool_result_images(
    message: &lingxi_core::types::ConversationMessage,
) -> lingxi_core::types::ConversationMessage {
    let mut sanitized = message.clone();
    let blocks = match &mut sanitized {
        lingxi_core::types::ConversationMessage::User { content, .. }
        | lingxi_core::types::ConversationMessage::Assistant { content, .. } => content,
        lingxi_core::types::ConversationMessage::System { .. } => return sanitized,
    };
    for block in blocks {
        let lingxi_core::types::ContentBlock::ToolResult {
            content,
            content_blocks,
            ..
        } = block
        else {
            continue;
        };
        let Some(encoded) = content.string_units("") else {
            continue;
        };
        let Ok(marker) =
            lingxi_core::types::utf16_json::Utf16JsonProjection::parse_utf16_json(&encoded)
        else {
            continue;
        };
        if marker
            .at_pointer("/_lingxi_ephemeral")
            .and_then(|value| value.as_bool())
            != Some(true)
        {
            continue;
        }
        *content = marker.subprojection("/summary").unwrap_or_else(|_| {
            lingxi_core::types::utf16_json::Utf16JsonProjection::from(
                "Ephemeral tool image omitted from session persistence.",
            )
        });
        *content_blocks = None;
    }
    sanitized
}

#[cfg(test)]
#[path = "conversation_test.rs"]
mod conversation_test;

#[cfg(test)]
#[path = "model_call_prepare_test.rs"]
mod model_call_prepare_test;

/// Bare text injected as a user message when streaming is cancelled (ESC /
/// SIGINT) DURING tool execution for the current turn. 1:1 with claude-code
/// `messages.ts:208` `INTERRUPT_MESSAGE_FOR_TOOL_USE`.
const INTERRUPT_MESSAGE_FOR_TOOL_USE: &str = "[Request interrupted by user for tool use]";

/// Folded outcome of the `PreCompact` lifecycle hooks.
pub(crate) struct PreCompactHookOutcome {
    /// Blocking reason, when a hook rejected compaction.
    pub(crate) blocked_by: Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    /// Successful hook stdout appended to the summary prompt.
    pub(crate) additional_instructions: Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
}

fn merge_compact_instructions(
    primary: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>,
    additional: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>,
) -> Option<lingxi_core::types::utf16_json::Utf16JsonProjection> {
    let mut parts = Vec::new();
    for source in [primary, additional].into_iter().flatten() {
        let trimmed = compaction::prompt::trim_compact_source_text(source);
        let units = trimmed
            .string_units("")
            .expect("compact instructions are a string");
        if !units.is_empty() {
            parts.push(units);
        }
    }
    if parts.is_empty() {
        return None;
    }
    let mut units = Vec::new();
    for (index, part) in parts.into_iter().enumerate() {
        if index > 0 {
            units.extend("\n\n".encode_utf16());
        }
        units.extend(part);
    }
    Some(
        lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
            String::from_utf16_lossy(&units),
            units,
        )
        .expect("display instructions are derived from exact UTF-16 units"),
    )
}

/// The orchestrator. Owns the session, dispatches tools, drives the loop.
///
/// Construction is via `new(...)` (batched-only) or `new_with_streaming(...)`
/// (both paths). Driven via `run_turn(prompt)` or `run_turn_streaming(prompt)`.
/// Snapshot of the `--agent`-adopted main-thread agent (claude-code
/// `mainThreadAgentDefinition` reduced to the fields LingXi applies on the MAIN
/// conversation loop). Set once at startup by the composition root; see
/// [`ConversationOrchestrator::main_thread_agent`].
#[derive(Debug, Clone)]
pub(crate) struct MainThreadAgentState {
    /// The agent's stable `agentType` (claude-code `mainThreadAgentType` /
    /// `MB()`), threaded into every main-thread lifecycle hook payload.
    pub(crate) agent_type: String,
    /// The agent's system-prompt body (claude-code `agentDef.getSystemPrompt()`)
    /// — becomes the main-loop system prompt on every query unless
    /// `--system-prompt` (`overrideSystemPrompt`) is set. `None` for an agent
    /// that declares no prompt (the assembled default prompt is then used).
    pub(crate) system_prompt: Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    /// The agent's `tools:` frontmatter policy (claude-code `agentDef.tools`).
    /// Filters the advertised main-loop tool pool via [`Self::build_wire_tools`]
    /// — the `HJ(agentDef,to,!1,!0)` port with `n=true`, which keeps everything
    /// on [`AgentToolPolicy::All`] (no `tools:` field) and narrows to the named
    /// tools on [`AgentToolPolicy::Explicit`].
    pub(crate) tool_policy: agent::AgentToolPolicy,
    /// The agent's per-definition `disallowedTools` (claude-code
    /// `agentDef.disallowedTools`) — subtracted from the advertised pool BEFORE
    /// the `tool_policy` projection (base tool name, `(rule)` stripped).
    pub(crate) disallowed_tools: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct WireToolSchemaCacheKey {
    tool_names: Vec<String>,
    dynamic_schema_revisions: Vec<(String, String)>,
    exact_schema_identities: Vec<(String, String)>,
    model: String,
    model_profile: Option<String>,
    /// Whether this request can load `workflow-authoring`, matching the value
    /// passed to each tool's PromptOptions during construction of this entry.
    workflow_authoring_available: bool,
    bash_precommit_skills: tool_api::tool_trait::BashPrecommitSkills,
    bash_precommit_session_generation: u64,
    mod_registration_identity: Option<(u64, u64)>,
    mod_tool_description_generation: u64,
}

#[derive(Debug, Clone)]
struct WireToolSchemaCache {
    key: WireToolSchemaCacheKey,
    wire: Vec<lingxi_core::host::SourceToolWireDefinition>,
}

/// `date_change` (cc `Cop`) per-conversation state.
///
/// The oracle keeps two things: `LGe = Vr(wcs)`, the local date memoized when
/// the session started (the same date the env-context `currentDate` entry was
/// first built from), and the delivered `date_change` attachment itself.
/// Reminders are outgoing-only, so the delivered date is carried here and must
/// survive compaction; otherwise a long-lived session would receive the same
/// midnight reminder again after every compact. The whole struct is re-seeded
/// One `tool_result` SDK frame held until the collection point releases it.
#[derive(Debug, Clone)]
pub(crate) struct PendingToolFrame {
    pub(crate) tool: String,
    /// The model-facing text dispatch buffered. Compared against the released
    /// content to detect a substitution.
    pub(crate) model_text: lingxi_core::types::utf16_json::Utf16JsonProjection,
    pub(crate) result: lingxi_core::types::utf16_json::Utf16JsonProjection,
    pub(crate) denial_kind: Option<String>,
}

/// only when the live `SessionId` changes (`/clear` mints a new one; in-place
/// resume adopts the named one).
#[derive(Debug, Default)]
pub(crate) struct DateChangeState {
    /// Session this state belongs to; `None` until the first producer run.
    session_id: Option<lingxi_core::types::SessionId>,
    /// `LGe()` — the local date memoized at session start.
    session_date: String,
    /// `newDate` of the reminder last DELIVERED to the model in this session.
    delivered_date: Option<String>,
}

/// `plan_mode` attachment cadence — 2.1.238 `X4T` @296525982 with
/// `txl={TURNS_BETWEEN_ATTACHMENTS:5,FULL_REMINDER_EVERY_N_ATTACHMENTS:5}`
/// (@296558044).
///
/// ```js
/// if(t&&t.length>0){let{turnCount:y,foundPlanModeAttachment:_}=ixl(t);
///   if(_&&y<txl.TURNS_BETWEEN_ATTACHMENTS)return[]}
/// …
/// let c=Y4T(t??[])+1;
/// p = (…) || c%txl.FULL_REMINDER_EVERY_N_ATTACHMENTS===1 ? "full" : "sparse";
/// ```
///
/// The oracle derives both numbers by walking the message log backwards:
/// `ixl` (@296524028) counts non-meta user messages that carry NO `tool_result`
/// block (`sxl`/`y3T` @296541952) since the most recent `plan_mode` /
/// `plan_mode_reentry` attachment, and `Y4T` (@296525364) counts the
/// `plan_mode` attachments emitted since the last `plan_mode_exit`.
///
/// LingXi's reminders are OUTGOING-ONLY (never appended to `session.history`),
/// so neither walk is reconstructible from the log. This struct carries the two
/// derived quantities instead: the real-user-turn watermark at the last
/// emission, and the number of attachments emitted since plan mode was entered.
/// It is reset whenever `SessionState::plan_reminder_shown` is observed `false`
/// — the flag `EnterPlanMode` (`tools/plan/src/plan_mode.rs:333`) and
/// `handle_impl.rs:702` clear on plan-mode ENTRY, which is exactly the
/// `plan_mode_exit` boundary `Y4T` stops at.
#[derive(Debug, Default)]
pub(crate) struct PlanReminderCadence {
    /// `Y4T(...)` — `plan_mode` attachments emitted since plan-mode entry.
    attachments_emitted: u32,
    /// The real-user-turn count (see `ixl`) at the last emission; `None` before
    /// the first attachment of this plan-mode stretch, which is the oracle's
    /// `foundPlanModeAttachment === false` and bypasses the cadence gate.
    real_user_turns_at_last_emission: Option<usize>,
}

/// `txl.TURNS_BETWEEN_ATTACHMENTS` @296558044.
const PLAN_TURNS_BETWEEN_ATTACHMENTS: usize = 5;
/// `txl.FULL_REMINDER_EVERY_N_ATTACHMENTS` @296558044.
const PLAN_FULL_REMINDER_EVERY_N_ATTACHMENTS: u32 = 5;

/// Which main-loop driver is preparing an outgoing model call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelCallPath {
    Batched,
    Streaming,
}

/// Retry-safe rewriter for outgoing model-call history snapshots.
#[async_trait]
pub(crate) trait OutgoingHistoryRewriter: Send + Sync {
    async fn rewrite(
        &self,
        orch: &ConversationOrchestrator,
        raw_history: Vec<ConversationMessage>,
    ) -> Result<Vec<ConversationMessage>, OrchestratorError>;
}

/// Retry-safe composition of an existing product-specific history rewrite
/// (for example vision media preparation) with context collapse's read-time
/// projection. The full session history remains untouched.
struct ContextCollapseHistoryRewriter {
    inner: Option<Arc<dyn OutgoingHistoryRewriter>>,
    compactor: Arc<compaction::CompactionOrchestrator>,
}

#[async_trait]
impl OutgoingHistoryRewriter for ContextCollapseHistoryRewriter {
    async fn rewrite(
        &self,
        orch: &ConversationOrchestrator,
        raw_history: Vec<ConversationMessage>,
    ) -> Result<Vec<ConversationMessage>, OrchestratorError> {
        let rewritten = match self.inner.as_ref() {
            Some(inner) => inner.rewrite(orch, raw_history).await?,
            None => raw_history,
        };
        if !compaction::is_context_collapse_enabled() {
            return Ok(rewritten);
        }
        Ok(self
            .compactor
            .context_collapse
            .apply_collapses_if_needed(rewritten)
            .messages)
    }
}

/// Shared output from the pre-call preparation seam used by both main loops.
#[derive(Clone)]
pub(crate) struct PreparedModelCall {
    pub(crate) history_snapshot: Vec<ConversationMessage>,
    pub(crate) model: String,
    pub(crate) model_profile: Option<String>,
    pub(crate) outgoing_history_rewriter: Option<Arc<dyn OutgoingHistoryRewriter>>,
}

/// Shared pre-call hook seam for batched and streaming request preparation.
#[async_trait]
pub(crate) trait ModelCallPreparer: Send + Sync {
    async fn prepare(
        &self,
        orch: &ConversationOrchestrator,
        path: ModelCallPath,
        system_prompt: Option<
            &lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput,
        >,
        cancel: Option<&CancellationToken>,
        draft: PreparedModelCall,
    ) -> Result<PreparedModelCall, OrchestratorError>;
}

const TOOL_TOKEN_COUNT_OVERHEAD: u64 = 500;

/// Host-approved app-specific instructions appended after the immutable
/// platform/runtime prompt layers. This is intentionally not
/// `system_prompt_override`, which would replace the security prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppAgentPromptProfile {
    /// Monotonic host-approved profile revision.
    pub revision: u64,
    /// Bounded app-specific instructions.
    pub instructions: String,
}

/// Cached start-of-conversation git probe + `gitStatus:` attachment.
pub(crate) struct GitStatusSnapshot {
    /// Env-block `Is a git repository` probe.
    pub(crate) probe: Option<crate::prompt::GitStatus>,
    /// Rendered `gitStatus:` system-prompt attachment.
    pub(crate) block: Option<String>,
}

/// Ephemeral facts accumulated from the actual main-loop model responses.
/// `turn.complete` observes them after the turn has settled; they are never
/// added to model history or serialized with the session.
pub(crate) struct ModTurnFacts {
    pub(crate) id: String,
    pub(crate) started_at: std::time::Instant,
    pub(crate) answer: lingxi_core::types::utf16_json::Utf16JsonProjection,
    pub(crate) usage: Option<ModTurnUsage>,
    pub(crate) refusal: Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    pub(crate) error: bool,
}

pub(crate) struct ModTurnUsage {
    pub(crate) tokens: lingxi_core::token::Usage,
    pub(crate) model: String,
}

/// Shared context for ordered main-loop Mod session events.
#[derive(Clone)]
pub(crate) struct ModSessionEventContext {
    pub(crate) host: Arc<hooks::mods::ModHost>,
    pub(crate) session: Arc<dyn hooks::mods::ModSessionContext>,
    pub(crate) output: Arc<dyn OutputStream>,
}

/// Jobs owned by the main-loop Mod session-event FIFO.
pub(crate) enum ModSessionEventWork {
    TurnComplete {
        context: ModSessionEventContext,
        input: lingxi_core::types::utf16_json::Utf16JsonProjection,
        original_answer: lingxi_core::types::utf16_json::Utf16JsonProjection,
    },
    Measure {
        context: ModSessionEventContext,
        sampler: Arc<mod_session_measure_sampler::ModSessionMeasureSampler>,
        request: mod_session_measure_sampler::ModSessionMeasureRequest,
    },
    MeasureRequest {
        context: ModSessionEventContext,
        sampler: Arc<mod_session_measure_sampler::ModSessionMeasureSampler>,
        reason: mod_session_measure_sampler::ModSessionMeasureReason,
        snapshot: ModSessionMeasureSnapshot,
    },
}

/// Current facts that can be sent to `session.measure` from this host.
#[derive(Clone)]
pub(crate) struct ModSessionMeasureSnapshot {
    pub(crate) input: serde_json::Value,
    pub(crate) cost_usd: Option<f64>,
    pub(crate) limit_status: Option<String>,
}

/// Live sandbox facts used to build the main-loop model-facing attachment.
/// Hosts supply the effective runtime config and backend availability from
/// the same context their Bash tool uses.
#[derive(Clone, Debug)]
pub struct NativeSandboxHostFacts {
    /// Whether this host can currently execute commands inside the sandbox.
    pub sandbox_available: bool,
    /// Effective filesystem read values from the live sandbox context, with
    /// Native's `denyOnly` / `allowWithinDeny` property names and source order.
    pub filesystem_read: lingxi_core::types::utf16_json::Utf16JsonProjection,
    /// Effective filesystem write values from the live sandbox context, with
    /// Native's `allowOnly` / `denyWithinAllow` property names and source order.
    pub filesystem_write: lingxi_core::types::utf16_json::Utf16JsonProjection,
    /// Network restriction config returned by the live sandbox host, if present.
    pub network_restriction_config: Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    /// Unix sockets returned by the live sandbox host, if present.
    pub allow_unix_sockets: Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    /// Ignore-violation map returned by the live sandbox host, if present.
    pub ignore_violations: Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    /// Effective raw `sandbox` settings projection, retained for exact source
    /// strings that the typed runtime config cannot represent.
    pub sandbox_settings: Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    /// Whether the current live config permits the unsandboxed-command parameter.
    pub allow_unsandboxed_commands: bool,
    /// Whether the current live config allows Apple Events.
    pub allow_apple_events: bool,
    /// Whether the current live config allows local port binding.
    pub allow_local_binding: bool,
    /// Detected Native platform label (`macos`, `linux`, or `wsl`), when known.
    pub platform: Option<String>,
    /// Whether the current main loop uses Native's auto permission mode.
    pub auto_mode: bool,
    /// Whether Native's current filesystem policy resolves to `relaxed`.
    pub filesystem_policy_relaxed: bool,
    /// The two Native Claude temp paths used by `getSimpleSandboxSection`.
    pub temp_paths: Vec<String>,
    /// Whether the `tengu_elegant_ocean` boundary-note branch is enabled.
    pub boundary_note_enabled: bool,
    /// Whether the host supports `registerCommandNetworkLists` for `wge()`.
    pub command_network_lists_offered: bool,
    /// Native's `qb()` result for whether `/sandbox` changes settings.
    pub sandbox_command_changes_settings: bool,
    /// Whether the current session is interactive (`!ve()`).
    pub interactive_session: bool,
    /// Whether `CLAUDE_CODE_SESSION_KIND` is defined, including an empty value.
    pub session_kind_present: bool,
    /// All source-preserving settings tiers in their Native merge order.
    pub settings_layers: Vec<lingxi_core::settings::SettingsRawLayer>,
}

pub use native_policy_attachments::{
    NativeBashFirstSessionAssignment, NativePolicyHostFacts, NativeProactivityLevel,
    NativeProactivityResponse, NativeProactivitySnapshot, NativeProactivityState,
    NativeThinkingResumptionHostFacts,
};

pub struct ConversationOrchestrator {
    /// Lightweight child state never publishes process-wide session flags.
    pub(crate) side_chat: bool,
    pub(crate) side_chat_model_refs: Option<Vec<String>>,
    pub(crate) config: OrchestratorConfig,
    pub(crate) native_sandbox_host_facts_provider:
        Option<Arc<dyn Fn() -> NativeSandboxHostFacts + Send + Sync>>,
    pub(crate) native_policy_host_facts_provider:
        Option<Arc<dyn Fn() -> NativePolicyHostFacts + Send + Sync>>,
    pub(crate) native_proactivity_state: NativeProactivityState,
    /// Whether this host request path has Native's `options.refreshTools`
    /// callback. `None` means the composition root has no source-backed fact.
    pub(crate) native_tool_pool_refresh_callback_available: Option<bool>,
    pub(crate) api: Arc<dyn OrchestratorApiClient>,
    /// Streaming-path API client. Wired by `new_with_streaming`; the
    /// legacy `new` constructor wires a [`NoStreamingApiClient`] stub
    /// that always errors. Both methods share `self.session` so a
    /// caller can mix batched and streaming turns transparently.
    pub(crate) streaming_api: Arc<dyn StreamingApiClient>,
    pub(crate) tools: Arc<ToolRegistry>,
    pub(crate) computer_runtime: crate::native_computer::ComputerRuntime,
    pub(crate) tool_execution_journal: Option<Arc<dyn lingxi_core::host::ToolExecutionJournal>>,
    pub(crate) hooks: Arc<HookExecutor>, // = hooks::HookExecutorImpl (M5-06)
    /// Exact enforced tool and shared budget handles inherited by Agent hooks.
    pub(crate) hook_agent_inheritance: Option<lingxi_core::host::SubagentInheritance>,
    pub(crate) perms: Arc<dyn PermissionGate>,
    pub(crate) output: Arc<dyn OutputStream>,
    pub(crate) session: Arc<Mutex<SessionState>>,
    /// Both the REPL and the shared host shutdown barrier can end a session.
    pub(crate) session_end_fired: Mutex<HashSet<String>>,
    /// Current main-loop Mod lifecycle record, protected by `turn_gate`.
    pub(crate) mod_turn: std::sync::Mutex<Option<ModTurnFacts>>,
    /// FIFO queue for settled main-loop `turn.complete` and `session.measure`
    /// dispatches. The receiver drains after the sender drops; no turn or
    /// shutdown path waits for a slow Mod while holding this queue's lock.
    pub(crate) mod_session_event_sender:
        std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<ModSessionEventWork>>>,
    /// Coalesces rate-limit and settled-turn samples for `session.measure`.
    pub(crate) mod_session_measure_sampler:
        Arc<mod_session_measure_sampler::ModSessionMeasureSampler>,
    /// Last fullscreen text selection for `$.ui.selection()`. Surface-owned
    /// ephemeral data: it must never enter persisted conversation state.
    pub(crate) mod_ui_selection: std::sync::Mutex<Option<lingxi_core::host::ModUiSelection>>,
    /// Live remote UI attachments exposed by `$.session.surfaces()`. The
    /// composition root shares this Arc with its desktop runtime and bridge.
    pub(crate) mod_surface_roster: std::sync::Arc<crate::mod_surface_roster::ModSurfaceRoster>,
    /// Host settings sources, resolved with the same launch scope as execution.
    pub(crate) mod_settings_reader: Option<Arc<dyn hooks::mods::ModSettingsReader>>,
    /// Live slash-command view for `$.command.list` without a command-api
    /// dependency from the orchestrator crate.
    pub(crate) mod_command_catalog: Option<Arc<dyn hooks::mods::ModCommandCatalog>>,
    /// Owns cleanup of process-global invoked-skill rows even when a host drops
    /// or rebuilds this orchestrator without an explicit SessionEnd callback.
    pub(crate) invoked_skill_session_guard: compaction::invoked_skills::InvokedSkillSessionGuard,
    /// Weak self-owner installed after the platform composition root selects
    /// the final Arc. Autonomous streaming tool jobs upgrade this only for the
    /// lifetime of their dispatch; the orchestrator does not own those jobs.
    pub(crate) streaming_tool_dispatch_owner: std::sync::OnceLock<std::sync::Weak<Self>>,
    /// Serializes user, queued, and async-hook re-wake turns. A background hook
    /// may finish while a user turn is still streaming; waiting here makes its
    /// re-wake the next turn instead of racing two model loops over one history.
    pub(crate) turn_gate: Arc<Mutex<()>>,
    /// Recipient-owned peer report queue. Admission never acquires `turn_gate`;
    /// preparation persists each accepted report before acknowledging consumption.
    pub(crate) main_reports: main_reports_impl::MainReportInbox,
    /// Serializes model-switch hooks without blocking on a running turn.
    pub(crate) model_switch_gate: Mutex<()>,
    /// Model selection, accounting, fallback, and request preparation state.
    pub(crate) model_runtime: ModelRuntime,
    /// Local host opt-in; default false and never inferred from remote policy.
    pub(crate) idle_compaction_enabled: bool,
    /// Owned per-session idle timer and cancellation boundary.
    pub(crate) idle_compaction: idle_compaction_impl::IdleCompactionState,
    /// Session-owned dynamic-workflow gate shared with the Workflow tool and
    /// TUI `/config` consumers.
    pub(crate) dynamic_workflows_gate: lingxi_core::host::session_flags::DynamicWorkflowsGate,
    /// Session-owned workflow-size setting shared with the Workflow tool and
    /// TUI `/config` consumers.
    pub(crate) workflow_size_guideline:
        lingxi_core::host::session_flags::WorkflowSizeGuidelineState,
    /// `queryTracking.chainId` for analytics (claude-code `query.ts:347-358`): a
    /// random uuid grouping a query chain, stamped onto the `queryChainId` field
    /// of `tengu_query_error` / `tengu_auto_compact_*` events. In claude-code a
    /// subagent INHERITS the parent's chainId and increments `depth`; in this
    /// port subagents never run through `ConversationOrchestrator` (the `agent`
    /// crate is a separate path), so every orchestrator IS a top-level chain —
    /// `queryDepth` is always 0 and each orchestrator owns one fresh chainId.
    /// The byte value is a host-minted uuid (shape-parity only — never matches
    /// the binary's per-run uuid).
    pub(crate) query_chain_id: String,
    /// LINGXI.md hierarchy provider (M5-03). The orchestrator calls
    /// `memory.load(&cwd).await` once per `run_turn` to gather the
    /// memory files spliced into the system prompt.
    pub(crate) memory: Arc<dyn crate::prompt::MemoryHierarchyProvider>,
    /// Working directory used as the root for the env + file-tree +
    /// git-status + memory probes inside `build_system_prompt`. M5-12
    /// CLI will plumb `--cwd`; until then, callers pass the platform
    /// caller's cwd here.
    pub(crate) cwd: std::path::PathBuf,
    /// The CURRENT working directory — the session-init `cwd` by default, but
    /// MUTATED when a `cd` inside a Bash call moves the persistent shell cwd
    /// (the desktop composition root shares this exact `Arc` with the
    /// [`crate::OrchestratorCwdChangedFirer`], which writes the new path on every
    /// `CwdChanged` fire). Hook payloads read THIS (not the static `cwd`) so a
    /// PreToolUse/PostToolUse/lifecycle hook sees the post-`cd` directory — 1:1
    /// with claude-code, where every hook reads the single global `getCwd()`
    /// that `cd` mutates (`Shell.ts:409` `setCwdState` → `cwd.ts:19` `getCwd`).
    /// Defaults to a private `Arc` over `cwd` (no firer wired ⇒ never moves ⇒
    /// hooks read the static cwd exactly as before).
    pub(crate) current_cwd: Arc<std::sync::Mutex<std::path::PathBuf>>,
    /// Task 5 (worktree 206 session-cwd plumbing): the SAME switchable cwd
    /// cell the tool layer swaps on `EnterWorktree`/`ExitWorktree`
    /// ([`tool_api::SessionCwd`], Task 1). The system prompt's `# Environment`
    /// `Primary working directory:` line, its trailing gitStatus block, and the
    /// per-turn `additional_context_message`/memory-prefetch cwd all read
    /// THIS (via [`Self::build_prompt_context`] et al.), so they re-derive from
    /// the post-swap worktree instead of the frozen boot `cwd` above.
    ///
    /// Defaults to a private, never-swapped `SessionCwd` over the constructor's
    /// `cwd` (see [`ConversationOrchestrator::new_with_streaming`]), so a caller
    /// that never wires [`Self::with_session_cwd`] behaves exactly as before —
    /// the INERT INVARIANT this plan depends on. Wired at the desktop/mobile
    /// composition roots to the SAME `Arc` handed to `BuiltinToolContext`.
    pub(crate) session_cwd: Arc<tool_api::SessionCwd>,
    /// Guest→host hop for prompt probes when the session cwd is a
    /// mobile-linux guest path — see [`Self::with_prompt_probe_cwd_resolver`].
    /// `None` everywhere but the mobile host.
    pub(crate) prompt_probe_cwd_resolver:
        Option<Arc<dyn Fn(&std::path::Path) -> std::path::PathBuf + Send + Sync>>,
    /// Fixed, engine-owned mobile runtime reminder prepended to real main-loop
    /// model requests. The message is rendered once when the mobile composition
    /// root wires it, then cloned with the same id and bytes for every retry and
    /// turn. It stays outside the system prompt so an explicit system-prompt
    /// override remains byte-exact, and outside session history so it is never
    /// persisted or duplicated by resume/compaction.
    pub(crate) mobile_runtime_environment_message: Option<ConversationMessage>,
    /// Typed mobile environment retained so the mutable guest cwd can be
    /// rendered per request as a separate second message. Stable host/tool
    /// facts remain frozen in `mobile_runtime_environment_message`.
    pub(crate) mobile_runtime_environment:
        Option<lingxi_core::host::mobile_runtime_environment::MobileRuntimeEnvironment>,
    /// Optional mobile host-path to guest-path mapping for live cwd updates.
    pub(crate) mobile_workspace_cwd_resolver:
        Option<Arc<dyn Fn(&std::path::Path) -> Option<String> + Send + Sync>>,
    /// Resolved `$LINGXI_CONFIG_DIR ?? ~/.claude` dir (the claude-home root).
    /// Used by [`Self::computed_transcript_path`] to deterministically derive the
    /// session's transcript path (`<config_home>/projects/<sanitize(cwd)>/<uuid>.jsonl`,
    /// = claude-code `getTranscriptPathForSession`) for hook payloads when no
    /// `jsonl_writer` is wired — which is the PRODUCTION case (every
    /// `with_jsonl_writer` call site is a test). `None` for library/test callers
    /// that wire neither a writer nor a config home, in which case
    /// `computed_transcript_path` returns an empty path (the prior `""` behavior).
    /// Wired at the composition root via [`Self::with_config_home`].
    pub(crate) config_home: Option<std::path::PathBuf>,
    /// `/goal` workspace-trust gate.
    pub(crate) workspace_trusted: bool,
    /// `/goal` hook-policy gate.
    pub(crate) hooks_restricted: bool,
    /// Transcript persistence, tool-result metadata, and ordered attachment state.
    pub(crate) transcript: TranscriptStore,
    /// Model-input assembly, reminder, and prompt cache state.
    pub(crate) prompt_runtime: PromptRuntime,
    /// Set by [`lingxi_core::host::OrchestratorHandle::request_exit`] (M5-10).
    /// The REPL (M5-13) checks this flag at the start of each iteration
    /// and breaks the loop. Wraps `AtomicBool` so reads are lock-free.
    /// Once `true`, this flag is never cleared (idempotent `/exit`).
    pub(crate) should_exit: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Session-scoped fast-mode toggle (`/fast`). Shared (same `Arc`) with the
    /// request-building `ProviderApiAdapter` (via [`Self::with_fast_mode`]), so
    /// flipping it via the handle's `set_fast_mode` makes the next turn send
    /// `speed:"fast"` when the active model supports it. Wraps `AtomicBool` so
    /// reads are lock-free (mirrors `should_exit`). Defaults to a private
    /// always-`false` flag until the composition root shares one with the
    /// adapter.
    pub(crate) fast_mode: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Lifecycle hooks, agent selection, and goal-check state.
    pub(crate) lifecycle_runtime: LifecycleRuntime,
    /// MCP registry (M2-02b). `None` when not wired — `list_mcp_servers`
    /// then returns `vec![]`. The CLI binary (M6-07 init.rs) populates
    /// this from `.mcp.json` + `~/.config/lingxi/mcp.json`.
    pub(crate) mcp_registry: Option<Arc<mcp::McpRegistry>>,
    /// Live task state for Mod agent enumeration and completion waits.
    pub(crate) task_registry: Option<Arc<dyn lingxi_core::host::task_registry::TaskRegistryHandle>>,
    /// The same live name store used by the Agent tool and its spawner.
    pub(crate) mod_agent_name_registry:
        Option<Arc<dyn lingxi_core::host::agent_name_registry::AgentNameRegistry>>,
    /// Host route authority shared with Agent model selection.
    pub(crate) model_resolution_context_provider:
        Option<Arc<dyn agent::model_resolution::ModelResolutionContextProvider>>,
    /// Provider-neutral local IDE lifecycle handle. `None` for hosts that do
    /// not expose a local endpoint inventory (mobile/embedded callers).
    pub(crate) ide_handle: Option<Arc<dyn lingxi_core::host::IdeHandle>>,
    /// Compaction engines, token ledgers, and extraction state.
    pub(crate) compaction_runtime: CompactionRuntime,
    /// `/fork` background-agent spawner. When wired (via
    /// [`Self::with_fork_spawner`], the composition root's
    /// `BackgroundAgentSpawner`), [`OrchestratorHandle::fork_conversation`]
    /// spawns a detached background agent that inherits the conversation.
    /// `None` (tests / non-desktop roots) ⇒ `fork_conversation` fails with a
    /// clear `ActionFailed` rather than panicking. Mirrors the existing
    /// `with_compaction` / `with_cache_safe_slot` Option-field pattern.
    pub(crate) fork_spawner: Option<Arc<dyn lingxi_core::host::subagent_spawn::SubagentSpawner>>,
    /// `/fork` budget enforcer inherited by the spawned background agent
    /// (`SubagentInheritance::budget`). Wired via [`Self::with_fork_budget`].
    /// `None` ⇒ `fork_conversation` fails gracefully.
    pub(crate) fork_budget: Option<Arc<dyn lingxi_core::host::budget::BudgetEnforcerHandle>>,
    /// 2.1.212 `/fork` (`vAd`) background-session forker. When wired (via
    /// [`Self::with_bg_session_forker`], the CLI composition root's
    /// `CliBgSessionForker`), [`OrchestratorHandle::fork_to_background_session`]
    /// snapshots the live conversation into a NEW background session (the
    /// `--bg`/daemon session-copy path) and returns the system line for the live
    /// session. `None` (tests / non-desktop roots) ⇒ that handle method fails
    /// with a clear `ActionFailed`. Mirrors the `fork_spawner`/`fork_budget`
    /// optional-seam pattern above.
    pub(crate) bg_session_forker:
        Option<Arc<dyn lingxi_core::host::bg_session_forker::BgSessionForker>>,
    /// Host-owned live catalog reconciler used by `register_repo_root`.
    ///
    /// The orchestrator admits the root into the sandbox and MCP root set
    /// first; the desktop composition root then refreshes the registries it
    /// exclusively owns.
    pub(crate) repo_root_reloader: Option<Arc<dyn lingxi_core::host::RepoRootReloader>>,
    /// `/recap` side-query runner — the SAME single-turn
    /// [`sidequery::ForkedAgentRunner`] the autocompact summarizer uses (cloned
    /// from the composition root's `forked_runner` before it moves into the
    /// `Autocompactor`), so recap replays the same cache-safe prefix. Wired via
    /// [`Self::with_recap_runner`]. `None` ⇒ [`OrchestratorHandle::generate_recap`]
    /// fails gracefully. Read-only: recap NEVER writes history/slot (skipTranscript
    /// / skipCacheWrite), unlike `force_compact`.
    pub(crate) recap_runner: Option<Arc<sidequery::ForkedAgentRunner>>,
    /// (`/rewind`) Shared file-history checkpoint store. The SAME
    /// `Arc<session::FileHistory>` the CLI holds (for restore + picker rows). The
    /// turn loop calls `make_snapshot` once per user turn and hands each tool a
    /// `FileHistorySink` view via `ToolUseContext.file_history` so pre-edit
    /// content is backed up. `None` ⇒ no checkpointing (edits untracked).
    pub(crate) file_history: Option<Arc<session::FileHistory>>,
    /// Forced permission decisions keyed by `tool_use_id`, consulted ONCE
    /// (removed on read) by the permission gate in
    /// [`crate::turn_loop::dispatch_tool_uses_tracked`]. Populated transiently by
    /// [`Self::run_orphaned_permission`] right before it re-dispatches an
    /// orphaned tool so the recovered `control_response` decision REPLACES the
    /// interactive gate — twin of claude-code's forced `canUseTool` in
    /// `handleOrphanedPermission` (`queryHelpers.ts:278-284`). Empty on every
    /// normal turn → the gate's behaviour (and the byte-locked turn-loop
    /// fixtures) are unchanged.
    pub(crate) orphan_forced_decisions: Mutex<
        std::collections::HashMap<
            lingxi_core::types::ToolUseId,
            crate::test_support::PermissionDecision,
        >,
    >,
    /// Mid-turn drain seam: source of queued user input to inject WITHIN a
    /// running streaming turn (claude-code's query.ts mid-turn injection,
    /// ~1570-1580). Empty ⇒ the streaming loop's mid-turn drain is a strict
    /// no-op (the default — keeps the locked fixtures byte-identical). Wired at
    /// the composition root from the `MessageQueueManager` via a msgqueue-backed
    /// adapter, so the orchestrator keeps NO dependency on `msgqueue`. A
    /// [`std::sync::OnceLock`] so it can be set on `&self` AFTER the orchestrator
    /// is shared as an `Arc` (the bridge wires it post-build with its
    /// per-connection queue). See
    /// [`crate::prompt::mid_turn_input::MidTurnInputSource`].
    pub(crate) mid_turn_input:
        std::sync::OnceLock<Arc<dyn crate::prompt::mid_turn_input::MidTurnInputSource>>,
    /// Abort-reason flag shared with the queue adapter so the streaming loop can
    /// distinguish a `Now`-command abort from a user Ctrl+C/ESC interrupt at the
    /// cancel-check points. Unset ⇒ every abort is treated as a user interrupt
    /// (today's behavior). Wired alongside [`Self::mid_turn_input`] at the
    /// composition root. See [`crate::prompt::mid_turn_input::CancelReasonFlag`].
    pub(crate) cancel_reason: std::sync::OnceLock<crate::prompt::mid_turn_input::CancelReasonFlag>,
    /// EndConversation (2.1.206) end-request slot, shared with the
    /// [`crate::end_conversation_tool::EndConversationTool`]: raised by the
    /// tool's 2nd consecutive call, read (and consumed) by the turn loop after
    /// tool execution to terminate the conversation. `None` when the feature is
    /// disabled (default) → the turn loop never checks it → byte-identical.
    pub(crate) end_conversation_slot: Option<crate::end_conversation_tool::EndConversationSlot>,
    /// `/loop` dynamic mode: raised by `ScheduleWakeup` when a call ARMS a
    /// wakeup, read (and consumed) by the turn loop so a round whose only tool
    /// call was that one ends the turn instead of feeding the result back.
    /// `None` on hosts that wire no `ScheduleWakeup` seam → the branch never
    /// fires → byte-identical.
    pub(crate) loop_wakeup_armed_slot: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Per-turn tallies the `/loop` no-op fold reads at the turn-completion
    /// edge (see [`crate::turn_span`]). Always present — the counters cost a
    /// relaxed add per turn and nothing reads them unless a fold is pending.
    pub(crate) turn_span: crate::turn_span::TurnSpanTally,
    /// Per-instance test seam avoids mutating process-global simple-mode flags.
    #[cfg(test)]
    pub(crate) coordinator_simple_mode_override: Option<bool>,
    #[cfg(test)]
    pub(crate) coordinator_pool_override: Option<(bool, Vec<String>)>,
    /// Tool-wide denial snapshot captured when assembling the current wire pool.
    pub(crate) tool_pool_denied_names: std::sync::RwLock<Vec<String>>,
    /// Main-agent policy names from the current tool assembly, before coordinator filtering.
    pub(crate) main_agent_tool_names: std::sync::RwLock<Option<std::collections::HashSet<String>>>,
    /// Live coordinator-mode flag (`Ci()`). `None` is an ordinary session, so
    /// unknown-tool `Ldt` never takes the coordinator `Y7e` arm.
    pub(crate) coordinator_mode:
        Option<std::sync::Arc<dyn lingxi_core::host::coordinator_mode::CoordinatorModeHandle>>,
}

impl ConversationOrchestrator {
    /// Put a finalized orchestrator behind its production Arc owner and bind
    /// the weak back-reference required by owned streaming dispatch.
    pub fn into_shared(this: Self) -> Arc<Self> {
        let shared = Arc::new(this);
        Self::bind_streaming_tool_dispatch_owner(&shared);
        let session_id = shared
            .session
            .try_lock()
            .expect("finalized orchestrator binds before turn publication")
            .session_id;
        shared.bind_subagent_stop_hook_owner(session_id);
        shared
    }

    /// Bind the finalized composition-root Arc used by autonomous streaming
    /// tool dispatch. Call this immediately after constructing the production
    /// Arc, before publishing the orchestrator to any turn caller.
    pub fn bind_streaming_tool_dispatch_owner(this: &Arc<Self>) {
        let candidate = Arc::downgrade(this);
        let installed = this
            .streaming_tool_dispatch_owner
            .get_or_init(|| candidate.clone());
        assert!(
            installed.ptr_eq(&candidate),
            "ConversationOrchestrator was already bound to a different owner"
        );
    }

    /// Upgrade the required production owner for a `'static` scheduler job.
    /// The pointer check prevents a builder clone from dispatching through a
    /// different session's Arc.
    pub(crate) fn upgrade_streaming_tool_dispatch_owner(&self) -> Option<Arc<Self>> {
        let owner = self.streaming_tool_dispatch_owner.get()?.upgrade()?;
        std::ptr::eq(self, Arc::as_ptr(&owner)).then_some(owner)
    }

    /// Share this orchestrator's live UI attachment roster with its host.
    #[must_use]
    pub fn mod_surface_roster(
        &self,
    ) -> std::sync::Arc<crate::mod_surface_roster::ModSurfaceRoster> {
        self.mod_surface_roster.clone()
    }

    /// Return this conversation's current request-recovery scope for Native
    /// tool-search reminder state. The clone shares the session-owned latches;
    /// request preparation reads it without borrowing or mutating the store.
    pub(crate) fn native_tool_search_scope_snapshot(
        &self,
    ) -> llm_runtime::thinking_scope::ThinkingRecoveryScope {
        self.transcript
            .thinking_recovery
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Native `pfr` treats `options.refreshTools !== undefined` as an input
    /// fact. Keep it distinct from the existence of the shared ToolRegistry.
    pub(crate) fn native_tool_pool_refresh_callback_available(&self) -> Option<bool> {
        self.native_tool_pool_refresh_callback_available
    }
}

// Responsibility-focused implementation modules. `conversation.rs` owns the
// public façade and shared state shape; behavior lives in these child modules.
#[path = "conversation/attachment_order.rs"]
mod attachment_order;
pub(crate) mod closing_placement;
#[path = "conversation/command_describe.rs"]
mod command_describe_impl;
#[path = "conversation/compaction.rs"]
mod compaction_impl;
mod task_delivery;
pub(crate) use compaction_impl::{
    SessionCompactCore, SessionCompactCoreOutput, SessionCompactDecision,
};
#[path = "conversation/drivers/mod.rs"]
mod drivers_impl;
pub use drivers_impl::{
    realtime_history, skill_prompt_inputs, PendingPromptIdentity, QueuedPromptInput,
    RealtimeAgentContext, RealtimeAgentEnd, RealtimeAgentInput, RealtimeAgentLimits,
};
#[path = "conversation/context_announcements.rs"]
pub(crate) mod context_announcements_impl;
#[path = "conversation/goal_retry.rs"]
mod goal_retry_impl;
#[path = "conversation/hooks.rs"]
mod hooks_impl;
#[path = "conversation/main_reports.rs"]
pub(crate) mod main_reports_impl;
#[path = "conversation/mod_projects_consent.rs"]
mod mod_projects_consent;
#[path = "conversation/mod_session_measure_sampler.rs"]
mod mod_session_measure_sampler;
#[path = "conversation/model.rs"]
mod model_impl;
#[path = "conversation/model_reminders.rs"]
mod model_reminders_impl;
#[path = "conversation/native_classifier_context.rs"]
pub(crate) mod native_classifier_context;
#[path = "conversation/native_policy_attachments.rs"]
mod native_policy_attachments;
#[path = "conversation/native_sandbox_attachments.rs"]
mod native_sandbox_attachments;
#[path = "conversation/native_settings_attachments.rs"]
mod native_settings_attachments;
#[path = "conversation/prompt_suggestion.rs"]
mod prompt_suggestion;
#[path = "conversation/prompt_suggestion_background.rs"]
mod prompt_suggestion_background;
pub(crate) use context_announcements_impl::PreparedContextAnnouncements;
#[path = "conversation/prompt_cache.rs"]
mod prompt_cache_impl;
#[path = "conversation/prompt.rs"]
mod prompt_impl;
#[path = "conversation/prompt_ingress_environment.rs"]
mod prompt_ingress_environment;
#[path = "conversation/reminders.rs"]
mod reminders_impl;
#[path = "conversation/tooling.rs"]
mod tooling_impl;
#[path = "conversation/transcript.rs"]
mod transcript_impl;
pub use transcript_impl::ScheduledLoopFire;
pub(crate) use transcript_impl::{
    active_mod_result_stage_is_virtual, with_mod_result_stage, with_virtual_mod_result_stage,
    ModResultStage,
};
#[path = "conversation/wiring.rs"]
mod wiring_impl;

#[path = "conversation/output_accounting.rs"]
mod output_accounting_impl;
#[path = "conversation/runtime.rs"]
mod runtime_impl;

#[path = "conversation/headless_mcp.rs"]
mod headless_mcp;
#[path = "conversation/headless_ui.rs"]
mod headless_ui;
#[path = "conversation/idle_compaction.rs"]
mod idle_compaction_impl;

use drivers_impl::parse_generated_session_name;
pub(crate) use runtime_impl::TurnTimingCapture;
use runtime_impl::{
    camelize_json_keys, compact_file_reference_body, extend_session_memory_fork_context,
    find_unresolved_tool_use_in_history, CompactionRuntime, LifecycleRuntime, ModelRuntime,
    PromptRuntime, SessionMemoryInFlightReset, TranscriptStore,
};
pub use runtime_impl::{
    CostSessionSwitcher, PreparedSessionSwitch, SessionActivationObserver, SessionMemoryHandle,
    TurnExecutionMetrics,
};

/// Internal no-op streaming client used by [`ConversationOrchestrator::new`]
/// when the caller doesn't supply a streaming transport. Every call to
/// `stream` returns `LlmError::Transport("no streaming client configured")`.
/// Wired in Task 12 when the legacy `new()` constructor delegates to
/// `new_with_streaming(..., NoStreamingApiClient, ...)`.
#[allow(dead_code)]
pub(crate) struct NoStreamingApiClient;

#[async_trait]
impl StreamingApiClient for NoStreamingApiClient {
    async fn stream(
        &self,
        _model: &str,
        _profile: Option<&str>,
        _system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        _messages: Vec<ConversationMessage>,
        _tools: Vec<llm_runtime::ToolDeclaration>,
        _query_source: &str,
        _skip_global_cache_for_system_prompt: bool,
        _request_dispatch_admission: Option<llm_runtime::RequestDispatchAdmission>,
    ) -> Result<futures::stream::BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        Err(LlmError::Transport {
            message: "no streaming client configured".into(),
        })
    }
}

#[path = "conversation/side_chat.rs"]
mod side_chat_impl;
pub use side_chat_impl::SideChatParentSnapshot;
