//! Application LLM runtime backed by `lingxi-llm-client`.
//!
//! The shared client owns wire formats, request execution, streams, files,
//! model resolution and price arithmetic. This crate owns application policy,
//! credential lifecycle, platform adaptation, retry and durable-accounting hooks.

#![forbid(unsafe_code)]
// Documentation debt, not a decision that docs do not matter: this crate had
// 28 undocumented public item(s) when `missing_docs` was measured across the
// workspace (2026-09-16). The lint stays `warn` at the workspace level so a NEW
// crate still inherits the requirement; this allow is scoped here so the debt
// is visible per crate and can be repaid one crate at a time by deleting this
// line.
#![allow(missing_docs)]
// Dead code kept visible, not swept: this crate had 1 item(s) rustc could
// reach from nothing when the workspace was measured (2026-09-16). The lint
// stays `warn` at the workspace level so a NEW crate still inherits it; this
// allow is scoped here so the count is per crate and repayable by deleting this
// line. This is the category where "named, computed, never wired" hides — some
// of these read like features that were built and never connected. Each wants a
// decision (delete, or wire), not a blanket deletion.
// ⚠️ The count above is ONE macOS, lib-target measurement. It is not a list of
// deletable items — see docs/HANDOFF-dead-code-adjudication-2026-09-17.md,
// which records two near-misses where it said "dead" about live code.
#![allow(dead_code)]

extern crate self as llm_runtime;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub use lingxi_llm_client::protocol::Region;
pub use lingxi_llm_client::providers::anthropic::system_prompt::{PromptText, SystemPromptInput};
#[cfg(any(test, feature = "test-support"))]
pub use test_support::{
    FrameStream, RawStreamFrame, ResponsesWebSocketTransportSession, StreamingResponse,
};
mod attempt_pricing;
#[cfg(test)]
mod codec_tests;
pub use attempt_pricing::{AttemptPriceBounds, AttemptTokenRates};

pub mod catalog;
#[allow(missing_docs)]
pub mod client;
pub mod cloud_provider_env;
pub mod config;
pub mod computer;
pub use computer::{ComputerNativeDeclaration, ComputerRequestProjection, scope_computer_request};
pub mod convert;
pub mod cost;
mod dispatch_header;
pub mod error;
mod execution;
mod execution_context;
mod structured_output;
pub use execution_context::{ExecutionContext, PendingPromptCacheObservation, PromptCacheRequestContext, RequestDispatchAdmission};
pub mod auth;
pub mod fusion_hints;
pub mod history;
mod history_projection;
mod history_usage;
pub mod mod_turn_step;
pub mod model;
pub mod model_attempt;
pub mod prompt_cache;
pub mod protocol;
pub mod provider_settings;
pub mod reasoning_controls;
pub mod redaction;
pub mod registry;
pub mod retry;
#[allow(missing_docs)]
pub mod route;
#[allow(missing_docs)]
pub mod service;
pub mod services;
pub mod ssl;
pub mod stream_accumulator;
pub mod thinking_scope;
pub mod transport;
pub mod types;
pub mod unicode_repair;
mod upstream;

pub use crate::protocol::{
    LlmRequest, ProviderRequest, ProviderResponse, ProviderStreamTransport, ReasoningConfig,
    RequestMetadata, stream_content_order, stream_provider_metadata_from_headers,
    validate_capabilities,
};
pub use auth::external_aws::{
    AwsAuthProcess, AwsAuthRefresh, AwsAuthRefresher, AwsAuthSettings, ShellAwsAuthProcess,
};
pub use auth::provider::{
    AnthropicAuthSnapshot, CopilotExchangeCredentialProvider, Credential, CredentialProvider,
    CredentialScope, CredentialSource, EnvCredentialProvider, StaticCredentialProvider,
};
pub use catalog::{BuiltinCatalog, builtin_presets};
pub use client::{
    FileActivationPoll, ModelRuntime, PreparedLlmCall, ResponsesSession,
    ResponsesWebSocketRequestSnapshot,
};
pub use cloud_provider_env::{
    FoundryCredential, bedrock_base_url_override, foundry_base_host, foundry_base_host_from_env,
    foundry_credential_from_env, foundry_messages_base_url, foundry_messages_base_url_from_env,
    select_foundry_credential, skip_bedrock_auth, skip_foundry_auth, skip_vertex_auth,
    small_fast_model_aws_region, vertex_base_host, vertex_base_host_url, vertex_codec_base_url,
    vertex_codec_base_url_from_env, vertex_default_region, vertex_region_env_var_for_model,
    vertex_region_for_model, vertex_region_for_model_from_env,
};
pub use config::{
    AuthStrategy, AzureConfig, Capabilities, ClientConfig, ConnectionSpec, CredentialConfig,
    FailoverTriggers, ModelProfile, PricingConfig, ProtocolFamily, ProviderProfile, SigningConfig,
};
pub use cost::{CostEstimator, PricingCatalog, PricingOverride, PricingPolicy};
pub use error::{
    LlmError, MediaDelegationAccounting, api_error_detail, api_error_status, error_display_text,
};
pub use fusion_hints::hints_for;
pub use lingxi_core::host::ModelBillingMode;
pub use lingxi_llm_client::SseFrameSplitter;
pub use lingxi_llm_client::framing::eventstream::{EventStreamMessage, EventStreamSplitter, crc32};
pub use lingxi_llm_client::protocol::TokenPricing;
pub use lingxi_llm_client::protocol::{
    ContinuationRef, HostedTool, NativeExtension, NativeType, OutputFormat, PromptCachePolicy,
    WebSearchConfig,
};
pub use lingxi_llm_client::protocol::{ServerToolUsage, Usage, UsageReport, UsageState};
pub use lingxi_llm_client::providers::google::files_wire::GeminiFile;
pub use model_attempt::{
    ModelAttemptHooks, ModelAttemptLease, ModelAttemptSettlement, ModelAttemptUsageCompleteness,
};
pub use provider_settings::{
    ParsedUserProvider, ProviderCredentialMode, ProviderKind, ProviderParseOptions,
    anthropic_model_profiles, anthropic_provider_profile, parse_provider_profiles_lenient,
    parse_provider_profiles_strict, pricing_provider_id_for_profile, split_profile_model,
};
pub use reasoning_controls::{
    ReasoningControlSpec, ReasoningSelection, ReasoningTarget, TokenBudgetRange,
    apply_reasoning_selection, reasoning_control_spec,
};
pub use redaction::Redactor;
pub use registry::{ConnectionHop, MediaRoute, ModelListing, ModelRegistry, ResolvedRoute};
pub use retry::{ResponseMetadata, RetryDecision, RetryPolicy};
pub use route::Route;
pub use service::{
    ApiService, FallbackPolicy, MessagesCreateOptions, MessagesCreateRequest,
    NonStreamingRequestClass, NonStreamingRetryOptions, RetryInfo, RetryReporter, SubscriberState,
    with_mod_request_effort,
};
pub use services::{ProviderServiceSnapshot, ProviderServices};
pub use ssl::{detect_ssl_code, is_ssl_code, ssl_hint};
pub use transport::{BoxFuture, Transport};
pub use types::{CostEstimate, ExecutionUsage, PricingModelRef, ProviderId};

pub use lingxi_llm_client::providers::anthropic::system_prompt::{
    AgentPromptCacheTtlOverride, PromptCacheQuerySource,
};

tokio::task_local! {
    /// The running agent's exact `experimental.cacheTtl` frontmatter value.
    /// A task-local avoids races because `ApiService` is shared by concurrent
    /// agents; the inline runner keeps the override scoped to that agent.
    pub static AGENT_PROMPT_CACHE_TTL_OVERRIDE: Option<AgentPromptCacheTtlOverride>;
}

/// Read the running agent's explicit prompt-cache TTL, if present.
#[must_use]
pub fn agent_prompt_cache_ttl_override() -> Option<AgentPromptCacheTtlOverride> {
    AGENT_PROMPT_CACHE_TTL_OVERRIDE
        .try_with(|value| *value)
        .unwrap_or(None)
}

/// Run `future` with the agent's exact prompt-cache TTL override in scope.
///
/// The inner future is boxed because `run_subagent` is close to the debug
/// stack limit and a task-local scope inline previously overflowed runner tests.
pub async fn scope_agent_prompt_cache_ttl<F: std::future::Future>(
    override_value: Option<AgentPromptCacheTtlOverride>,
    future: F,
) -> F::Output {
    let future = Box::pin(future);
    AGENT_PROMPT_CACHE_TTL_OVERRIDE
        .scope(override_value, future)
        .await
}

pub use history::{
    HistoryContentDelta, HistoryEvent, HistoryMessageDelta, HistoryResponse, HistoryStopDetails,
};

pub use history::{
    CacheControl, CacheEdit, CacheScope, ContentBlock, Message, ResponseFormat, SystemBlock,
    ToolChoice, ToolDeclaration,
};
