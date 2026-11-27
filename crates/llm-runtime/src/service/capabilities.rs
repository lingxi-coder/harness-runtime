//! Model routing, capability and budget queries.

use crate::service::anthropic::endpoint::apply_native_first_party_base_url_assumption;
use crate::service::usage::json_payload_u64_at;
use crate::service::ApiService;
use crate::{LlmError, LlmRequest, MediaRoute};

impl ApiService {
    /// Prompt routing uses the exact request-local catalog, without credential lookup.
    pub fn prompt_target(
        &self,
        model: &str,
        profile: Option<&str>,
        mode: lingxi_core::host::session_context::ConversationMode,
    ) -> Result<lingxi_core::host::prompt_policy::PromptTarget, LlmError> {
        self.client().prompt_target(model, profile, mode)
    }

    /// Protocol selected for the model's canonical request and history adapter.
    pub fn protocol_for_model(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<crate::ProtocolFamily, LlmError> {
        self.client().protocol_for_model(model, profile)
    }

    /// Reasoning controls follow the selected route's canonical provider and
    /// wire protocol, independently of its user-defined connection label.
    pub fn reasoning_provider_for_model(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Option<String> {
        let client = self.client();
        let target = client
            .prompt_target(
                model,
                profile,
                lingxi_core::host::session_context::ConversationMode::Code,
            )
            .ok()?;
        let protocol = client.protocol_for_model(model, profile).ok()?;
        use crate::ProtocolFamily;
        Some(match protocol {
            ProtocolFamily::AnthropicMessages
            | ProtocolFamily::BedrockClaude
            | ProtocolFamily::VertexClaude
            | ProtocolFamily::FoundryClaude => "anthropic".to_owned(),
            ProtocolFamily::OpenAiResponses | ProtocolFamily::AzureOpenAi => "openai".to_owned(),
            ProtocolFamily::GeminiGenerateContent
            | ProtocolFamily::GeminiInteractions
            | ProtocolFamily::VertexGemini => "gemini".to_owned(),
            ProtocolFamily::OpenAiChat => target.provider_id,
        })
    }

    /// Account epoch used to fence model-validation probes. This is the
    /// existing account observer generation, so token refreshes within the
    /// same account do not invalidate a probe or its cache.
    pub fn model_validation_account_generation(&self) -> u64 {
        self.anthropic.fast_availability.generation()
    }

    /// Native process-wide ANTHROPIC_CUSTOM_MODEL_OPTION source, kept as UTF-16 units.
    pub fn model_validation_custom_model_option(&self) -> Option<Vec<u16>> {
        std::env::var("ANTHROPIC_CUSTOM_MODEL_OPTION")
            .ok()
            .map(|value| value.encode_utf16().collect())
    }

    /// A bounded increase based on complete usage and a documented total-token
    /// contract. Unknown accounting never becomes a guessed retry budget.
    pub fn next_output_budget(
        &self,
        model: &str,
        profile: Option<&str>,
        usage: &crate::ExecutionUsage,
    ) -> Result<Option<u32>, LlmError> {
        if std::env::var("LINGXI_MAX_OUTPUT_TOKENS")
            .ok()
            .is_some_and(|raw| {
                let n = lingxi_core::host::env::parse_int_env(&raw);
                !n.is_nan() && n > 0.0
            })
        {
            return Ok(None);
        }
        if usage.report.complete().is_none() {
            return Ok(None);
        }
        let provider_metadata = usage
            .provider_metadata
            .payload_at("/stream")
            .unwrap_or_else(|| usage.provider_metadata.clone());
        let route = self.client().resolve_media_route(model, profile)?.main;
        let observation = provider_metadata.payload_at("/llm_client/output_budget");
        let different_route = observation.as_ref().is_some_and(|budget| {
            budget
                .string_at("/profile")
                .is_some_and(|profile| profile != route.profile_name)
                || budget
                    .string_at("/model")
                    .is_some_and(|model| model != route.request_model)
        });
        if different_route
            || provider_metadata
                .string_at("/llm_client/executed_profile")
                .is_some_and(|profile| profile != route.profile_name)
        {
            return Ok(None);
        }
        let profiles = self.client().provider_service_profiles();
        let Some(connection) = profiles
            .iter()
            .find(|p| p.profile_name == route.profile_name)
        else {
            return Ok(None);
        };
        // A request budget is recoverable after confirmed output exhaustion.
        // The SDK bounds growth by this route's model and remaining context;
        // administrative LINGXI_MAX_OUTPUT_TOKENS remains a hard ceiling.
        let issued = observation
            .as_ref()
            .and_then(|value| json_payload_u64_at(value, "/value"))
            .or_else(|| {
                observation
                    .as_ref()
                    .and_then(|value| json_payload_u64_at(value, "/provider_default"))
            });

        let Some(selected) = connection.models.iter().find(|row| {
            row.request_model == route.request_model
                && row.display_model == route.display_model
                && row.billing_model == route.pricing_model.billing_model
        }) else {
            return Ok(None);
        };
        let context = lingxi_llm_client::CodecContext::for_model(
            connection,
            selected,
            lingxi_llm_client::RequestMode::Complete,
        );
        let effective_context = selected.metadata.context_window_tokens;
        Ok(lingxi_llm_client::client::recovery_output_budget(
            context.profile(),
            &route.request_model,
            &usage.report,
            issued,
            effective_context,
        ))
    }

    /// Resolve the selected connection's context policy; no bare-name capacity cache.
    pub fn resolve_context(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<crate::ResolvedContext, LlmError> {
        self.resolve_context_for(
            model,
            profile,
            &lingxi_llm_client::client::context::ContextRequest {
                thinking: self
                    .thinking()
                    .ok()
                    .map(|t| t != crate::model::thinking::ThinkingConfig::Disabled),
                ..Default::default()
            },
            None,
            None,
        )
    }

    fn resolve_context_for(
        &self,
        model: &str,
        profile: Option<&str>,
        context_request: &lingxi_llm_client::client::context::ContextRequest,
        output_tokens: Option<u32>,
        service_tier: Option<lingxi_llm_client::protocol::ServiceTier>,
    ) -> Result<crate::ResolvedContext, LlmError> {
        let route = self.client().resolve_media_route(model, profile)?.main;
        let profiles = self.client().provider_service_profiles();
        let connection = profiles
            .iter()
            .find(|p| p.profile_name == route.profile_name)
            .ok_or_else(|| LlmError::InvalidRequest {
                message: "context route profile is missing".into(),
            })?;
        let selected = connection
            .models
            .iter()
            .find(|m| {
                m.request_model == route.request_model
                    && m.display_model == route.display_model
                    && m.billing_model == route.pricing_model.billing_model
            })
            .ok_or_else(|| LlmError::InvalidRequest {
                message: "context route model is missing".into(),
            })?;
        let codec = lingxi_llm_client::CodecContext::for_model(
            connection,
            selected,
            lingxi_llm_client::RequestMode::Complete,
        );
        let output = lingxi_llm_client::client::resolve_output_budget(
            codec.profile(),
            &selected.request_model,
            output_tokens,
        )
        .ok()
        .and_then(|budget| budget.value);
        lingxi_llm_client::client::context::resolve_context(
            connection,
            selected,
            context_request,
            output,
            &lingxi_llm_client::protocol::PricingContext {
                service_tier,
                unix_seconds: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_secs()),
                ..Default::default()
            },
        )
        .map_err(|e| LlmError::InvalidRequest {
            message: e.to_string(),
        })
    }

    /// Estimate selected-route input without credentials or transport. Host
    /// preflight callers must use the same schema/replay projection as generation.
    pub fn estimate_input_tokens(&self, request: &LlmRequest) -> Result<u64, LlmError> {
        self.client().estimate_input_tokens_for_route(request)
    }

    /// Check each physical route before generation. Only a provider count may
    /// produce ContextOverflow; approximations can enforce an application soft
    /// budget but are explicitly identified as estimates in that local error.
    pub(super) async fn ensure_context_fit(
        &self,
        request: &LlmRequest,
        prior_dispatch: bool,
    ) -> Result<(), LlmError> {
        use lingxi_llm_client::protocol::{ContextPolicy, ThinkingMode};
        // An opaque/overridden native model source may not have a catalog row.
        // Do not borrow the route selector's limits for another wire identity.
        if let Some(source) = request.input.model_source.as_ref() {
            if source.string_at("").as_deref() != Some(request.input.model.as_str()) {
                return Ok(());
            }
        }
        let context = self.resolve_context_for(
            &request.input.model,
            request.profile.as_deref(),
            &lingxi_llm_client::client::context::ContextRequest {
                policy: (request.execution.query_source.as_deref() == Some("compaction"))
                    .then_some(ContextPolicy::Long),
                thinking: request
                    .input
                    .thinking
                    .as_ref()
                    .map(|t| t.mode != Some(ThinkingMode::Disabled)),
                ..Default::default()
            },
            request.input.max_tokens,
            request.input.service_tier,
        )?;
        let Some(budget) = context.effective_input_budget else {
            return Ok(());
        };
        let estimate = self.client().estimate_input_tokens_for_route(request)?;
        if estimate < budget.saturating_sub(13_000.min(budget / 10)) {
            return Ok(());
        }
        // Exact counting sends the prompt and tools. Output-limited invocations
        // reserve live host authorization and dispatch receipts for generation.
        if self.scope_output_quota.is_some() {
            return crate::model::context_admission::check(&context, estimate, None);
        }
        let mut counted = request.clone();
        counted.profile = Some(context.profile_name.clone());
        let scope = crate::model::retry_scope::ModelCallRetryScope::current_or_new();
        let exact = crate::model::count_tokens::try_count_tokens_exact_in_scope(
            self.client().as_ref(),
            self.transport.clone(),
            &counted,
            Some(&scope),
            || request.execution.check_dispatch_admission(prior_dispatch),
        )
        .await?;
        crate::model::context_admission::check(&context, estimate, exact)
    }

    pub fn resolve_media_route(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<MediaRoute, LlmError> {
        self.client().resolve_media_route(model, profile)
    }

    /// Redacted source from the selected route's host credential resolver.
    pub async fn credential_source(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<crate::CredentialSource, LlmError> {
        self.client().credential_source(model, profile).await
    }

    /// Resolve classifier model gates while fencing credential reads against
    /// the same host account generation used by prompt-cache preparation.
    pub async fn classifier_model_route_facts(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<crate::model::classifier_model_policy::ClassifierModelRouteFacts, LlmError> {
        for _ in 0..2 {
            let epoch = self.anthropic.prompt_cache_overage.account_epoch();
            let mut facts = self
                .client()
                .classifier_model_route_facts(model, profile)
                .await?;
            let assume_first_party_base_url = self
                .anthropic
                .native_system_prefix
                .as_ref()
                .is_some_and(|prefix| prefix.assume_first_party_base_url);
            apply_native_first_party_base_url_assumption(&mut facts, assume_first_party_base_url);
            if epoch == self.anthropic.prompt_cache_overage.account_epoch() {
                return Ok(facts);
            }
        }
        Err(LlmError::Authentication {
            message: "account changed while resolving classifier model route".into(),
        })
    }

    /// Native first-party route identity, without credential or network work.
    pub fn is_first_party_route(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<bool, LlmError> {
        self.client().is_first_party_route(model, profile)
    }

    /// Enumerate available `provider/model` ids + `@aliases` for `/model`'s list
    /// mode — the available-model ids captured from the client registry at
    /// construction. Backs the orchestrator's
    /// `OrchestratorApiClient::available_models`.
    #[must_use]
    pub fn available_models(&self) -> Vec<String> {
        self.client()
            .available_models()
            .into_iter()
            .map(|m| m.display_model)
            .collect()
    }

    /// Enumerate full provider-specific model metadata for picker surfaces.
    #[must_use]
    pub fn model_listings(&self) -> Vec<crate::ModelListing> {
        self.client().available_models()
    }
}
