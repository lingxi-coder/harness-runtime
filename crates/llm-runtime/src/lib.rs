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
pub mod convert;
pub mod cost;
pub mod error;
mod execution;
mod execution_context;
pub use execution_context::ExecutionContext;
pub mod auth;
pub mod fusion_hints;
pub mod history;
mod history_projection;
mod history_usage;
pub mod model;
pub mod model_attempt;
pub mod prompt_format;
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
    stream_content_order, stream_provider_metadata_from_headers, validate_capabilities, LlmRequest,
    ProviderRequest, ProviderResponse, ProviderStreamTransport, ReasoningConfig, RequestMetadata,
};
pub use auth::external_aws::{
    AwsAuthProcess, AwsAuthRefresh, AwsAuthRefresher, AwsAuthSettings, ShellAwsAuthProcess,
};
pub use auth::provider::{
    CopilotExchangeCredentialProvider, Credential, CredentialProvider, CredentialScope,
    EnvCredentialProvider, StaticCredentialProvider,
};
pub use catalog::{builtin_presets, BuiltinCatalog};
pub use client::{
    FileActivationPoll, ModelRuntime, PreparedLlmCall, ResponsesSession,
    ResponsesWebSocketRequestSnapshot,
};
pub use cloud_provider_env::{
    bedrock_base_url_override, foundry_base_host, foundry_base_host_from_env,
    foundry_credential_from_env, foundry_messages_base_url, foundry_messages_base_url_from_env,
    select_foundry_credential, skip_bedrock_auth, skip_foundry_auth, skip_vertex_auth,
    small_fast_model_aws_region, vertex_base_host, vertex_base_host_url, vertex_codec_base_url,
    vertex_codec_base_url_from_env, vertex_default_region, vertex_region_env_var_for_model,
    vertex_region_for_model, vertex_region_for_model_from_env, FoundryCredential,
};
pub use config::{
    AuthStrategy, AzureConfig, Capabilities, ClientConfig, ConnectionSpec, CredentialConfig,
    FailoverTriggers, ModelProfile, PricingConfig, ProtocolFamily, ProviderProfile, SigningConfig,
};
pub use cost::{CostEstimator, PricingCatalog, PricingOverride, PricingPolicy};
pub use error::{
    api_error_detail, api_error_status, error_display_text, LlmError, MediaDelegationAccounting,
};
pub use fusion_hints::hints_for;
pub use lingxi_core::host::ModelBillingMode;
pub use lingxi_llm_client::framing::eventstream::{crc32, EventStreamMessage, EventStreamSplitter};
pub use lingxi_llm_client::protocol::TokenPricing;
pub use lingxi_llm_client::protocol::{
    ContinuationRef, HostedTool, NativeExtension, NativeType, OutputFormat, PromptCachePolicy,
    WebSearchConfig,
};
pub use lingxi_llm_client::protocol::{ServerToolUsage, Usage, UsageReport, UsageState};
pub use lingxi_llm_client::providers::google::files_wire::GeminiFile;
pub use lingxi_llm_client::SseFrameSplitter;
pub use model_attempt::{
    ModelAttemptHooks, ModelAttemptLease, ModelAttemptSettlement, ModelAttemptUsageCompleteness,
};
pub use provider_settings::{
    anthropic_model_profiles, anthropic_provider_profile, parse_provider_profiles_lenient,
    parse_provider_profiles_strict, pricing_provider_id_for_profile, split_profile_model,
    ParsedUserProvider, ProviderCredentialMode, ProviderKind, ProviderParseOptions,
};
pub use reasoning_controls::{
    apply_reasoning_selection, reasoning_control_spec, ReasoningControlSpec, ReasoningSelection,
    ReasoningTarget, TokenBudgetRange,
};
pub use redaction::Redactor;
pub use registry::{ConnectionHop, MediaRoute, ModelListing, ModelRegistry, ResolvedRoute};
pub use retry::{ResponseMetadata, RetryDecision, RetryPolicy};
pub use route::Route;
pub use service::{ApiService, RetryInfo, RetryReporter, SubscriberState};
pub use services::{ProviderServiceSnapshot, ProviderServices};
pub use ssl::{detect_ssl_code, is_ssl_code, ssl_hint};
pub use transport::{BoxFuture, Transport};
pub use types::{CostEstimate, ExecutionUsage, PricingModelRef, ProviderId};

tokio::task_local! {
    /// The running agent's `experimental.cacheTtl`, scoped by the agent runner
    /// around its turn (claude-code `agentCacheTtlOverride`).
    ///
    /// A task-local rather than a `build_request` parameter because
    /// `ApiService` is shared as an `Arc` across concurrent subagents — a field
    /// on the service would race. The runner awaits its round-trip inline, so
    /// the value propagates.
    ///
    /// ⛔ If a future change moves the request onto its own task, this silently
    /// reads `false` again. `agent_cache_ttl_1h_applies_through_the_real_path`
    /// is the assertion that would catch it.
    pub static AGENT_CACHE_TTL_1H: bool;
}

/// Read the running agent's 1h-TTL override; `false` outside any agent scope.
#[must_use]
pub fn agent_cache_ttl_1h_override() -> bool {
    AGENT_CACHE_TTL_1H.try_with(|v| *v).unwrap_or(false)
}

/// Run `future` with the agent's 1h-TTL override in scope.
///
/// Mirrors `thinking_scope::scope_thinking_recovery`'s shape so the runner
/// composes them the same way.
/// ⚠️ The inner future is BOXED. `run_subagent`'s future is already close to the
/// stack limit in debug builds; wrapping it in a task-local scope inline pushed
/// it over and overflowed the stack in existing runner tests. Boxing moves the
/// scoped future to the heap and keeps the frame flat.
pub async fn scope_agent_cache_ttl<F: std::future::Future>(wants_1h: bool, future: F) -> F::Output {
    let future = Box::pin(future);
    AGENT_CACHE_TTL_1H.scope(wants_1h, future).await
}

pub use history::{
    HistoryContentDelta, HistoryEvent, HistoryMessageDelta, HistoryResponse, HistoryStopDetails,
};

pub use history::{
    CacheControl, CacheEdit, CacheScope, ContentBlock, Message, ResponseFormat, SystemBlock,
    ToolChoice, ToolDeclaration,
};
pub mod computer;
pub use computer::{ComputerNativeDeclaration, ComputerRequestProjection, scope_computer_request};
pub use auth::provider::{
    AnthropicAuthSnapshot, CopilotExchangeCredentialProvider, Credential, CredentialProvider,
    CredentialScope, CredentialSource, EnvCredentialProvider, StaticCredentialProvider,
};
