//! Thin orchestrator-seam adapter over [`llm_runtime::ApiService`].
//!
//! The provider drive loop (prepare → header injection → `transport.execute()` /
//! `open_stream()` → `codec.decode_response()` + the retry/rate-limit/betas
//! machinery) lives in `llm_runtime::ApiService`. This module holds an
//! `Arc<ApiService>` and implements the orchestrator's seam traits
//! ([`OrchestratorApiClient`], [`StreamingApiClient`], [`agent::SubagentApiClient`])
//! by delegating each method 1:1 — the only orchestrator-domain logic kept here is
//! the catalog→`ModelListing` projection and the `RateLimitSnapshot` mapping.

use crate::conversation::{OrchestratorApiClient, StreamingApiClient};
use crate::model::rate_limit::{RateLimitInfo, RawUtilization};
use async_trait::async_trait;
use futures::stream::BoxStream;
use llm_runtime::{LlmError, LlmEvent, LlmResponse, MediaDelegationAccounting};
use protocol::ConversationMessage;
use std::sync::Arc;

/// Subscriber-state seed for [`llm_runtime::ApiService`]'s 429 gate. Re-exported
/// from llm-runtime so existing `orchestrator::provider_adapter::SubscriberState`
/// import paths (the composition roots) keep resolving after the drive loop moved.
pub use llm_runtime::SubscriberState;

/// Production adapter: a thin holder of [`llm_runtime::ApiService`] (which owns the
/// provider drive loop) that implements the orchestrator's seam traits.
pub struct ProviderApiAdapter {
    service: Arc<llm_runtime::ApiService>,
    /// (M4 cc2.1.198) The session's initial effort level from CLI `--effort`
    /// (binary: session state `thinkingConfig: SF(a.effort)` → request
    /// `output_config.effort`). `None` (the default) keeps main-loop request
    /// bodies byte-identical to before this field existed.
    initial_effort: std::sync::RwLock<Option<serde_json::Value>>,
    /// (`/fast`) Session-scoped fast-mode toggle, shared (same `Arc`) with the
    /// [`ConversationOrchestrator`] so the handle's `set_fast_mode` flip is seen
    /// here on the next turn. When set AND the active model carries the
    /// canonical `fast_mode` capability, the MAIN-loop stream sends
    /// `speed:"fast"`. The
    /// default flag is always `false`, so bodies stay byte-identical until a
    /// live `/fast` toggle flips it.
    fast_mode: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Whether `model` carries the canonical registry's `fast_mode` capability.
fn model_supports_fast_mode(model: &str) -> bool {
    platform_api::model_capabilities::has_capability(
        model,
        platform_api::model_capabilities::ModelCapability::FastMode,
    )
}

impl ProviderApiAdapter {
    /// Wrap a constructed [`llm_runtime::ApiService`]. The composition roots build
    /// the service via `ApiService::new_with_routing(...).with_*(...)` and hand the
    /// `Arc` here; every trait method delegates 1:1 to it.
    #[must_use]
    pub fn new(service: Arc<llm_runtime::ApiService>) -> Self {
        Self {
            service,
            initial_effort: std::sync::RwLock::new(None),
            fast_mode: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// (`/fast`) Share the session's fast-mode flag with this adapter (the same
    /// `Arc<AtomicBool>` the [`ConversationOrchestrator`] holds). When the flag
    /// is set and the active model supports fast mode, the MAIN-loop stream
    /// carries `speed:"fast"`. Without this the flag is a private always-`false`
    /// default, so bodies are byte-identical.
    #[must_use]
    pub fn with_fast_mode(mut self, flag: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.fast_mode = flag;
        self
    }

    /// (M4 cc2.1.198) Set the session's initial effort (CLI `--effort`,
    /// already validated/normalized by the CLI to one of
    /// low/medium/high/xhigh/max). The MAIN-loop [`StreamingApiClient::stream`]
    /// impl then carries it as `output_config.effort` (the service adds the
    /// `effort-2025-11-24` beta whenever the body has effort). Subagent calls
    /// keep their own per-spawn effort resolution and are unaffected.
    #[must_use]
    pub fn with_initial_effort(mut self, effort: Option<serde_json::Value>) -> Self {
        self.initial_effort = std::sync::RwLock::new(effort);
        self
    }

    fn current_effort(&self) -> Option<serde_json::Value> {
        self.initial_effort
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn build_scheduled_request(
        &self,
        settings: crate::scheduled_turn::ScheduledSettings,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
        max_tokens: Option<u32>,
    ) -> Result<llm_runtime::LlmRequest, LlmError> {
        self.service.build_side_query_request_with_thinking(
            &settings.model,
            Some(&settings.provider),
            system,
            messages,
            tools,
            max_tokens,
            None,
            Vec::new(),
            Some(settings.thinking),
            settings.effort,
            None,
            Some("scheduled_task"),
        )
    }

    /// Return the most recently observed rate-limit header snapshot (the internal
    /// nine-field [`RateLimitInfo`]). Delegates to the service. Kept as an inherent
    /// method so both the `OrchestratorApiClient::last_rate_limit_info` (three-field
    /// projection) and `last_rate_limit_full` (full snapshot) trait overrides can
    /// reach it.
    #[must_use]
    pub fn last_rate_limit_info(&self) -> Option<RateLimitInfo> {
        self.service.last_rate_limit_info()
    }
}
// ── Trait implementations ─────────────────────────────────────────────────────

#[async_trait]
impl tool_api::McpTokenCounter for ProviderApiAdapter {
    async fn count_mcp_content_tokens(
        &self,
        model: &str,
        content: &serde_json::Value,
    ) -> Result<Option<u64>, String> {
        let blocks = match content {
            serde_json::Value::String(text) => {
                vec![protocol::ContentBlock::Text { text: text.clone() }]
            }
            serde_json::Value::Array(values) => values
                .iter()
                .filter(|&block| {
                    block.get("type").and_then(serde_json::Value::as_str) == Some("text")
                })
                .map(|block| protocol::ContentBlock::Text {
                    text: block
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect(),
            _ => return Ok(None),
        };
        if blocks.is_empty() {
            return Ok(None);
        }
        let message = ConversationMessage::User {
            id: protocol::MessageId::new(),
            content: blocks,
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        };
        self.service
            .count_tokens_exact(model, None, None, vec![message], Vec::new())
            .await
            .map_err(|error| error.to_string())
    }
}

#[async_trait]
impl OrchestratorApiClient for ProviderApiAdapter {
    fn active_betas(&self) -> Vec<String> {
        self.service.active_custom_betas().to_vec()
    }

    fn set_thinking_config(&self, thinking: llm_runtime::model::thinking::ThinkingConfig) {
        self.service.set_thinking(thinking);
    }

    fn set_effort(&self, effort: Option<serde_json::Value>) {
        *self
            .initial_effort
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = effort;
    }

    async fn messages_create(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        msgs: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
    ) -> Result<LlmResponse, LlmError> {
        if let Some(settings) = crate::scheduled_turn::current() {
            return self
                .service
                .messages_create_side_query_with_thinking(
                    &settings.model,
                    Some(&settings.provider),
                    system,
                    msgs,
                    tools,
                    None,
                    None,
                    Vec::new(),
                    Some(settings.thinking),
                    settings.effort,
                    None,
                    Some("scheduled_task"),
                )
                .await;
        }
        self.service
            .messages_create(model, profile, system, msgs, tools)
            .await
    }

    async fn messages_create_hook_prompt(
        &self,
        model: &str,
        profile: Option<&str>,
        system: &str,
        msgs: Vec<ConversationMessage>,
    ) -> Result<LlmResponse, LlmError> {
        let stream = self.service.stream_json_schema_with_thinking(
            model, profile, Some(system), msgs,
            serde_json::json!({"type":"object","properties":{"ok":{"type":"boolean"},"reason":{"type":"string"},"impossible":{"type":"boolean"}},"required":["ok","reason"],"additionalProperties":false}),
            None, None, Some(llm_runtime::model::thinking::ThinkingConfig::Disabled), None, Some("hook_prompt"),
        ).await?;
        llm_runtime::stream_accumulator::accumulate_stream_salvaging(stream)
            .await
            .map_err(|(_, error)| error)
    }

    async fn count_tokens(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        msgs: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
    ) -> Result<u64, LlmError> {
        self.service
            .count_tokens(model, profile, system, msgs, tools)
            .await
    }

    async fn count_tokens_exact(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        msgs: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
    ) -> Result<Option<u64>, LlmError> {
        self.service
            .count_tokens_exact(model, profile, system, msgs, tools)
            .await
    }

    async fn messages_create_with_context_hint(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        msgs: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
        context_hint: Option<serde_json::Value>,
    ) -> Result<LlmResponse, LlmError> {
        if let Some(settings) = crate::scheduled_turn::current() {
            let mut request = self.build_scheduled_request(settings, system, msgs, tools, None)?;
            request.context_hint = context_hint;
            return self.service.execute_side_query_request(request).await;
        }
        self.service
            .messages_create_with_context_hint(model, profile, system, msgs, tools, context_hint)
            .await
    }

    async fn messages_create_with_opts(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        msgs: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
        max_tokens: u32,
    ) -> Result<LlmResponse, LlmError> {
        if let Some(settings) = crate::scheduled_turn::current() {
            let request =
                self.build_scheduled_request(settings, system, msgs, tools, Some(max_tokens))?;

            return self.service.execute_side_query_request(request).await;
        }
        self.service
            .messages_create_with_opts(model, profile, system, msgs, tools, max_tokens)
            .await
    }

    async fn messages_create_with_fallback(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        msgs: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
        fallback_model: Option<&str>,
        is_subscriber: bool,
        is_enterprise: bool,
    ) -> Result<LlmResponse, LlmError> {
        if let Some(settings) = crate::scheduled_turn::current() {
            let request = self.build_scheduled_request(settings, system, msgs, tools, None)?;

            return self.service.execute_side_query_request(request).await;
        }
        self.service
            .messages_create_with_fallback(
                model,
                profile,
                system,
                msgs,
                tools,
                fallback_model,
                is_subscriber,
                is_enterprise,
            )
            .await
    }

    async fn messages_create_seeded(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        msgs: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
        initial_consecutive_overloaded: u8,
    ) -> Result<LlmResponse, LlmError> {
        if let Some(settings) = crate::scheduled_turn::current() {
            let request = self.build_scheduled_request(settings, system, msgs, tools, None)?;

            return self.service.execute_side_query_request(request).await;
        }
        self.service
            .messages_create_seeded(
                model,
                profile,
                system,
                msgs,
                tools,
                initial_consecutive_overloaded,
            )
            .await
    }

    fn available_models(&self) -> Vec<String> {
        self.service.available_models()
    }

    fn resolve_media_route(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<llm_runtime::MediaRoute, LlmError> {
        self.service.resolve_media_route(model, profile)
    }

    async fn analyze_vision_delegation(
        &self,
        packet: sidequery::VisionPacket,
    ) -> Result<sidequery::VisionDelegationResult, LlmError> {
        let client = std::sync::Arc::new(sidequery::ProviderSideQueryClient::from_service(
            self.service.clone(),
        ));
        let service = sidequery::VisionDelegationService::new(client);
        service.analyze(packet).await.map_err(|error| match error {
            sidequery::SideQueryError::Api(error) => error,
            sidequery::SideQueryError::InvalidResponse(message) => {
                LlmError::MediaDelegationUnavailable { message }
            }
            sidequery::SideQueryError::StructuredOutputUnsupported => {
                LlmError::MediaDelegationUnavailable {
                    message: "structured output is unsupported".into(),
                }
            }
            sidequery::SideQueryError::Partial {
                source,
                usage,
                elapsed,
                retry_count,
                api_calls,
            } => LlmError::MediaDelegationPartial {
                message: source.to_string(),
                accounting: MediaDelegationAccounting::from_counts(
                    usage.tokens.input,
                    usage.tokens.output,
                    usage.tokens.cache_write,
                    usage.tokens.cache_read,
                    usage.tokens.reasoning_output,
                    usage.tokens.cache_write_1h,
                    elapsed,
                    retry_count,
                    api_calls,
                ),
            },
        })
    }

    fn list_model_listings(&self) -> Vec<platform_api::orchestrator::ModelListing> {
        self.service
            .model_listings()
            .into_iter()
            .filter(|listing| listing.capabilities.tools)
            .map(lower_model_listing)
            .collect()
    }

    /// Return the most recently observed rate-limit header snapshot.
    ///
    /// Delegates to [`Self::last_rate_limit_info`] and maps the internal
    /// `RateLimitInfo` struct into the public [`platform_api::RateLimitSnapshot`]
    /// (all three fields: `rate_limit_type`, `overage_status`, and
    /// `overage_disabled_reason`).
    fn last_rate_limit_info(&self) -> Option<platform_api::RateLimitSnapshot> {
        self.last_rate_limit_info()
            .map(|info| platform_api::RateLimitSnapshot {
                rate_limit_type: info.rate_limit_type,
                overage_status: info.overage_status,
                overage_disabled_reason: info.overage_disabled_reason,
            })
    }

    fn last_request_id(&self) -> Option<String> {
        self.service.last_request_id()
    }

    fn last_retry_count(&self) -> u32 {
        self.service.last_retry_count()
    }

    fn thinking_signature_stripped(&self) -> bool {
        self.service.thinking_signature_stripped()
    }

    fn set_thinking_signature_stripped(&self, stripped: bool) {
        self.service.set_thinking_signature_stripped(stripped);
    }

    fn thinking_stripped_messages(&self) -> std::collections::HashMap<protocol::MessageId, usize> {
        self.service.thinking_stripped_messages()
    }

    fn set_thinking_stripped_messages(
        &self,
        messages: std::collections::HashMap<protocol::MessageId, usize>,
    ) {
        self.service.set_thinking_stripped_messages(messages);
    }

    /// Task 8 (llm-runtime future-work batch 3): expose the FULL internal
    /// nine-field snapshot for the turn drivers' `emit_rate_limit` seam.
    /// Delegates to the inherent [`Self::last_rate_limit_info`] (which
    /// already returns the internal `RateLimitInfo`); the trait method of
    /// the same name above keeps its three-field projection untouched.
    fn last_rate_limit_full(&self) -> Option<RateLimitInfo> {
        self.last_rate_limit_info()
    }

    /// Task 2 (llm-runtime future-work batch 5): expose the raw per-window
    /// snapshot cached by `record_rate_limit_from_headers` for the turn
    /// drivers' `emit_raw_utilization` seam.
    fn last_raw_utilization(&self) -> Option<RawUtilization> {
        self.service.last_raw_utilization()
    }

    /// Task 6 (llm-runtime future-work batch 5): expose the limits copy
    /// composed by `record_rate_limit_from_429` from the most recent
    /// 429 error response's unified headers, for the orchestrator's
    /// terminal-429 re-map (claude-code `errors.ts:480-524`).
    fn last_rate_limit_error_message(&self) -> Option<String> {
        self.service.last_rate_limit_error_message()
    }

    async fn prewarm_responses_websocket(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
    ) -> Result<(), LlmError> {
        self.service
            .prewarm_responses_websocket(model, profile, system, messages, tools)
            .await
    }

    async fn close_responses_websocket_session(&self) -> Result<(), LlmError> {
        self.service.close_responses_websocket_session().await
    }
}

/// Build the grouped-picker listing for EVERY provider.
///
/// First-party Anthropic is NOT part of the models.dev preset catalog
/// (`builtin_presets`); in a live session it enters via
/// `anthropic_provider_profile`. We surface it here too so this seam is
/// provider-COMPLETE on its own — it previously omitted Anthropic, which is why
/// `list_available_models` had to carry a Claude-only hardcoded fallback. With
/// Anthropic in the catalog that special case is gone. Anthropic is listed first
/// to keep the picker's Claude-first ordering for catalog-only (no live config)
/// callers.
fn lower_reasoning_spec(
    raw: llm_runtime::ReasoningControlSpec,
) -> platform_api::ReasoningControlSpec {
    let mandatory = raw
        .mandatory_selection
        .as_ref()
        .map(|selection| match selection {
            llm_runtime::ReasoningSelection::Automatic => {
                platform_api::ReasoningSelection::Automatic
            }
            llm_runtime::ReasoningSelection::Disabled => platform_api::ReasoningSelection::Disabled,
            llm_runtime::ReasoningSelection::Enabled => platform_api::ReasoningSelection::Enabled,
            llm_runtime::ReasoningSelection::Level(id) => {
                platform_api::ReasoningSelection::Level { id: id.clone() }
            }
            llm_runtime::ReasoningSelection::TokenBudget(tokens) => {
                platform_api::ReasoningSelection::TokenBudget {
                    tokens: u64::from(*tokens),
                }
            }
        });
    let mut available = vec![platform_api::ReasoningSelection::Automatic];
    if let Some(required) = mandatory.as_ref() {
        available = vec![required.clone()];
    } else {
        if raw.can_disable {
            available.push(platform_api::ReasoningSelection::Disabled);
        }
        if raw.can_enable {
            available.push(platform_api::ReasoningSelection::Enabled);
        }
        available.extend(
            raw.levels
                .iter()
                .cloned()
                .map(|id| platform_api::ReasoningSelection::Level { id }),
        );
    }
    let auto_only = available.len() == 1
        && matches!(
            available.first(),
            Some(platform_api::ReasoningSelection::Automatic)
        )
        && raw.token_budget.is_none();
    platform_api::ReasoningControlSpec {
        available,
        selections_persistable: mandatory.is_none(),
        budget_range: raw
            .token_budget
            .map(|range| platform_api::ReasoningBudgetRange {
                min_tokens: range.min,
                max_tokens: range.max,
                supports_dynamic: false,
                supports_disabled: raw.can_disable,
            }),
        provider_default: mandatory.unwrap_or(platform_api::ReasoningSelection::Automatic),
        forced: raw.mandatory_selection.is_some(),
        modifiable: raw.mandatory_selection.is_none() && !auto_only,
        disabled_reason: if raw.mandatory_selection.is_some() {
            Some("reasoning_required".to_string())
        } else if auto_only {
            Some("reasoning_unavailable".to_string())
        } else {
            None
        },
    }
}

/// Project an llm-runtime route listing into the provider-neutral picker type.
#[must_use]
pub fn lower_model_listing(listing: llm_runtime::ModelListing) -> platform_api::ModelListing {
    let capabilities = platform_api::ModelCapabilities {
        streaming: listing.capabilities.streaming,
        tools: listing.capabilities.tools,
        vision: listing.capabilities.vision,
        documents: listing.capabilities.documents,
        reasoning: listing.capabilities.reasoning,
        structured_output: listing.capabilities.structured_output,
    };
    // Computed before the struct literal moves `profile_name` out.
    // Only report a group when the profile actually is one connection of
    // several; a standalone provider leaves this default so every existing
    // consumer keeps reading `provider_id` and nothing changes for it.
    let connection = if listing.group == listing.profile_name {
        platform_api::ConnectionRef::default()
    } else {
        platform_api::ConnectionRef {
            group: Some(listing.group),
            connection_id: Some(listing.connection_id),
        }
    };
    platform_api::ModelListing {
        display_model: listing.display_model,
        request_model: listing.request_model,
        provider_label: provider_label_owned(&listing.profile_name),
        provider_id: listing.profile_name,
        description: listing.description,
        supports_reasoning: capabilities.reasoning,
        metadata: listing.metadata,
        capabilities,
        reasoning: lower_reasoning_spec(listing.reasoning),
        fusion_analyst_capable: listing.fusion_analyst_capable,
        connection,
    }
}

fn catalog_model_listings() -> Vec<platform_api::orchestrator::ModelListing> {
    // Build one listing, defaulting an absent description to a known per-model
    // parity blurb for the Claude family (models.dev / the Anthropic profiles
    // carry no such string).
    let row = |display_model: String,
               request_model: String,
               label: String,
               provider: String,
               description: Option<String>,
               supports_reasoning: bool| {
        let description =
            description.or_else(|| model_description(&request_model).map(str::to_string));
        platform_api::orchestrator::ModelListing {
            connection: Default::default(),
            display_model,
            request_model,
            provider_label: label,
            provider_id: provider,
            description,
            supports_reasoning,
            metadata: platform_api::ModelMetadata::default(),
            capabilities: platform_api::ModelCapabilities {
                reasoning: supports_reasoning,
                ..platform_api::ModelCapabilities::default()
            },
            reasoning: platform_api::ReasoningControlSpec::default(),
            // This fallback catalog carries no per-route capability or protocol
            // facts (`ModelCapabilities::default()` above), so it cannot claim
            // a route can judge. Fail closed: the `/fusion setup` analyst
            // picker shows nothing here rather than offering a pick that only
            // fails once every panel has spent.
            fusion_analyst_capable: false,
        }
    };

    // 1. First-party Anthropic (not in the preset catalog).
    let mut listings: Vec<platform_api::orchestrator::ModelListing> =
        llm_runtime::anthropic_model_profiles()
            .into_iter()
            .map(|m| {
                let supports_reasoning = m.capabilities.reasoning;
                row(
                    m.display_model,
                    m.request_model,
                    provider_label("anthropic").to_string(),
                    "anthropic".to_string(),
                    m.description,
                    supports_reasoning,
                )
            })
            .collect();

    // 2. Static models.dev presets (OpenAI, Gemini, DeepSeek, …). EXCLUDE models
    //    that don't support tool calls (`tool_call=false` — image/TTS/audio
    //    models, gpt-3.5-turbo, gpt-5-chat-latest, the aion-labs set, ~85
    //    OpenRouter passthroughs). The agent sends tools on EVERY turn, so such a
    //    model can never complete an agentic turn — it hard-fails "unsupported
    //    capability: tools" (llm-runtime `validate_capabilities`). Offering it in
    //    the `/model` picker is offering a permanently-broken pick; it stays
    //    resolvable by explicit id for any non-agentic caller.
    let catalog = llm_runtime::builtin_presets();
    if let Ok(registry) = llm_runtime::ModelRegistry::from_config(llm_runtime::ClientConfig {
        providers: catalog.providers,
    }) {
        listings.extend(
            registry
                .available_models()
                .into_iter()
                .filter(|m| m.capabilities.tools)
                .map(lower_model_listing),
        );
    }
    listings
}

/// Cached `request_model -> display_model` map over the full static catalog
/// (Anthropic first-party + models.dev presets), built once on first use.
fn catalog_display_names() -> &'static std::collections::HashMap<String, String> {
    static MAP: std::sync::OnceLock<std::collections::HashMap<String, String>> =
        std::sync::OnceLock::new();
    MAP.get_or_init(|| {
        catalog_model_listings()
            .into_iter()
            .map(|m| (m.request_model, m.display_model))
            .collect()
    })
}

/// The catalog display name for `request_model` (e.g. `deepseek-v4-pro` ->
/// `"DeepSeek V4 Pro"`), or `None` for an id not in the catalog.
///
/// Feeds the `<env>` identity line's marketing-name slot for NON-Claude models
/// — Claude ids resolve their name via [`crate::prompt::env_meta::
/// marketing_name_for_model`] (byte-parity), and only when THAT returns `None`
/// (a non-Claude model) does the builder fall back to this so the line reads
/// the strong "powered by the model named {name}" form instead of the weak
/// id-only fallback. Cached, so it is cheap to call per turn.
#[must_use]
pub(crate) fn display_name_for_model(request_model: &str) -> Option<String> {
    catalog_display_names().get(request_model).cloned()
}

/// Cached `request_model -> UNIQUE provider profile`. The value is `None` when
/// the same wire id is served by MORE than one provider (ambiguous), so callers
/// don't guess. Built once from the full catalog.
fn catalog_provider_profiles() -> &'static std::collections::HashMap<String, Option<String>> {
    static MAP: std::sync::OnceLock<std::collections::HashMap<String, Option<String>>> =
        std::sync::OnceLock::new();
    MAP.get_or_init(|| {
        let mut map: std::collections::HashMap<String, Option<String>> =
            std::collections::HashMap::new();
        for l in catalog_model_listings() {
            let request_model = l.request_model;
            let provider = l.provider_id;
            match map.get_mut(&request_model) {
                // Already seen under a DIFFERENT provider → ambiguous.
                Some(existing) => {
                    if existing.as_deref() != Some(provider.as_str()) {
                        *existing = None;
                    }
                }
                None => {
                    map.insert(request_model, Some(provider));
                }
            }
        }
        map
    })
}

/// The UNIQUE provider profile that serves `request_model` in the catalog, or
/// `None` when the id is unknown OR served by more than one provider. Lets
/// cost/telemetry attribute a bare wire id whose live session profile is unknown
/// (e.g. after a cross-provider `--resume` clears it) to its REAL provider
/// instead of blindly defaulting to Anthropic.
#[must_use]
pub(crate) fn provider_for_model(request_model: &str) -> Option<String> {
    catalog_provider_profiles()
        .get(request_model)
        .cloned()
        .flatten()
}

/// Whether a catalog model has multiple possible connection profiles.
pub(crate) fn model_has_ambiguous_profile(request_model: &str) -> bool {
    catalog_provider_profiles().get(request_model) == Some(&None)
}

/// Known one-line description for a built-in model wire id, mirroring the
/// claude-code `/model` picker blurbs (`modelOptions.ts`). Matched by a
/// case-insensitive family substring so dated ids (`claude-opus-4-7`, etc.) and
/// short aliases (`opus`, `sonnet`, `haiku`) both resolve. Returns `None` for
/// any id we don't recognize (then the row renders with no sub-line).
#[must_use]
pub(crate) fn model_description(request_model: &str) -> Option<&'static str> {
    let id = request_model.to_ascii_lowercase();
    if id.contains("opus") {
        Some("Best for everyday, complex tasks")
    } else if id.contains("haiku") {
        Some("Fastest for quick answers")
    } else if id.contains("sonnet") {
        Some("Efficient for routine tasks")
    } else {
        None
    }
}

/// Human provider header for a catalog profile name, connection-aware.
///
/// A provider reachable several ways names each connection
/// `<group>:<connection>` (+ `#<n>` per extra key slot). Labelling that raw
/// would put `deepseek:cn#1` in the settings model directory as if it were a
/// vendor, so the label is built from the VENDOR plus the connection using the
/// same parser the TUI picker and the desktop header use.
fn provider_label_owned(profile_name: &str) -> String {
    let (group, connection, slot) = platform_api::split_connection_profile(profile_name);
    if connection.is_none() && slot.is_none() {
        return provider_label(profile_name).to_string();
    }
    let base = provider_label(group).to_string();
    match (connection, slot) {
        (Some(connection), Some(slot)) => format!("{base} · {connection} · key {}", slot + 1),
        (Some(connection), None) => format!("{base} · {connection}"),
        (None, Some(slot)) => format!("{base} · key {}", slot + 1),
        (None, None) => base,
    }
}

/// Human provider header for a catalog profile name.
fn provider_label(profile_name: &str) -> &str {
    match profile_name {
        "anthropic" => "Anthropic",
        "openrouter" => "OpenRouter",
        "deepseek" => "DeepSeek",
        "kimi" => "Kimi",
        "kimi-code" => "Kimi Code",
        "glm-coding" => "GLM (coding)",
        "zai" => "Z.AI",
        "openai" => "OpenAI",
        "openai-chatgpt" => "OpenAI (ChatGPT login)",
        "github-copilot" => "GitHub Copilot",
        "gemini" => "Google Gemini",
        "zhipuai-coding-plan" => "GLM (coding)",
        other => other,
    }
}

/// Subagent API seam — delegates 1:1 to the service. Subagent calls never carry
/// a provider profile, so `profile` is always `None`.
#[async_trait]
impl agent::SubagentApiClient for ProviderApiAdapter {
    fn consume_pending_near_limit_wrap_up_hint(&self) -> bool {
        self.service.consume_pending_near_limit_wrap_up_hint()
    }

    fn dispatch_near_limit_checkpoint(&self, request: agent::NearLimitCheckpointRequest) {
        // The port has one shared `SessionState.todos` list rather than the
        // oracle's per-agent todo buckets. A child-specific bucket therefore
        // has no representable entries here; keep it empty instead of
        // accidentally snapshotting the main thread's plan.
        let _ = session::dispatch_rate_limit_checkpoint(session::OwnedCheckpointRequest {
            session_id: session::checkpoint_session_key(request.session_id),
            trigger: session::CheckpointTrigger::NearLimit,
            todos: Vec::new(),
            cwd: request.cwd,
            gates: session::CheckpointGates {
                non_interactive: request.non_interactive,
                remote_workspace: false,
                policy_allows: session::local_checkpoint_commit_allowed(),
            },
        });
    }

    fn record_usage_limit_near_wrap_up(&self) {
        // Oracle `y("usage_limit_near_wrapup")`: a success/count gate rather
        // than a `tengu_*` analytics-bus event. Match the existing `y(...)`
        // ports by emitting a debug tracing event with the literal gate name.
        tracing::debug!(event = "usage_limit_near_wrapup");
    }

    async fn messages_create(
        &self,
        model: &str,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
    ) -> Result<LlmResponse, LlmError> {
        self.service
            .messages_create(model, None, system, messages, tools)
            .await
    }

    async fn messages_create_stream(
        &self,
        model: &str,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
        effort: Option<serde_json::Value>,
    ) -> Result<BoxStream<'static, Result<LlmEvent, LlmError>>, LlmError> {
        // Subagent stream: fast mode is a main-loop-only tier, so `speed=None`.
        self.service
            .stream(model, None, system, messages, tools, effort, None)
            .await
    }

    async fn messages_create_stream_forced(
        &self,
        model: &str,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
        forced_tool: Option<&str>,
        effort: Option<serde_json::Value>,
    ) -> Result<BoxStream<'static, Result<LlmEvent, LlmError>>, LlmError> {
        self.service
            .stream_forced(model, None, system, messages, tools, forced_tool, effort)
            .await
    }

    // ── Provider-routed variants (dual-LLM dual-PROVIDER) ──────────────────
    // These forward the per-spawn `profile` to the multi-provider service so a
    // dual-LLM candidate's resolved provider is honored, instead of dropping the
    // profile (which forced every subagent onto the default provider). The
    // default-trait impls delegate to the profile-less methods above; these
    // overrides are the single place the subagent path becomes provider-aware.

    async fn messages_create_in(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
    ) -> Result<LlmResponse, LlmError> {
        self.service
            .messages_create(model, profile, system, messages, tools)
            .await
    }

    async fn messages_create_stream_in(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
        effort: Option<serde_json::Value>,
    ) -> Result<BoxStream<'static, Result<LlmEvent, LlmError>>, LlmError> {
        // Subagent stream: fast mode is a main-loop-only tier, so `speed=None`.
        self.service
            .stream(model, profile, system, messages, tools, effort, None)
            .await
    }

    async fn messages_create_stream_forced_in(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
        forced_tool: Option<&str>,
        effort: Option<serde_json::Value>,
    ) -> Result<BoxStream<'static, Result<LlmEvent, LlmError>>, LlmError> {
        self.service
            .stream_forced(model, profile, system, messages, tools, forced_tool, effort)
            .await
    }

    // ── Fusion / COGS opts-aware variants (WP2a item 2, F002 sub-claim 3) ──
    // The default trait impls silently drop `opts` and fall back to the
    // profile-routed methods above (`max_output_tokens` never reaches the
    // wire, `query_source_label` never tags telemetry). These overrides are
    // the only place a `SubagentApiCallOpts`-aware caller (Fusion panels)
    // becomes wire-faithful: `opts.max_output_tokens` → the request's
    // `max_tokens`, `opts.query_source_label` → `QuerySource` (the label is
    // already the `sidequery::QuerySource::as_str()` wire form — see
    // `SubagentSpawnRequest::query_source_label`'s own doc — so it is threaded
    // through verbatim without re-depending on `sidequery` here).

    async fn messages_create_stream_in_opts(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
        effort: Option<serde_json::Value>,
        opts: agent::api::SubagentApiCallOpts,
    ) -> Result<BoxStream<'static, Result<LlmEvent, LlmError>>, LlmError> {
        self.service
            .stream_with_attempt_opts(
                model,
                profile,
                system,
                messages,
                tools,
                None,
                effort,
                opts.max_output_tokens,
                opts.query_source_label.as_deref(),
                opts.model_attempt,
            )
            .await
    }

    async fn messages_create_stream_forced_in_opts(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
        forced_tool: Option<&str>,
        effort: Option<serde_json::Value>,
        opts: agent::api::SubagentApiCallOpts,
    ) -> Result<BoxStream<'static, Result<LlmEvent, LlmError>>, LlmError> {
        self.service
            .stream_with_attempt_opts(
                model,
                profile,
                system,
                messages,
                tools,
                forced_tool,
                effort,
                opts.max_output_tokens,
                opts.query_source_label.as_deref(),
                opts.model_attempt,
            )
            .await
    }
}

#[async_trait]
impl StreamingApiClient for ProviderApiAdapter {
    async fn stream(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<serde_json::Value>,
    ) -> Result<BoxStream<'static, Result<LlmEvent, LlmError>>, LlmError> {
        if let Some(settings) = crate::scheduled_turn::current() {
            let request = self.build_scheduled_request(settings, system, messages, tools, None)?;
            return self.service.stream_request(request).await;
        }
        // (M4 cc2.1.198) The MAIN loop carries the session's initial effort
        // (CLI `--effort` → `output_config.effort`); `None` (no flag) keeps
        // the pre-M4 body byte-identical.
        // (/fast) When the shared fast-mode flag is set AND the active model
        // carries the canonical fast-mode capability, send `speed:"fast"` — the
        // service's `beta_context` reads it back to add the fast-mode beta.
        // `None` (flag off, or an unsupported model) keeps the body unchanged.
        let speed = if self.fast_mode.load(std::sync::atomic::Ordering::SeqCst)
            && model_supports_fast_mode(model)
        {
            Some("fast".to_string())
        } else {
            None
        };
        self.service
            .stream(
                model,
                profile,
                system,
                messages,
                tools,
                self.current_effort(),
                speed,
            )
            .await
    }

    fn last_retry_count(&self) -> u32 {
        self.service.last_retry_count()
    }

    fn thinking_signature_stripped(&self) -> bool {
        self.service.thinking_signature_stripped()
    }

    fn set_thinking_signature_stripped(&self, stripped: bool) {
        self.service.set_thinking_signature_stripped(stripped);
    }

    fn thinking_stripped_messages(&self) -> std::collections::HashMap<protocol::MessageId, usize> {
        self.service.thinking_stripped_messages()
    }

    fn set_thinking_stripped_messages(
        &self,
        messages: std::collections::HashMap<protocol::MessageId, usize>,
    ) {
        self.service.set_thinking_stripped_messages(messages);
    }
}
// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {

    /// The settings model directory must not show a raw connection profile name
    /// as if it were a vendor. This is the third copy of the profile-name→label
    /// mapping (with `engine-desktop` and the TUI picker) and was the one left
    /// behind, so `deepseek:cn#1` reached the desktop and mobile catalog.
    #[test]
    fn provider_label_is_connection_aware() {
        assert_eq!(super::provider_label_owned("deepseek"), "DeepSeek");
        assert_eq!(super::provider_label_owned("deepseek:cn"), "DeepSeek · cn");
        assert_eq!(
            super::provider_label_owned("deepseek:cn#1"),
            "DeepSeek · cn · key 2"
        );
        // `#` is a key slot only when a number follows it: an unknown provider
        // keeps its WHOLE name rather than losing half of it to a phantom slot.
        // (This adapter returns unknown names verbatim; only the desktop header
        // title-cases them.)
        assert_eq!(super::provider_label_owned("weird#name"), "weird#name");
    }
    use super::*;
    use llm_runtime::model::user_agent::UserAgentEnv;
    use llm_runtime::DefaultLlmClient;
    use llm_runtime::{
        ApiService, AuthStrategy, BoxFuture, Capabilities, ClientConfig, CredentialConfig,
        LlmError, ModelProfile, PricingConfig, ProtocolFamily, ProviderId, ProviderProfile,
        ProviderRequest, ProviderResponse, StreamingResponse, Transport,
    };
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    // ── FakeTransport (duplicated into the orchestrator-domain tests) ─────────────

    /// A fake Transport that returns a scripted sequence of responses (or errors).
    struct FakeTransport {
        /// Pre-recorded responses returned in order; cycles to last entry.
        responses: Mutex<Vec<FakeResponse>>,
        /// All requests received, in order.
        seen: Mutex<Vec<ProviderRequest>>,
    }

    #[derive(Clone)]
    #[allow(dead_code)]
    enum FakeResponse {
        Ok(ProviderResponse),
        Err(LlmError),
    }

    impl FakeTransport {
        /// Return the same response on every call.
        fn always(resp: ProviderResponse) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(vec![FakeResponse::Ok(resp)]),
                seen: Mutex::new(vec![]),
            })
        }

        #[allow(dead_code)]
        fn seen_count(&self) -> usize {
            self.seen.lock().unwrap().len()
        }
    }

    impl llm_runtime::test_support::FixtureTransport for FakeTransport {
        fn execute<'a>(
            &'a self,
            request: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
            let mut seen = self.seen.lock().unwrap();
            seen.push(request.clone());
            let idx = (seen.len() - 1).min({
                let resps = self.responses.lock().unwrap();
                resps.len().saturating_sub(1)
            });
            let resp = {
                let resps = self.responses.lock().unwrap();
                resps[idx].clone()
            };
            Box::pin(async move {
                match resp {
                    FakeResponse::Ok(r) => Ok(r),
                    FakeResponse::Err(e) => Err(e),
                }
            })
        }

        fn open_stream<'a>(
            &'a self,
            request: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
            // Record the WIRE request (post-codec `body_json`) so tests can
            // inspect exactly what a streaming call sent — e.g. WP2a item 2's
            // `stream_with_opts`/`stream_forced_with_opts` max_tokens
            // assertion — even though this fake never returns a real stream.
            self.seen.lock().unwrap().push(request.clone());
            Box::pin(async move {
                // `InvalidRequest` (not `Transport`): `ApiService::drive_stream`'s
                // retry classifier (`next_step_with_backoff`,
                // `llm-runtime/src/model/retry.rs`) treats `Transport` as
                // retry-worthy and backs off + retries up to the configured cap
                // — for a caller that never scripts a `StreamingResponse` (every
                // test that only inspects `seen` / the wire request, e.g. this
                // one) that turned one assertion into a multi-hundred-second
                // real-time backoff loop for no reason the assertion cares
                // about. `InvalidRequest` hits the "overflow check, else
                // Terminal" arm — a message that doesn't parse as an overflow
                // report (this one doesn't) is `DriveStep::Terminal` — so the
                // call returns after exactly ONE `open_stream` per attempt,
                // with the request still recorded in `seen` first.
                Err(LlmError::InvalidRequest {
                    message: "open_stream not scripted (FakeTransport)".to_string(),
                })
            })
        }
    }
    llm_runtime::impl_fixture_transport!(FakeTransport);

    // ── Test helpers ──────────────────────────────────────────────────────────

    fn ok_response_json() -> serde_json::Value {
        serde_json::json!({
            "id": "msg_test",
            "model": "claude-sonnet-4-20250514",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 5, "output_tokens": 2}
        })
    }

    #[tokio::test]
    async fn scheduled_settings_reach_wire_without_changing_adapter_defaults() {
        let transport = FakeTransport::always(ProviderResponse {
            status: 200,
            headers: BTreeMap::new(),
            body_json: ok_response_json(),
            request_id: None,
        });
        let adapter =
            make_adapter(transport.clone()).with_initial_effort(Some(serde_json::json!("low")));
        let settings = crate::scheduled_turn::ScheduledSettings {
            model: "claude-sonnet-4-20250514".into(),
            provider: "anthropic".into(),
            reasoning: platform_api::ReasoningSelection::Disabled,
            thinking: llm_runtime::model::thinking::ThinkingConfig::Disabled,
            effort: None,
        };
        let _ = crate::scheduled_turn::SETTINGS
            .scope(
                settings,
                StreamingApiClient::stream(
                    &adapter,
                    "ignored/default",
                    None,
                    None,
                    Vec::new(),
                    Vec::new(),
                ),
            )
            .await;
        let request = transport.seen.lock().unwrap()[0].body_json.clone();
        assert_eq!(request["model"], "claude-sonnet-4-20250514");
        assert!(request["output_config"]["effort"].is_null());
        assert!(crate::scheduled_turn::current().is_none());
        assert_eq!(adapter.current_effort(), Some(serde_json::json!("low")));
    }

    /// Build the thin `ProviderApiAdapter` over an `ApiService` constructed exactly
    /// as the original `make_adapter` did (a single anthropic profile, ApiKey auth).
    fn make_adapter(transport: Arc<dyn Transport>) -> ProviderApiAdapter {
        std::env::set_var("ADAPTER_TEST_KEY", "test-key");
        let client = Arc::new(
            DefaultLlmClient::from_config(ClientConfig {
                providers: vec![ProviderProfile {
                    wire_profile: None,
                    regions: llm_runtime::Region::all(),
                    provider_id: ProviderId::AnthropicFirstParty,
                    profile_name: "anthropic".to_string(),
                    base_url: "https://api.anthropic.com".to_string(),
                    protocol: ProtocolFamily::AnthropicMessages,
                    auth: AuthStrategy::ApiKey,
                    credential: CredentialConfig::Env {
                        var: "ADAPTER_TEST_KEY".to_string(),
                    },
                    models: vec![
                        ModelProfile {
                            display_model: "claude-sonnet-4-20250514".to_string(),
                            request_model: "claude-sonnet-4-20250514".to_string(),
                            billing_model: "claude-sonnet-4".to_string(),
                            aliases: vec!["claude".to_string()],
                            description: None,
                            metadata: Default::default(),
                            capabilities: Capabilities {
                                streaming: true,
                                tools: true,
                                reasoning: true,
                                ..Default::default()
                            },
                        },
                        ModelProfile {
                            display_model: "claude-legacy-no-tools".to_string(),
                            request_model: "claude-legacy-no-tools".to_string(),
                            billing_model: "claude-legacy-no-tools".to_string(),
                            aliases: Vec::new(),
                            description: None,
                            metadata: Default::default(),
                            capabilities: Capabilities {
                                streaming: true,
                                tools: false,
                                reasoning: false,
                                ..Default::default()
                            },
                        },
                    ],
                    pricing: PricingConfig::default(),
                    signing: None,
                    azure: None,
                    supports_websockets: false,
                    supports_websocket_compression: false,
                    websocket_connect_timeout_ms: None,
                    vision_delegate: None,
                    connection: Default::default(),
                }],
            })
            .expect("client"),
        );
        ProviderApiAdapter::new(Arc::new(ApiService::new(
            client,
            transport,
            SubscriberState::default(),
            UserAgentEnv {
                user_type: Some("external".to_string()),
                entrypoint: Some("cli".to_string()),
                ..Default::default()
            },
            "0.0.0",
            None,
            None,
        )))
    }

    #[tokio::test]
    async fn cancelled_thinking_retry_is_durable_before_cold_resume_without_duplicate_marker() {
        use crate::test_support::{
            noop_hook_executor, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
        };
        struct RetryTransport(tokio::sync::Notify);
        impl llm_runtime::test_support::FixtureTransport for RetryTransport {
            fn execute<'a>(
                &'a self,
                request: &'a ProviderRequest,
            ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
                Box::pin(async move {
                    let thinking = request.body_json["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .flat_map(|m| m["content"].as_array().unwrap())
                        .any(|b| b["type"] == "thinking");
                    if thinking {
                        Ok(ProviderResponse::json(
                            400,
                            serde_json::json!({
                                "type":"error", "error":{"type":"invalid_request_error", "message":"Invalid signature in thinking block"}
                            }),
                        ))
                    } else {
                        self.0.notify_one();
                        std::future::pending().await
                    }
                })
            }
            fn open_stream<'a>(
                &'a self,
                _: &'a ProviderRequest,
            ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
                Box::pin(async {
                    Err(LlmError::Transport {
                        message: "unused".into(),
                    })
                })
            }
        }
        llm_runtime::impl_fixture_transport!(RetryTransport);
        let transport = Arc::new(RetryTransport(tokio::sync::Notify::new()));
        let adapter = Arc::new(make_adapter(transport.clone()));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let writer = Arc::new(session::jsonl::writer::JsonlWriter::new(
            path.clone(),
            Arc::new(platform_posix::fs::PosixFileSystem::new(
                dir.path().to_path_buf(),
            )),
        ));
        let orch = crate::ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            adapter.clone(),
            Arc::new(tool_api::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            dir.path().to_path_buf(),
        )
        .with_jsonl_writer(writer);
        let old = ConversationMessage::Assistant {
            id: protocol::MessageId::new(),
            content: vec![
                protocol::ContentBlock::Thinking {
                    thinking: "rejected".into(),
                    signature: Some("sig".into()),
                },
                protocol::ContentBlock::Text {
                    text: "old answer".into(),
                },
            ],
            stop_reason: Some("end_turn".into()),
        };
        orch.session.lock().await.history.push(old.clone());
        orch.persist_message_to_jsonl(&old).await;
        {
            let call = orch.scope_api_session(
                false,
                OrchestratorApiClient::messages_create(
                    adapter.as_ref(),
                    "claude-sonnet-4-20250514",
                    None,
                    None,
                    vec![old.clone()],
                    vec![],
                ),
            );
            tokio::pin!(call);
            tokio::select! {
                _ = transport.0.notified() => {},
                result = &mut call => panic!("retry should remain pending: {result:?}"),
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => panic!("retry did not start"),
            }
            // Dropping the in-flight retry skips every post-call persistence seam.
        }
        let body = tokio::fs::read_to_string(&path).await.unwrap();
        let rows: Vec<session::JsonlMessage> = body
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            rows.len(),
            2,
            "one assistant and its recovery attachment: {body}"
        );
        assert_eq!(rows[1].parent_uuid.as_deref(), Some(rows[0].uuid.as_str()));
        let resumed = crate::resume::state_from_messages(
            orch.session.lock().await.session_id.as_uuid(),
            &rows,
        );
        assert!(resumed.thinking_signature_stripped);
        assert!(!resumed.thinking_stripped_messages.is_empty());
        let mut resumed_history = resumed.history.clone();
        llm_runtime::model::thinking_signature::strip_marked_conversation_thinking(
            &mut resumed_history,
            &resumed.thinking_stripped_messages,
        );
        assert!(resumed_history.iter().all(|m| match m {
            ConversationMessage::Assistant { content, .. } => !content
                .iter()
                .any(|b| matches!(b, protocol::ContentBlock::Thinking { .. })),
            _ => true,
        }));
        orch.persist_thinking_signature_strip_latch().await;
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            body,
            "post-call synchronization cannot duplicate a durable recovery marker"
        );
    }

    #[tokio::test]
    async fn shared_adapter_isolates_thinking_recovery_for_identical_parent_ids() {
        use llm_runtime::thinking_scope::{scope_thinking_recovery, ThinkingRecoveryScope};
        struct ScopedTransport {
            seen: Mutex<Vec<ProviderRequest>>,
        }
        impl llm_runtime::test_support::FixtureTransport for ScopedTransport {
            fn execute<'a>(
                &'a self,
                request: &'a ProviderRequest,
            ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
                Box::pin(async move {
                    // Both sessions enter the transport before either resumes.
                    tokio::task::yield_now().await;
                    self.seen.lock().unwrap().push(request.clone());
                    let body = &request.body_json;
                    let rejecting = body["system"].to_string().contains("reject-session");
                    let has_thinking = body["messages"].as_array().unwrap().iter().any(|m| {
                        m["content"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|b| b["type"] == "thinking")
                    });
                    Ok(if rejecting && has_thinking {
                        ProviderResponse::json(
                            400,
                            serde_json::json!({
                                "type":"error", "error":{"type":"invalid_request_error",
                                "message":"Invalid signature in thinking block"}
                            }),
                        )
                    } else {
                        ProviderResponse::json(200, ok_response_json())
                    })
                })
            }
            fn open_stream<'a>(
                &'a self,
                _: &'a ProviderRequest,
            ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
                Box::pin(async {
                    Err(LlmError::Transport {
                        message: "unused".into(),
                    })
                })
            }
        }
        llm_runtime::impl_fixture_transport!(ScopedTransport);
        // Use the same production adapter and service for both query owners.
        let transport = Arc::new(ScopedTransport {
            seen: Mutex::new(Vec::new()),
        });
        let adapter = make_adapter(transport.clone());
        let old = ConversationMessage::Assistant {
            id: protocol::MessageId::new(),
            content: vec![
                protocol::ContentBlock::Thinking {
                    thinking: "parent".into(),
                    signature: Some("sig".into()),
                },
                protocol::ContentBlock::Text {
                    text: "answer".into(),
                },
            ],
            stop_reason: Some("end_turn".into()),
        };
        let a = ThinkingRecoveryScope::default();
        let b = ThinkingRecoveryScope::default();
        let (ra, rb) = tokio::join!(
            scope_thinking_recovery(
                a.clone(),
                OrchestratorApiClient::messages_create(
                    &adapter,
                    "claude-sonnet-4-20250514",
                    None,
                    Some("reject-session"),
                    vec![old.clone()],
                    vec![]
                )
            ),
            scope_thinking_recovery(
                b.clone(),
                OrchestratorApiClient::messages_create(
                    &adapter,
                    "claude-sonnet-4-20250514",
                    None,
                    Some("healthy-session"),
                    vec![old.clone()],
                    vec![]
                )
            ),
        );
        ra.unwrap();
        rb.unwrap();
        assert_eq!(a.messages().get(&old.id()), Some(&0));
        assert!(b.messages().is_empty());
        let fresh = ConversationMessage::Assistant {
            id: protocol::MessageId::new(),
            content: vec![
                protocol::ContentBlock::Thinking {
                    thinking: "fresh".into(),
                    signature: Some("fresh-sig".into()),
                },
                protocol::ContentBlock::Text {
                    text: "fresh answer".into(),
                },
            ],
            stop_reason: Some("end_turn".into()),
        };
        let history = vec![
            old.clone(),
            ConversationMessage::user(protocol::MessageId::new(), "continue".into()),
            fresh.clone(),
        ];
        let (ra, rb) = tokio::join!(
            scope_thinking_recovery(
                a.clone(),
                OrchestratorApiClient::messages_create(
                    &adapter,
                    "claude-sonnet-4-20250514",
                    None,
                    Some("next-a"),
                    history.clone(),
                    vec![]
                )
            ),
            scope_thinking_recovery(
                b.clone(),
                OrchestratorApiClient::messages_create(
                    &adapter,
                    "claude-sonnet-4-20250514",
                    None,
                    Some("next-b"),
                    history,
                    vec![]
                )
            ),
        );
        ra.unwrap();
        rb.unwrap();
        for (label, expected) in [("next-a", 1), ("next-b", 2)] {
            let seen = transport.seen.lock().unwrap();
            let body = &seen
                .iter()
                .find(|r| r.body_json["system"].to_string().contains(label))
                .unwrap()
                .body_json;
            let thinking = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|m| m["content"].as_array().unwrap())
                .filter(|b| b["type"] == "thinking")
                .count();
            assert_eq!(thinking, expected, "{label}: {body}");
        }
        assert!(!a.messages().contains_key(&fresh.id()));
        assert!(b.messages().is_empty());
        assert!(OrchestratorApiClient::thinking_stripped_messages(&adapter).is_empty());
    }

    #[test]
    fn list_model_listings_exposes_actual_configured_catalog() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);
        let listings = OrchestratorApiClient::list_model_listings(&adapter);
        assert_eq!(
            listings.len(),
            1,
            "only configured, tool-capable routes are listed"
        );
        let listing = &listings[0];
        assert_eq!(listing.provider_id, "anthropic");
        assert_eq!(listing.provider_label, "Anthropic");
        assert_eq!(listing.request_model, "claude-sonnet-4-20250514");
        assert!(listing.capabilities.tools);
        assert!(listing.capabilities.reasoning);
    }

    #[test]
    fn list_model_listings_excludes_no_tool_models() {
        // Models with `tools=false` cannot complete an agentic turn and are
        // filtered from the actual configured catalog.
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);
        let listings = OrchestratorApiClient::list_model_listings(&adapter);
        let has = |id: &str| listings.iter().any(|l| l.request_model == id);

        assert!(has("claude-sonnet-4-20250514"));
        assert!(!has("claude-legacy-no-tools"));
    }

    #[test]
    fn list_model_listings_surfaces_reasoning_capability() {
        // The picker's thinking indicator reads `supports_reasoning` off each
        // listing (populated from the catalog `capabilities.reasoning`).
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);
        let listings = OrchestratorApiClient::list_model_listings(&adapter);
        let reasoning = |id: &str| {
            listings
                .iter()
                .find(|l| l.request_model == id)
                .map(|l| l.supports_reasoning)
        };
        assert_eq!(reasoning("claude-sonnet-4-20250514"), Some(true));
        let listing = listings
            .iter()
            .find(|listing| listing.request_model == "claude-sonnet-4-20250514")
            .expect("configured route listed");
        assert!(!listing.reasoning.modifiable);
        assert_eq!(
            listing.reasoning.disabled_reason.as_deref(),
            Some("reasoning_unavailable"),
            "an unverified custom/legacy id stays auto-only even when its broad capability flag is true"
        );
    }

    #[test]
    fn model_description_matches_known_families() {
        assert_eq!(
            model_description("claude-opus-4-7"),
            Some("Best for everyday, complex tasks")
        );
        assert_eq!(
            model_description("anthropic/claude-sonnet-4-6"),
            Some("Efficient for routine tasks")
        );
        assert_eq!(
            model_description("claude-3-5-haiku"),
            Some("Fastest for quick answers")
        );
        assert_eq!(model_description("gpt-4o"), None);
    }

    #[tokio::test]
    async fn subagent_api_client_seam_forwards_through_trait_object() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport.clone());
        let seam: Arc<dyn agent::SubagentApiClient> = Arc::new(adapter);
        let result = seam
            .messages_create(
                "claude-sonnet-4-20250514",
                Some("sys"),
                Vec::new(),
                Vec::new(),
            )
            .await;
        // May succeed or fail with UnsupportedCapability if stream not configured,
        // but must not panic.
        let _ = result;
    }

    /// WP2a item 2 (F002 sub-claim 3): `messages_create_stream_in_opts` /
    /// `messages_create_stream_forced_in_opts` must thread
    /// `SubagentApiCallOpts::max_output_tokens` onto the WIRE request's
    /// `max_tokens` — the trait DEFAULTS silently drop `opts` and fall back to
    /// the profile-routed methods, which never touch it. `FakeTransport`'s
    /// `open_stream` records the request even though it always errors (no
    /// scripted `StreamingResponse`), so this asserts purely on what reached
    /// the transport, not on a successful round-trip.
    #[tokio::test]
    async fn opts_variants_thread_max_output_tokens_onto_the_wire() {
        let auto_transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let auto_seam: Arc<dyn agent::SubagentApiClient> =
            Arc::new(make_adapter(auto_transport.clone()));
        let auto_error = auto_seam
            .messages_create_stream_in_opts(
                "claude-sonnet-4-20250514",
                None,
                None,
                Vec::new(),
                Vec::new(),
                None,
                agent::api::SubagentApiCallOpts {
                    max_output_tokens: Some(777),
                    model_attempt: None,
                    query_source_label: Some("fusion_panel".to_string()),
                },
            )
            .await
            .err();
        let auto_seen = auto_transport.seen.lock().unwrap();
        let auto_request = auto_seen
            .last()
            .unwrap_or_else(|| panic!("auto request did not dispatch: {auto_error:?}"));
        assert_eq!(
            auto_request.body_json.get("max_tokens").and_then(serde_json::Value::as_u64),
            Some(777),
            "wire max_tokens must equal the requested ceiling, not the model's auto default; body: {}",
            auto_request.body_json
        );
        drop(auto_seen);

        let forced_transport =
            FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let forced_seam: Arc<dyn agent::SubagentApiClient> =
            Arc::new(make_adapter(forced_transport.clone()));
        let _ = forced_seam
            .messages_create_stream_forced_in_opts(
                "claude-sonnet-4-20250514",
                None,
                None,
                Vec::new(),
                vec![serde_json::json!({"name":"StructuredOutput","description":"Return structured output","input_schema":{"type":"object","properties":{}}})],
                Some("StructuredOutput"),
                None,
                agent::api::SubagentApiCallOpts {
                    max_output_tokens: Some(321),
                    model_attempt: None,
                    query_source_label: Some("fusion_panel".to_string()),
                },
            )
            .await;
        let forced_seen = forced_transport.seen.lock().unwrap();
        let forced_request = forced_seen
            .last()
            .expect("the forced opts-aware call reached the transport");
        assert_eq!(
            forced_request
                .body_json
                .get("max_tokens")
                .and_then(serde_json::Value::as_u64),
            Some(321),
            "forced opts-aware wire max_tokens must equal the requested ceiling; body: {}",
            forced_request.body_json
        );
    }

    /// The trait default (used by mocks / non-routing impls) is the byte/4
    /// approximation over the conversation text: `(system + msg text) / 4`,
    /// floored at 1.
    #[tokio::test]
    async fn count_tokens_default_impl_is_byte_over_four_approximation() {
        let mock = crate::test_support::MockApiClient::new(vec![]);
        // system = 8 bytes; one user message of 40 bytes → (8 + 40) / 4 = 12.
        let msgs = vec![protocol::ConversationMessage::user(
            protocol::MessageId::new(),
            "1234567890123456789012345678901234567890".to_string(),
        )];
        let count = OrchestratorApiClient::count_tokens(
            &mock,
            "any-model",
            None,
            Some("12345678"),
            msgs,
            Vec::new(),
        )
        .await
        .expect("default count_tokens ok");
        assert_eq!(count, 12, "(8 system + 40 user) / 4 = 12 tokens");
    }

    // ── Fix 2: RepeatedOverloaded → LlmError::Overloaded { repeated: true } → OrchestratorError ──

    /// Fix 2 end-to-end: a scripted transport that returns 529 three times triggers
    /// the external non-sandbox `DriveStep::RepeatedOverloaded` branch, which the
    /// adapter surfaces as `LlmError::Overloaded { repeated: true }`.  The
    /// `From<LlmError>` conversion on `OrchestratorError` then produces
    /// `OrchestratorError::RepeatedOverloaded` whose Display equals the byte-locked
    /// `"Repeated 529 Overloaded errors"` copy (`errors.ts:166`).
    #[tokio::test]
    async fn repeated_529_terminal_maps_to_byte_locked_copy() {
        use crate::error::{OrchestratorError, REPEATED_529_ERROR_MESSAGE};

        // Transport that always returns 529 overloaded.
        let overloaded_body = serde_json::json!({
            "type": "error",
            "error": {"type": "overloaded_error", "message": "Overloaded"}
        });
        let transport = FakeTransport::always(ProviderResponse::json(529, overloaded_body));
        // make_adapter wires user_type=Some("external") in the UserAgentEnv but
        // resolve_retry_control reads USER_TYPE from ResolveRetryEnv::from_process_env().
        // Set the env vars temporarily to gate allow_fallback + is_external.
        // std::env::set_var is deprecated (Rust 2024) but not removed; acceptable
        // in test-only code.
        #[allow(deprecated)]
        std::env::set_var("USER_TYPE", "external");
        #[allow(deprecated)]
        std::env::set_var("FALLBACK_FOR_ALL_PRIMARY_MODELS", "1");
        #[allow(deprecated)]
        std::env::remove_var("IS_SANDBOX");

        let adapter = make_adapter(transport);
        let llm_result = adapter
            .messages_create(
                "claude-sonnet-4-20250514",
                None,
                None,
                Vec::new(),
                Vec::new(),
            )
            .await;

        // Clean up before any assert that might panic.
        #[allow(deprecated)]
        std::env::remove_var("FALLBACK_FOR_ALL_PRIMARY_MODELS");
        #[allow(deprecated)]
        std::env::remove_var("USER_TYPE");

        // The adapter must return Err(LlmError::Overloaded { repeated: true }).
        match &llm_result {
            Err(LlmError::Overloaded { repeated: true }) => {} // correct
            other => {
                panic!("expected Err(LlmError::Overloaded {{ repeated: true }}), got {other:?}")
            }
        }

        // The OrchestratorError conversion must yield RepeatedOverloaded.
        let orch_err: OrchestratorError = llm_result.unwrap_err().into();
        assert!(
            matches!(orch_err, OrchestratorError::RepeatedOverloaded),
            "OrchestratorError must be RepeatedOverloaded, got {orch_err:?}"
        );
        assert_eq!(
            orch_err.to_string(),
            REPEATED_529_ERROR_MESSAGE,
            "Display must equal the byte-locked copy"
        );
    }

    // ── 3c-T3: LlmResponse.cost populated from cost estimator ─────────────────

    fn make_adapter_with_estimator(transport: Arc<dyn Transport>) -> ProviderApiAdapter {
        use crate::cost_wiring::llm_catalog_from_cost;
        use cost::pricing::PricingCatalog as CostCatalog;
        use llm_runtime::{CostEstimator, PricingPolicy};
        #[allow(deprecated)]
        std::env::set_var("ADAPTER_TEST_KEY", "test-key");
        let cost_cat = CostCatalog::builtin_reference();
        let llm_cat = llm_catalog_from_cost(&cost_cat);
        let estimator = Arc::new(CostEstimator::new(llm_cat, PricingPolicy::MarkUnestimated));

        let client = Arc::new(
            DefaultLlmClient::from_config(ClientConfig {
                providers: vec![ProviderProfile {
                    wire_profile: None,
                    regions: llm_runtime::Region::all(),
                    provider_id: ProviderId::AnthropicFirstParty,
                    profile_name: "anthropic".to_string(),
                    base_url: "https://api.anthropic.com".to_string(),
                    protocol: ProtocolFamily::AnthropicMessages,
                    auth: AuthStrategy::ApiKey,
                    credential: CredentialConfig::Env {
                        var: "ADAPTER_TEST_KEY".to_string(),
                    },
                    models: vec![ModelProfile {
                        display_model: "claude-sonnet-4-20250514".to_string(),
                        request_model: "claude-sonnet-4-20250514".to_string(),
                        billing_model: "claude-sonnet-4".to_string(),
                        aliases: vec!["claude".to_string()],
                        description: None,
                        metadata: Default::default(),
                        capabilities: Capabilities {
                            streaming: true,
                            tools: true,
                            reasoning: true,
                            ..Default::default()
                        },
                    }],
                    pricing: PricingConfig::default(),
                    signing: None,
                    azure: None,
                    supports_websockets: false,
                    supports_websocket_compression: false,
                    websocket_connect_timeout_ms: None,
                    vision_delegate: None,
                    connection: Default::default(),
                }],
            })
            .expect("client"),
        );
        ProviderApiAdapter::new(Arc::new(ApiService::new_with_estimator(
            client,
            transport,
            SubscriberState::default(),
            UserAgentEnv {
                user_type: Some("external".to_string()),
                entrypoint: Some("cli".to_string()),
                ..Default::default()
            },
            "0.0.0",
            None,
            None,
            Some(estimator),
        )))
    }

    /// 3c-T3: adapter populates response.cost for a priced model.
    ///
    /// claude-sonnet-4 billing_model → catalog hit → cost is Some(estimate with
    /// total_cost_usd present).
    #[tokio::test]
    async fn cost_populated_for_priced_model() {
        let response_json = serde_json::json!({
            "id": "msg_cost_test",
            "model": "claude-sonnet-4-20250514",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1_000_000, "output_tokens": 1_000_000}
        });
        let transport = FakeTransport::always(ProviderResponse::json(200, response_json));
        let adapter = make_adapter_with_estimator(transport);
        let resp = adapter
            .messages_create(
                "claude-sonnet-4-20250514",
                None,
                None,
                Vec::new(),
                Vec::new(),
            )
            .await
            .expect("ok");
        let cost = resp.cost.expect("cost must be Some for a priced model");
        let total = cost.total_cost_usd.expect("total_cost_usd must be Some");
        // claude-sonnet-4: input 3_000 nano → 3.0 usd/M × 1M + output 15_000 → 15.0 × 1M = 18.0
        assert!(
            (total - 18.0).abs() < 1e-9,
            "expected total $18.0 for 1M in + 1M out at $3/$15, got ${total}"
        );
    }

    /// 3c-T3: unknown billing model → cost stays None (no error).
    ///
    /// The adapter uses a model profile whose billing_model ("claude-sonnet-4")
    /// IS in the catalog; to test the None path we use a profile with a
    /// billing_model that has no entry.
    #[tokio::test]
    async fn cost_none_for_unpriced_model() {
        // Build an adapter with an estimator but a billing model not in the catalog.
        use crate::cost_wiring::llm_catalog_from_cost;
        use cost::pricing::PricingCatalog as CostCatalog;
        use llm_runtime::{CostEstimator, PricingPolicy};
        #[allow(deprecated)]
        std::env::set_var("ADAPTER_TEST_KEY2", "test-key");
        let cost_cat = CostCatalog::builtin_reference();
        let llm_cat = llm_catalog_from_cost(&cost_cat);
        let estimator = Arc::new(CostEstimator::new(llm_cat, PricingPolicy::MarkUnestimated));

        let client = Arc::new(
            DefaultLlmClient::from_config(ClientConfig {
                providers: vec![ProviderProfile {
                    wire_profile: None,
                    regions: llm_runtime::Region::all(),
                    provider_id: ProviderId::AnthropicFirstParty,
                    profile_name: "anthropic".to_string(),
                    base_url: "https://api.anthropic.com".to_string(),
                    protocol: ProtocolFamily::AnthropicMessages,
                    auth: AuthStrategy::ApiKey,
                    credential: CredentialConfig::Env {
                        var: "ADAPTER_TEST_KEY2".to_string(),
                    },
                    models: vec![ModelProfile {
                        display_model: "claude-future-9999".to_string(),
                        request_model: "claude-future-9999".to_string(),
                        // billing_model not in any catalog entry
                        billing_model: "claude-future-9999".to_string(),
                        aliases: vec![],
                        description: None,
                        metadata: Default::default(),
                        capabilities: Capabilities {
                            reasoning: true,
                            ..Default::default()
                        },
                    }],
                    pricing: PricingConfig::default(),
                    signing: None,
                    azure: None,
                    supports_websockets: false,
                    supports_websocket_compression: false,
                    websocket_connect_timeout_ms: None,
                    vision_delegate: None,
                    connection: Default::default(),
                }],
            })
            .expect("client"),
        );
        let response_json = serde_json::json!({
            "id": "msg_unpriced",
            "model": "claude-future-9999",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 100, "output_tokens": 50}
        });
        let transport = FakeTransport::always(ProviderResponse::json(200, response_json));
        let adapter = ProviderApiAdapter::new(Arc::new(ApiService::new_with_estimator(
            client,
            transport,
            SubscriberState::default(),
            UserAgentEnv {
                user_type: Some("external".to_string()),
                entrypoint: Some("cli".to_string()),
                ..Default::default()
            },
            "0.0.0",
            None,
            None,
            Some(estimator),
        )));
        let resp = adapter
            .messages_create("claude-future-9999", None, None, Vec::new(), Vec::new())
            .await
            .expect("ok");
        assert!(
            resp.cost.is_none(),
            "unpriced billing_model must leave cost = None; got {:?}",
            resp.cost
        );
    }

    /// 3c-T3: cost tracker recording is unchanged (existing CostTracker tests still pass).
    ///
    /// When no estimator is wired, response.cost stays None — backward-compat.
    #[tokio::test]
    async fn no_estimator_leaves_cost_none() {
        let response_json = serde_json::json!({
            "id": "msg_no_est",
            "model": "claude-sonnet-4-20250514",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 100, "output_tokens": 50}
        });
        let transport = FakeTransport::always(ProviderResponse::json(200, response_json));
        // make_adapter wires None estimator (ApiService::new default path)
        let adapter = make_adapter(transport);
        let resp = adapter
            .messages_create(
                "claude-sonnet-4-20250514",
                None,
                None,
                Vec::new(),
                Vec::new(),
            )
            .await
            .expect("ok");
        assert!(resp.cost.is_none(), "no estimator → cost must be None");
    }

    // ── Task 5 Part B: OrchestratorApiClient::last_rate_limit_info ──────────────

    /// `OrchestratorApiClient::last_rate_limit_info` returns the adapter's stored
    /// rate-limit info mapped into a `platform_api::RateLimitSnapshot`.
    ///
    /// After a 2xx response with unified headers the snapshot must carry all
    /// three fields: `rate_limit_type`, `overage_status`, and
    /// `overage_disabled_reason`.
    #[tokio::test]
    async fn orchestrator_api_client_last_rate_limit_info_returns_stored_info() {
        let mut headers = BTreeMap::new();
        headers.insert(
            "anthropic-ratelimit-unified-representative-claim".to_string(),
            "five_hour".to_string(),
        );
        headers.insert(
            "anthropic-ratelimit-unified-overage-status".to_string(),
            "allowed_warning".to_string(),
        );
        headers.insert(
            "anthropic-ratelimit-unified-overage-disabled-reason".to_string(),
            "out_of_credits".to_string(),
        );

        let transport = FakeTransport::always(ProviderResponse {
            status: 200,
            headers,
            body_json: ok_response_json(),
            request_id: None,
        });
        let adapter = make_adapter(transport);
        let _ = adapter
            .messages_create(
                "claude-sonnet-4-20250514",
                None,
                None,
                Vec::new(),
                Vec::new(),
            )
            .await
            .expect("ok");

        // Via the OrchestratorApiClient trait method (RateLimitSnapshot).
        let snapshot = OrchestratorApiClient::last_rate_limit_info(&adapter)
            .expect("must be Some after 2xx with unified headers");
        assert_eq!(
            snapshot.rate_limit_type.as_deref(),
            Some("five_hour"),
            "rate_limit_type must round-trip through the snapshot"
        );
        assert_eq!(
            snapshot.overage_status.as_deref(),
            Some("allowed_warning"),
            "overage_status must round-trip through the snapshot"
        );
        assert_eq!(
            snapshot.overage_disabled_reason.as_deref(),
            Some("out_of_credits"),
            "overage_disabled_reason must round-trip through the snapshot"
        );
    }

    /// `OrchestratorApiClient::last_rate_limit_info` returns `None` before any
    /// response with unified headers.
    #[tokio::test]
    async fn orchestrator_api_client_last_rate_limit_info_none_initially() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);
        let _ = adapter
            .messages_create(
                "claude-sonnet-4-20250514",
                None,
                None,
                Vec::new(),
                Vec::new(),
            )
            .await
            .expect("ok");

        // No unified headers → trait method also returns None.
        assert!(
            OrchestratorApiClient::last_rate_limit_info(&adapter).is_none(),
            "trait method must return None when adapter has no unified header snapshot"
        );
    }

    #[test]
    fn model_supports_fast_mode_gates_on_opus_fast_tier() {
        // The 2.1.220 catalog blob @225785080 lists `fast_mode` for opus-4-7:
        // `capabilities:["effort","max_effort","xhigh_effort",
        // "adaptive_thinking","context_management","fast_mode"]`. The previous
        // comment here asserted the opposite ("2.1.219 removed opus-4-7 from
        // fast mode") and the assertion below encoded it — a specific-sounding
        // claim about the oracle that the oracle does not support. The
        // env-block prompt agrees: "available on Opus 5/4.8/4.7." (@113736244).
        assert!(model_supports_fast_mode("claude-opus-4-8"));
        assert!(model_supports_fast_mode("claude-opus-4-7"));
        assert!(model_supports_fast_mode("claude-opus-5"));
        assert!(model_supports_fast_mode("CLAUDE-OPUS-4-8"));
        assert!(model_supports_fast_mode("us.anthropic.claude-opus-5-v1:0"));
        assert!(!model_supports_fast_mode("claude-sonnet-4-20250514"));
        assert!(!model_supports_fast_mode("claude-opus-4-1"));
        assert!(!model_supports_fast_mode("gpt-5.2"));
    }

    #[test]
    fn live_effort_replaces_and_clears_the_startup_value() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport).with_initial_effort(Some(serde_json::json!("low")));
        assert_eq!(adapter.current_effort(), Some(serde_json::json!("low")));

        OrchestratorApiClient::set_effort(&adapter, Some(serde_json::json!("high")));
        assert_eq!(adapter.current_effort(), Some(serde_json::json!("high")));

        OrchestratorApiClient::set_effort(&adapter, None);
        assert_eq!(adapter.current_effort(), None);
    }
    struct HostedSdkTransport {
        seen: Mutex<Vec<llm_runtime::services::sdk::HttpRequest>>,
        partial: bool,
    }
    #[async_trait::async_trait]
    impl llm_runtime::Transport for HostedSdkTransport {
        async fn send(
            &self,
            request: llm_runtime::services::sdk::HttpRequest,
        ) -> Result<
            llm_runtime::services::sdk::StreamResponse,
            llm_runtime::services::sdk::protocol::LlmError,
        > {
            use futures::StreamExt;
            use serde_json::json;
            self.seen.lock().unwrap().push(request);
            let mut events = vec![
                json!({"type":"message_start","message":{"id":"msg_search","model":"claude-sonnet-4-20250514","role":"assistant","content":[],"usage":{"input_tokens":7,"output_tokens":0}}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srv","name":"web_search","input":{"query":"Rust"}}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","tool_use_id":"srv","content":[{"type":"web_search_result","title":"Rust","url":"https://rust-lang.org","encrypted_content":"opaque"}]}}),
                json!({"type":"content_block_stop","index":1}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":7,"output_tokens":3,"server_tool_use":{"web_search_requests":1}}}),
                json!({"type":"message_stop"}),
            ];
            if self.partial {
                events.truncate(5);
            }
            let mut chunks = events
                .into_iter()
                .map(|event| Ok(format!("data: {event}\n\n").into()))
                .collect::<Vec<_>>();
            if self.partial {
                chunks.push(Err(
                    llm_runtime::services::sdk::protocol::LlmError::StreamInterrupted {
                        message: "connection lost".into(),
                    },
                ));
            }
            Ok(llm_runtime::services::sdk::StreamResponse {
                status: 200,
                headers: vec![],
                body: futures::stream::iter(chunks).boxed(),
            })
        }
    }
    #[tokio::test]
    async fn hosted_search_executes_through_sdk_and_preserves_partial_results_without_replay() {
        use tool_api::HostedWebSearchClient;
        for partial in [false, true] {
            let transport = Arc::new(HostedSdkTransport {
                seen: Mutex::new(vec![]),
                partial,
            });
            let adapter = make_adapter(transport.clone());
            let request = tool_api::HostedSearchRequest {
                model: "claude-sonnet-4-20250514".into(),
                profile: Some("anthropic".into()),
                query: "Rust".into(),
                allowed_domains: vec!["rust-lang.org".into()],
                blocked_domains: vec![],
            };
            assert!(adapter.supports_request(&request));
            let (progress, mut updates) = tokio::sync::mpsc::unbounded_channel();
            let output = adapter.search(request, progress).await.unwrap();
            assert!(output.results.iter().any(|entry|matches!(entry,tool_api::hosted_search::SearchResultEntry::Hit(value) if value.to_string().contains("rust-lang.org"))));
            assert!(updates.try_recv().is_ok());
            if partial {
                assert!(output.results.iter().any(|entry|matches!(entry,tool_api::hosted_search::SearchResultEntry::Text(text) if text.contains("incomplete"))));
            } else {
                assert_eq!(output.input_tokens, 7);
                assert_eq!(output.output_tokens, 3);
                assert_eq!(output.searches, 1);
            }
            let requests = transport.seen.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert!(requests[0]
                .headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("x-api-key") && v == "test-key"));
            let wire: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
            assert_eq!(wire["tools"][0]["allowed_domains"][0], "rust-lang.org");
            assert_eq!(wire["model"], "claude-sonnet-4-20250514");
        }
    }
    #[tokio::test]
    async fn hosted_search_openai_reports_one_call_not_one_per_citation() {
        use llm_runtime::services::sdk;
        use serde_json::json;
        use tool_api::HostedWebSearchClient;
        struct SearchTransport {
            partial: bool,
            sends: std::sync::atomic::AtomicUsize,
        }
        #[async_trait::async_trait]
        impl llm_runtime::Transport for SearchTransport {
            async fn send(
                &self,
                request: sdk::HttpRequest,
            ) -> Result<sdk::StreamResponse, sdk::protocol::LlmError> {
                use futures::StreamExt;
                self.sends
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(body["model"], "gpt-search");
                assert_eq!(body["tools"][0]["type"], "web_search");
                let call = json!({"id":"ws_1","type":"web_search_call","status":"completed","action":{"type":"search","query":"Rust"}});
                let citation = json!({"type":"url_citation","url":"https://rust-lang.org","title":"Rust","start_index":0,"end_index":4});
                let mut events = vec![
                    json!({"type":"response.created","response":{"id":"resp_1","model":"gpt-search"}}),
                    json!({"type":"response.output_item.added","output_index":0,"item":{"id":"ws_1","type":"web_search_call","status":"in_progress"}}),
                    json!({"type":"response.output_item.done","output_index":0,"item":call}),
                    json!({"type":"response.output_text.delta","output_index":1,"content_index":0,"delta":"Rust"}),
                    json!({"type":"response.output_text.annotation.added","output_index":1,"content_index":0,"annotation":citation}),
                    json!({"type":"response.completed","response":{"id":"resp_1","model":"gpt-search","status":"completed","output":[call,{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"Rust","annotations":[citation]}]}],"usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}}}),
                ];
                if self.partial {
                    events.pop(); // No response.completed snapshot arrives.
                    events.push(json!({"type":"response.output_item.done","output_index":1,"item":{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"Rust","annotations":[citation]}]}}));
                }
                let mut chunks = events
                    .into_iter()
                    .map(|event| Ok(format!("data: {event}\n\n").into()))
                    .collect::<Vec<_>>();
                if self.partial {
                    chunks.push(Err(sdk::protocol::LlmError::StreamInterrupted {
                        message: "connection lost after citation".into(),
                    }));
                }
                Ok(sdk::StreamResponse {
                    status: 200,
                    headers: vec![],
                    body: futures::stream::iter(chunks).boxed(),
                })
            }
        }
        for partial in [false, true] {
            let mut profile: ProviderProfile = serde_json::from_value(json!({
            "provider_id":"open_ai","profile_name":"search-openai","base_url":"https://api.openai.com/v1","protocol":"open_ai_responses","auth":"none","credential":{"type":"none"},
            "models":[{"display_model":"gpt-search","request_model":"gpt-search","billing_model":"gpt-search","capabilities":{"streaming":true,"tools":true,"vision":false,"documents":false,"reasoning":false,"structured_output":false}}]
        })).unwrap();
            profile.protocol = ProtocolFamily::OpenAiResponses;
            let mut wire: sdk::protocol::ProviderProfile = serde_json::from_value(json!({
            "provider_id":"openai","profile_name":"search-openai","base_url":"https://api.openai.com/v1","protocol":"open_ai_responses","auth":"none","models":[],"extra":{"web_search":"openai_responses"}
        })).unwrap();
            wire.regions = sdk::protocol::Region::all();
            profile.wire_profile = Some(wire);
            let client = Arc::new(
                DefaultLlmClient::from_config(ClientConfig {
                    providers: vec![profile],
                })
                .unwrap(),
            );
            let transport = Arc::new(SearchTransport {
                partial,
                sends: Default::default(),
            });
            let adapter = ProviderApiAdapter::new(Arc::new(ApiService::new(
                client,
                transport.clone(),
                SubscriberState::default(),
                UserAgentEnv::default(),
                "test",
                None,
                None,
            )));
            let (progress, mut updates) = tokio::sync::mpsc::unbounded_channel();
            let request = tool_api::HostedSearchRequest {
                model: "gpt-search".into(),
                profile: Some("search-openai".into()),
                query: "Rust".into(),
                allowed_domains: vec![],
                blocked_domains: vec![],
            };
            assert!(adapter.supports_request(&request));
            let result = adapter.search(request, progress).await.unwrap();
            assert_eq!(result.searches, 1);
            assert!(updates.try_recv().is_ok());
            assert!(
                updates.try_recv().is_err(),
                "duplicated final output counted twice"
            );
            assert_eq!(
                transport.sends.load(std::sync::atomic::Ordering::Relaxed),
                1,
                "interrupted search must not be resent"
            );
            if !partial {
                assert_eq!((result.input_tokens, result.output_tokens), (7, 3));
            }
            let hits = result
                .results
                .iter()
                .filter_map(|entry| match entry {
                    tool_api::hosted_search::SearchResultEntry::Hit(value) => {
                        value["content"].as_array()
                    }
                    _ => None,
                })
                .flatten()
                .collect::<Vec<_>>();
            assert_eq!(
                hits.len(),
                1,
                "stream and terminal citations must be deduplicated"
            );
            assert_eq!(hits[0]["url"], "https://rust-lang.org");
            assert!(result.results.iter().any(|entry| matches!(entry, tool_api::hosted_search::SearchResultEntry::Text(text) if text == "Rust")));
            assert_eq!(result.results.iter().any(|entry| matches!(entry, tool_api::hosted_search::SearchResultEntry::Text(text) if text.contains("incomplete"))), partial);
        }
    }
    #[tokio::test]
    async fn hosted_search_gemini_reports_queries_once_per_grounding_snapshot() {
        use llm_runtime::services::sdk;
        use serde_json::json;
        use tool_api::HostedWebSearchClient;
        struct SearchTransport {
            interrupted: bool,
            calls: std::sync::atomic::AtomicUsize,
        }
        #[async_trait::async_trait]
        impl llm_runtime::Transport for SearchTransport {
            async fn send(
                &self,
                request: sdk::HttpRequest,
            ) -> Result<sdk::StreamResponse, sdk::protocol::LlmError> {
                use futures::StreamExt;
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                assert!(request.url.contains("gemini-search"));
                assert!(body["tools"][0].get("googleSearch").is_some());
                let grounding = json!({"webSearchQueries":["Rust","Rust docs"],"groundingChunks":[{"web":{"uri":"https://rust-lang.org","title":"Rust"}}]});
                let mut events = vec![
                    json!({"modelVersion":"gemini-search","candidates":[{"content":{"role":"model","parts":[{"text":"Rust"}]},"groundingMetadata":grounding}]}),
                    json!({"candidates":[{"content":{"role":"model","parts":[]},"groundingMetadata":grounding,"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3,"totalTokenCount":10}}),
                ];
                if self.interrupted {
                    events.pop();
                }
                let mut chunks = events
                    .into_iter()
                    .map(|event| Ok(format!("data: {event}\n\n").into()))
                    .collect::<Vec<_>>();
                if self.interrupted {
                    chunks.push(Err(sdk::protocol::LlmError::StreamInterrupted {
                        message: "lost after grounding".into(),
                    }));
                }
                Ok(sdk::StreamResponse {
                    status: 200,
                    headers: vec![],
                    body: futures::stream::iter(chunks).boxed(),
                })
            }
        }
        for interrupted in [false, true] {
            let mut profile: ProviderProfile = serde_json::from_value(json!({
            "provider_id":"gemini","profile_name":"search-google","base_url":"https://generativelanguage.googleapis.com/v1beta","protocol":"gemini_generate_content","auth":"none","credential":{"type":"none"},
            "models":[{"display_model":"gemini-search","request_model":"gemini-search","billing_model":"gemini-search","capabilities":{"streaming":true,"tools":true,"vision":false,"documents":false,"reasoning":false,"structured_output":false}}]
        })).unwrap();
            profile.protocol = ProtocolFamily::GeminiGenerateContent;
            let mut wire: sdk::protocol::ProviderProfile = serde_json::from_value(json!({
            "provider_id":"gemini","profile_name":"search-google","base_url":"https://generativelanguage.googleapis.com/v1beta","protocol":"gemini_generate_content","auth":"none","models":[],"extra":{"web_search":"gemini"}
        })).unwrap();
            wire.regions = sdk::protocol::Region::all();
            profile.wire_profile = Some(wire);
            let client = Arc::new(
                DefaultLlmClient::from_config(ClientConfig {
                    providers: vec![profile],
                })
                .unwrap(),
            );
            let transport = Arc::new(SearchTransport {
                interrupted,
                calls: Default::default(),
            });
            let adapter = ProviderApiAdapter::new(Arc::new(ApiService::new(
                client,
                transport.clone(),
                SubscriberState::default(),
                UserAgentEnv::default(),
                "test",
                None,
                None,
            )));
            let (progress, mut updates) = tokio::sync::mpsc::unbounded_channel();
            let request = tool_api::HostedSearchRequest {
                model: "gemini-search".into(),
                profile: Some("search-google".into()),
                query: "Rust".into(),
                allowed_domains: vec![],
                blocked_domains: vec![],
            };
            assert!(adapter.supports_request(&request));
            let result = adapter.search(request, progress).await.unwrap();
            assert_eq!(result.searches, 2);
            assert!(updates.try_recv().is_ok());
            assert!(updates.try_recv().is_ok());
            assert!(
                updates.try_recv().is_err(),
                "duplicated final output counted twice"
            );
            if !interrupted {
                assert_eq!((result.input_tokens, result.output_tokens), (7, 3));
            }
            assert_eq!(transport.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert_eq!(result.results.iter().any(|entry| matches!(entry, tool_api::hosted_search::SearchResultEntry::Text(text) if text.contains("incomplete"))), interrupted);
            assert!(result.results.iter().any(|entry|matches!(entry,tool_api::hosted_search::SearchResultEntry::Hit(value) if value.to_string().contains("rust-lang.org"))));
        }
    }
}

#[async_trait::async_trait]
impl tool_api::HostedWebSearchClient for ProviderApiAdapter {
    fn supports(&self, model: &str, profile: Option<&str>) -> bool {
        self.service.supports_hosted_search(model, profile)
    }
    fn supports_request(&self, request: &tool_api::HostedSearchRequest) -> bool {
        self.service.supports_hosted_search_config(
            &request.model,
            request.profile.as_deref(),
            &llm_runtime::services::sdk::protocol::WebSearchConfig {
                allowed_domains: request.allowed_domains.clone(),
                blocked_domains: request.blocked_domains.clone(),
                max_uses: self
                    .service
                    .hosted_search_max_uses(&request.model, request.profile.as_deref()),
            },
        )
    }
    async fn search(
        &self,
        input: tool_api::HostedSearchRequest,
        progress: tokio::sync::mpsc::UnboundedSender<()>,
    ) -> Result<tool_api::HostedSearchOutput, tool_api::HostedSearchError> {
        use futures::StreamExt;
        use llm_runtime::services::sdk;
        use llm_runtime::{ContentBlock, LlmEvent};
        let mut request = llm_runtime::LlmRequest::new(input.model);
        request.profile = input.profile;
        request.max_tokens = Some(4096);
        request.query_source = Some("web_search_tool".into());
        request.system = vec![llm_runtime::SystemBlock {
            text: "You are an assistant for performing a web search tool use".into(),
            cache_control: None,
        }];
        request.messages = vec![llm_runtime::Message {
            role: "user".into(),
            content: vec![ContentBlock::Text {
                text: format!("Perform a web search for the query: {}", input.query),
                cache_control: None,
            }],
        }];
        request.hosted_tools = vec![sdk::protocol::HostedTool::WebSearch(
            sdk::protocol::WebSearchConfig {
                allowed_domains: input.allowed_domains,
                blocked_domains: input.blocked_domains,
                max_uses: self
                    .service
                    .hosted_search_max_uses(&request.model, request.profile.as_deref()),
            },
        )];
        let usage = Arc::new(std::sync::Mutex::new(llm_runtime::Usage::default()));
        let searches = Arc::new(std::sync::atomic::AtomicU64::new(0));
        // A failed stream may never deliver its terminal metadata snapshot.
        // Retain normalized citations independently of completed text blocks.
        let citations = Arc::new(std::sync::Mutex::new(
            sdk::protocol::WebSearchResult::default(),
        ));
        let observed_citations = citations.clone();
        let u = usage.clone();
        let n = searches.clone();
        let mut search_progress = sdk::hosted_search::SearchProgress::default();
        let stream = self
            .service
            .stream_request(request)
            .await
            .map_err(hosted_search_error)?
            .inspect(move |event| match event {
                Ok(LlmEvent::WebSearch { result }) => {
                    observed_citations
                        .lock()
                        .expect("search citations")
                        .citations
                        .extend(result.citations.iter().cloned());
                    let added = search_progress.observe(result);
                    n.fetch_add(added, std::sync::atomic::Ordering::Relaxed);
                    for _ in 0..added {
                        let _ = progress.send(());
                    }
                }
                Ok(LlmEvent::ContentBlockStart { content_block, .. }) => {
                    if sdk::hosted_search::search_started(
                        &serde_json::to_value(content_block).unwrap_or_default(),
                    ) {
                        n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let _ = progress.send(());
                    }
                }
                Ok(LlmEvent::MessageStart { response }) | Ok(LlmEvent::Completed { response }) => {
                    *u.lock().expect("search usage") = response.usage.clone();
                }
                Ok(LlmEvent::MessageDelta {
                    usage: Some(usage), ..
                }) => {
                    *u.lock().expect("search usage") = usage.clone();
                }
                _ => {}
            })
            .boxed();
        let (blocks, metadata, partial) =
            match llm_runtime::stream_accumulator::accumulate_stream_salvaging(stream).await {
                Ok(response) => (response.content, response.provider_metadata, None),
                Err((partial, error))
                    if !partial.is_empty()
                        || !citations
                            .lock()
                            .expect("search citations")
                            .citations
                            .is_empty() =>
                {
                    (partial, serde_json::Value::Null, Some(error.to_string()))
                }
                Err((_, error)) => return Err(hosted_search_error(error)),
            };
        let native = blocks
            .iter()
            .map(|block| serde_json::to_value(block).unwrap_or_default())
            .collect::<Vec<_>>();
        let mut results = sdk::hosted_search::parse_response_content(&native);
        let mut observations = metadata
            .get("llm_client")
            .and_then(|v| v.get("web_search"))
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let captured = std::mem::take(&mut *citations.lock().expect("search citations"));
        if !captured.citations.is_empty() {
            observations.push(serde_json::to_value(captured).expect("normalized search citations"));
        }
        // The SDK projects both sources together and deduplicates their URLs.
        sdk::hosted_search::append_citations(
            &mut results,
            Some(&serde_json::Value::Array(observations)),
        );
        if partial.is_some() {
            results.push(sdk::hosted_search::SearchResultEntry::Text(
                "Search response was interrupted; these results are incomplete.".into(),
            ));
        }
        let usage = usage.lock().expect("search usage");
        Ok(tool_api::HostedSearchOutput {
            results,
            searches: usage
                .server_tool_use
                .as_ref()
                .map(|u| u.web_search_requests)
                .unwrap_or_else(|| searches.load(std::sync::atomic::Ordering::Relaxed)),
            input_tokens: usage.billable_tokens.input,
            output_tokens: usage.billable_tokens.output,
        })
    }
}

fn hosted_search_error(error: llm_runtime::LlmError) -> tool_api::HostedSearchError {
    use llm_runtime::LlmError;
    let http_status = error.http_status().or(match &error {
        LlmError::RateLimited { .. } => Some(429),
        LlmError::Overloaded { .. } => Some(529),
        LlmError::RequestTooLarge => Some(413),
        _ => None,
    });
    tool_api::HostedSearchError {
        http_status,
        timeout: matches!(error, LlmError::TransportTimeout { .. }),
        message: error.to_string(),
    }
}

#[cfg(test)]
mod hosted_error_tests {
    use super::*;
    #[test]
    fn hosted_search_errors_keep_status_and_timeout() {
        for (error, status, timeout) in [
            (
                llm_runtime::LlmError::RateLimited {
                    retry_after: None,
                    scope: None,
                },
                Some(429),
                false,
            ),
            (
                llm_runtime::LlmError::Authentication {
                    message: "401 unauthorized".into(),
                },
                Some(401),
                false,
            ),
            (
                llm_runtime::LlmError::TransportTimeout {
                    message: "deadline".into(),
                },
                None,
                true,
            ),
            (
                llm_runtime::LlmError::Transport {
                    message: "disconnected".into(),
                },
                None,
                false,
            ),
        ] {
            let mapped = hosted_search_error(error);
            assert_eq!(mapped.http_status, status);
            assert_eq!(mapped.timeout, timeout);
        }
    }
}
