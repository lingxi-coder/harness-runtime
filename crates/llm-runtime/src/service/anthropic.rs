//! Host policy for Claude routes. The SDK owns request encoding and response facts.

pub(super) mod endpoint;
pub(super) mod fast;
pub(super) mod foundry;
pub(super) mod prompt;
pub(super) mod recovery;
pub(super) mod thinking;
pub(super) mod wire;

use super::{
    CacheEditingInputs, FoundryDeploymentKey, NativeThinkingResumptionRefusalLatch,
    NativeThinkingTypeKey, PromptCacheOverageState, PromptCacheTtlSettingsSource,
};
use std::sync::{Arc, Mutex};

pub(super) struct State {
    pub(super) fast_policy: crate::model::fast_admission::PolicySource,
    pub(super) fast_availability: Arc<crate::model::fast_admission::Availability>,
    pub(super) prompt_cache_ttl_settings_source: Option<PromptCacheTtlSettingsSource>,
    /// Native requestLatches.thinkingTypeOverrides, scoped to this service's
    /// lifetime and partitioned by selected route plus logical UTF-16 model.
    pub(super) native_thinking_type_overrides: Mutex<
        std::collections::HashMap<
            NativeThinkingTypeKey,
            lingxi_llm_client::protocol::NativeThinkingTypeRejection,
        >,
    >,
    /// Native QTe caches per-model adaptive capability observations including
    /// an absent observation. Route keys keep same-named models on distinct
    /// Harness providers from sharing cached capability state.
    pub(super) native_model_option_capabilities:
        Mutex<std::collections::HashMap<(NativeThinkingTypeKey, String), Option<bool>>>,
    /// Native requestLatches.thinkingResumptionRefused. Only the SDK's exact
    /// beta-specific HTTP 400 fact may set it, and it stays set for this
    /// ApiService lifetime.
    pub(super) native_thinking_resumption_refused: Arc<NativeThinkingResumptionRefusalLatch>,
    /// Identity for the Anthropic `metadata.user_id` field (claude-code
    /// `claude.ts:503-525`). `None` (the default) omits `metadata` entirely.
    /// Set via [`Self::with_request_metadata`]; the composition root supplies
    /// the composed identity string.
    pub(super) request_metadata: Option<crate::RequestMetadata>,
    /// 1P experimental cache-editing inputs (claude.ts `addCacheBreakpoints`
    /// `newCacheEdits`/`pinnedEdits`, claude.ts:3068-3069). LingXi has no
    /// cached-microcompact scheduler to produce these, so the default is
    /// `None`/empty — the gate-armed `cache_reference`-on-tool_results pass
    /// (the directly-exercised behavior) still runs from `req.input.messages`. Set
    /// only by [`Self::with_cache_editing_inputs`] (test-only today); wiring a
    /// real producer is residual. See module note.
    pub(super) cache_editing_inputs: CacheEditingInputs,
    /// Explicit client wire policy for admitted first-party Anthropic routes.
    pub(super) anthropic_client_metadata:
        Option<lingxi_llm_client::providers::anthropic::request_policy::AnthropicClientMetadata>,
    /// Exact Native Anthropic endpoint selected by the host. This controls the
    /// Native HTTP/1 layout independently from Native's process-environment
    /// request-id facts, which are resolved later by the SDK authenticator.
    pub(super) native_anthropic_api_base_url: Option<String>,
    /// Host-selected compatibility version for Anthropic-family user-agent
    /// construction. This is immutable service configuration, not request JSON.
    pub(super) anthropic_compatible_version: Option<String>,
    /// Host-selected Native display behavior, copied into each admitted main
    /// request's execution context.
    pub(super) native_thinking_display:
        Option<lingxi_llm_client::providers::anthropic::thinking_display::ThinkingDisplayPolicy>,
    /// Host-selected Native system-prefix policy, captured in each main
    /// request's prompt-cache context.
    pub(super) native_system_prefix:
        Option<lingxi_llm_client::providers::anthropic::system_prompt::NativeSystemPrefix>,
    /// Host-validated CLI beta additions for first-party Anthropic API-key
    /// message requests. Kept as explicit session state instead of a process
    /// environment variable so concurrent embedded runtimes cannot leak flags
    /// into one another.
    pub(super) custom_cli_betas: Vec<String>,
    /// Native `hQ` (`Ta(midConversationSystem, model, TF(model))`) memoized
    /// decisions. The service is host-scoped; each entry keeps the raw model
    /// key, partitioned by resolved provider/profile because one Harness
    /// service may route the same model string through multiple providers.
    /// Unsupported non-Anthropic routes are rejected before they reach this
    /// cache.
    pub(super) native_mid_conversation_system_cache:
        Mutex<std::collections::HashMap<(crate::ProviderId, String, String), bool>>,
    /// Scope-keyed Native overage facts and the auth generation they belong to.
    pub(super) prompt_cache_overage: Arc<PromptCacheOverageState>,
    /// Optional AWS auth-refresh driver (2.1.198 `ZBd`, `awsAuthRefresh`).
    ///
    /// When set, an AWS-auth failure (401/403) on the Bedrock provider runs
    /// the client-side refresh flow and retries the request, bounded at
    /// [`crate::auth::external_aws::AWS_AUTH_MAX_ATTEMPTS`] (`Ygf = 2`). `None` (the
    /// default) keeps every error path unchanged. Provider-gated inside
    /// [`crate::auth::external_aws::is_aws_auth_error`] — non-AWS providers never
    /// reach the refresh.
    pub(super) aws_auth: Option<Arc<dyn crate::auth::external_aws::AwsAuthRefresh>>,
    /// Native Foundry deployment capability observations. Only negative
    /// observations are stored; an unseen deployment/feature remains allowed.
    /// The key is the selected endpoint plus the Native model identity after
    /// removing `[1m]`/`[2m]` tags, matching `pl(e)`/`Fn(e)`.
    pub(super) foundry_capabilities:
        Mutex<std::collections::HashMap<FoundryDeploymentKey, std::collections::HashSet<String>>>,
}
