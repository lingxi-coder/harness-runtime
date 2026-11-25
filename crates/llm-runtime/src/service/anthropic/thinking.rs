//! Host thinking controls for Claude routes.

use crate::model::retry_scope::ModelCallRetryScope;
use crate::service::anthropic::endpoint::{
    is_native_main_anthropic_route, native_adaptive_provider_default,
    native_request_model_identity, native_request_model_name_for_option_lookup,
    native_tbc_effective_base_url, native_tbc_first_party_assume,
    native_tbc_first_party_base_url_is_defined, native_tbc_first_party_kxt,
    native_tbc_first_party_sl, native_tbc_first_party_tf,
};
use crate::service::request::{request_query_source_category, QuerySourceCategory};
use crate::service::ApiService;
use crate::{LlmError, LlmRequest};

#[derive(Debug, Clone, Copy)]
struct NativeHqQeeGate {
    original_status: u16,
    u6: Option<bool>,
    planned_tbc_header: bool,
    actual_tbc_header: bool,
    requested_tbc: Option<bool>,
    g_b: Option<bool>,
    j_k: Option<bool>,
}

impl NativeHqQeeGate {
    fn should_retry(self) -> bool {
        self.original_status == 400
            && self.u6 == Some(false)
            && self.planned_tbc_header
            && self.actual_tbc_header
            && self.requested_tbc == Some(true)
            && self.g_b == Some(false)
            && self.j_k == Some(false)
    }
}

fn native_hq_n_b_claim_from_facts(
    speed_model_matches_request: Option<bool>,
    refusal_fallback_target: Option<bool>,
    previously_fast_rejected: Option<bool>,
) -> Option<bool> {
    match speed_model_matches_request {
        Some(false) => Some(false),
        Some(true) => match (refusal_fallback_target, previously_fast_rejected) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (Some(false), Some(false)) => Some(false),
            _ => None,
        },
        None => None,
    }
}

fn native_hq_j_k_claim_from_facts(k_ye: Option<bool>, n_b: Option<bool>) -> Option<bool> {
    match (k_ye, n_b) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod native_hq_qee_gate_tests {
    use super::NativeHqQeeGate;

    fn base() -> NativeHqQeeGate {
        NativeHqQeeGate {
            original_status: 400,
            u6: Some(false),
            planned_tbc_header: true,
            actual_tbc_header: true,
            requested_tbc: Some(true),
            g_b: Some(false),
            j_k: Some(false),
        }
    }

    #[test]
    fn broad_qee_requires_every_source_guard_to_be_proven_negative() {
        assert!(base().should_retry());
        assert!(!NativeHqQeeGate {
            g_b: None,
            ..base()
        }
        .should_retry());
        assert!(!NativeHqQeeGate {
            j_k: None,
            ..base()
        }
        .should_retry());
        assert!(!NativeHqQeeGate {
            g_b: Some(true),
            ..base()
        }
        .should_retry());
        assert!(!NativeHqQeeGate {
            j_k: Some(true),
            ..base()
        }
        .should_retry());
    }

    #[test]
    fn broad_qee_is_original_400_only_and_outside_u6_only() {
        assert!(!NativeHqQeeGate {
            original_status: 422,
            ..base()
        }
        .should_retry());
        assert!(!NativeHqQeeGate {
            u6: Some(true),
            ..base()
        }
        .should_retry());
        assert!(!NativeHqQeeGate { u6: None, ..base() }.should_retry());
    }

    #[test]
    fn broad_qee_requires_both_planned_and_final_header_membership() {
        assert!(!NativeHqQeeGate {
            planned_tbc_header: false,
            ..base()
        }
        .should_retry());
        assert!(!NativeHqQeeGate {
            actual_tbc_header: false,
            ..base()
        }
        .should_retry());
        assert!(!NativeHqQeeGate {
            requested_tbc: None,
            ..base()
        }
        .should_retry());
    }

    #[test]
    fn native_nb_needs_speed_model_match_and_either_local_fallback_fact() {
        assert_eq!(
            super::native_hq_n_b_claim_from_facts(Some(false), Some(true), Some(true)),
            Some(false)
        );
        assert_eq!(
            super::native_hq_n_b_claim_from_facts(Some(true), Some(true), Some(false)),
            Some(true)
        );
        assert_eq!(
            super::native_hq_n_b_claim_from_facts(Some(true), Some(false), Some(true)),
            Some(true)
        );
        assert_eq!(
            super::native_hq_n_b_claim_from_facts(Some(true), Some(false), Some(false)),
            Some(false)
        );
        assert_eq!(
            super::native_hq_n_b_claim_from_facts(Some(true), None, Some(false)),
            None
        );
        assert_eq!(
            super::native_hq_n_b_claim_from_facts(None, Some(false), Some(false)),
            None
        );
    }

    #[test]
    fn native_jk_positive_n_b_does_not_require_hrn15_evidence() {
        assert_eq!(
            super::native_hq_j_k_claim_from_facts(None, Some(true)),
            Some(true)
        );
        assert_eq!(
            super::native_hq_j_k_claim_from_facts(Some(true), None),
            Some(true)
        );
        assert_eq!(
            super::native_hq_j_k_claim_from_facts(Some(false), Some(false)),
            Some(false)
        );
        assert_eq!(
            super::native_hq_j_k_claim_from_facts(None, Some(false)),
            None
        );
    }
}

#[derive(Debug, Default)]
pub(in crate::service) struct NativeThinkingResumptionRefusalLatch(std::sync::atomic::AtomicBool);

impl NativeThinkingResumptionRefusalLatch {
    pub(in crate::service) fn observe(&self, error: &LlmError) {
        if error
            .native_anthropic_error_facts()
            .is_some_and(|facts| facts.thinking_resumption_beta_refused)
        {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    pub(in crate::service) fn is_refused(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
mod native_thinking_resumption_refusal_latch_tests {
    use super::NativeThinkingResumptionRefusalLatch;
    use crate::LlmError;
    use lingxi_llm_client::protocol::{
        NativeAnthropicErrorFacts, ProtocolFamily, ProviderErrorText, ProviderId,
    };

    fn error(exact_beta_refusal: bool, broad_refusal: bool) -> LlmError {
        LlmError::ProviderErrorText {
            source: Box::new(LlmError::InvalidRequest {
                message: "fixture provider error".into(),
            }),
            text: ProviderErrorText::new(
                "fixture provider error".into(),
                "fixture provider error".encode_utf16().collect(),
            )
            .expect("fixture text is valid UTF-16"),
            native_anthropic_facts: Some(NativeAnthropicErrorFacts {
                provider_id: ProviderId::from("anthropic"),
                protocol: ProtocolFamily::AnthropicMessages,
                request_authority: Some("api.anthropic.com".into()),
                thinking_resumption_refused: broad_refusal,
                thinking_resumption_beta_requested: Some(true),
                thinking_resumption_beta_refused: exact_beta_refusal,
                native_thinking_type_rejection: None,
                native_thinking_binding_controls_beta_requested: None,
                native_thinking_binding_controls_error_evidence: None,
                native_hq_model_source: None,
                native_hq_model_provider_family: None,
                native_hq_known_error_evidence: None,
                native_hq_hrn15_evidence: None,
                response: None,
            }),
        }
    }

    #[test]
    fn only_exact_beta_refusal_sets_a_sticky_service_latch() {
        let latch = NativeThinkingResumptionRefusalLatch::default();
        assert!(!latch.is_refused());

        latch.observe(&error(false, true));
        assert!(!latch.is_refused(), "the broad refusal fact is not enough");

        latch.observe(&error(true, false));
        assert!(latch.is_refused());

        latch.observe(&error(false, false));
        assert!(
            latch.is_refused(),
            "the service-lifetime latch never clears"
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(in crate::service) struct NativeThinkingTypeKey {
    provider_id: crate::ProviderId,
    protocol: crate::ProtocolFamily,
    profile_name: String,
    logical_model_utf16: Vec<u16>,
}

impl ApiService {
    /// Sample Native's process-local MCTC and inline-tools gates for one
    /// logical request. Callers retain the result in the trusted request
    /// context so preparation, beta generation, and SDK encoding share it.
    pub fn native_tool_change_feature_gates(&self) -> (bool, bool) {
        let b6_disabled = crate::model::thinking_binding_controls::native_b6_disabled();
        let foamy_spring_enabled =
            ::telemetry::feature_flags::flag_bool("tengu_foamy_spring", true);
        let session_profile_active =
            std::env::var_os("CCR_SESSION_PROFILE").is_some_and(|value| !value.is_empty());
        let inline_tools_env = std::env::var_os(branding::INLINE_TOOLS_ENV)
            .map(|value| value.to_str().map(str::to_owned).unwrap_or_default());
        let brisk_meadow_enabled = if !b6_disabled
            && foamy_spring_enabled
            && !session_profile_active
            && inline_tools_env.is_none()
        {
            ::telemetry::feature_flags::flag_bool("tengu_brisk_meadow", false)
        } else {
            false
        };
        crate::model::betas::native_tool_change_feature_gates_for_inputs(
            b6_disabled,
            foamy_spring_enabled,
            session_profile_active,
            inline_tools_env.as_deref(),
            brisk_meadow_enabled,
        )
    }

    /// Native `hQ` decision for placing a model-facing system attachment
    /// before the closing line on this model/profile route. `Ta` lazily
    /// memoizes the `TF` result per host and raw model string. This service
    /// keeps the same raw-key behavior within each resolved Anthropic-family
    /// profile so one provider's route cannot poison another provider's result.
    pub fn supports_native_mid_conversation_system(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> bool {
        let Ok(protocol) = self.protocol_for_model(model, profile) else {
            return false;
        };
        // Foundry accepts only empty effort markers, never system attachments.
        if protocol == crate::ProtocolFamily::FoundryClaude
            || !Self::is_anthropic_family_protocol(&protocol)
        {
            return false;
        }
        let Ok(route) = self.client().resolve_media_route(model, profile) else {
            return false;
        };
        let cache_key = (
            route.main.provider_id.clone(),
            route.main.profile_name.clone(),
            model.to_owned(),
        );
        let mut cache = self
            .anthropic
            .native_mid_conversation_system_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(supported) = cache.get(&cache_key) {
            return *supported;
        }
        let supported = crate::model::betas::native_mid_conversation_system_supported(
            model,
            &route.main.request_model,
            Self::native_model_option_overrides_allowed(&route.main.provider_id),
        );
        cache.insert(cache_key, supported);
        supported
    }

    /// Native `mSe(model)` decision after the provider-family and `hQ(model)`
    /// gates. A resolved but ineligible route returns `Some(false)`; `None`
    /// means this service cannot resolve the requested model/profile.
    pub fn native_mid_conversation_tool_change_supported(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Option<bool> {
        let protocol = self.protocol_for_model(model, profile).ok()?;
        let route = self.client().resolve_media_route(model, profile).ok()?.main;
        let route_admitted = protocol == crate::ProtocolFamily::AnthropicMessages
            && route.provider_id == crate::ProviderId::AnthropicFirstParty;
        if !route_admitted || !self.supports_native_mid_conversation_system(model, profile) {
            return Some(false);
        }

        let provider_route =
            serde_json::to_string(&(protocol, &route.provider_id, &route.profile_name))
                .expect("resolved protocol route identity is serializable");
        let unsupported_identity =
            lingxi_llm_client::providers::anthropic::beta_repair::ModelUnsupportedIdentity::new(
                provider_route,
                self.fast_rejection_identity(&route.request_model),
            );
        let model_unsupported =
            lingxi_llm_client::providers::anthropic::beta_repair::ModelUnsupportedBetaRejections::for_process()
                .rejected(
                    &unsupported_identity,
                    lingxi_llm_client::providers::anthropic::beta_repair::Beta::MidConversationToolChanges,
                );
        Some(
            crate::model::betas::native_mid_conversation_tool_change_supported(
                &self.fast_rejection_identity(&route.request_model),
                model_unsupported,
            ),
        )
    }

    pub(in crate::service) fn native_model_option_overrides_allowed(
        provider_id: &crate::ProviderId,
    ) -> bool {
        match provider_id {
            crate::ProviderId::AnthropicFirstParty => false,
            crate::ProviderId::OpenAICompatible { name } | crate::ProviderId::Custom { name } => {
                !matches!(
                    name.to_ascii_lowercase().as_str(),
                    "gateway" | "anthropicaws" | "anthropicgooglecloud"
                )
            }
            crate::ProviderId::OpenAI
            | crate::ProviderId::Gemini
            | crate::ProviderId::VertexGemini
            | crate::ProviderId::VertexClaude
            | crate::ProviderId::BedrockClaude
            | crate::ProviderId::FoundryClaude
            | crate::ProviderId::AzureOpenAI => true,
        }
    }

    /// Return a cached Native per-model environment capability for a request.
    /// The cache key includes the selected route and feature, matching QTe's
    /// model-plus-capability cache without allowing multi-provider routes in
    /// Harness to share observations.
    pub fn native_model_option_capability_for_request(
        &self,
        request: &LlmRequest,
        capability: &str,
    ) -> Result<Option<bool>, LlmError> {
        let route = self
            .client()
            .resolve_media_route(&request.input.model, request.profile.as_deref())?
            .main;
        let protocol = self
            .client()
            .protocol_for_model(&request.input.model, Some(&route.profile_name))?;
        if !is_native_main_anthropic_route(&route.provider_id, protocol) {
            return Ok(None);
        }
        Ok(self.native_model_option_capability_for_route(
            request,
            &route.provider_id,
            protocol,
            &route.profile_name,
            capability,
        ))
    }

    /// Read Native's sticky requestLatches.thinkingResumptionRefused value.
    /// The latch starts false and only transitions to true from the SDK's
    /// exact `thinking_resumption_beta_refused` fact.
    pub fn native_thinking_resumption_refused(&self) -> bool {
        self.anthropic
            .native_thinking_resumption_refused
            .is_refused()
    }

    pub(in crate::service) fn observe_native_thinking_resumption_refusal(&self, error: &LlmError) {
        self.anthropic
            .native_thinking_resumption_refused
            .observe(error);
    }

    fn native_model_option_capability_for_route(
        &self,
        request: &LlmRequest,
        provider_id: &crate::ProviderId,
        protocol: crate::ProtocolFamily,
        profile_name: &str,
        capability: &str,
    ) -> Option<bool> {
        let raw_model = native_request_model_name_for_option_lookup(request)?;
        let mut key = Self::native_thinking_type_key(request, provider_id, protocol, profile_name);
        // Native QTe lowercases the model component before joining it with the
        // feature name. Keep that model behavior while partitioning by the
        // route identity that Harness must preserve across providers.
        key.logical_model_utf16 = raw_model.to_lowercase().encode_utf16().collect();
        let cache_key = (key, capability.to_ascii_lowercase());
        let mut cache = self
            .anthropic
            .native_model_option_capabilities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *cache.entry(cache_key).or_insert_with(|| {
            crate::model::betas::native_model_option_capability_from_process_env(
                &raw_model,
                Self::native_model_option_overrides_allowed(provider_id),
                capability,
            )
        })
    }

    /// Supply a versioned SDK client identity for Anthropic-family requests.
    #[must_use]
    pub fn with_native_thinking_display(
        mut self,
        policy: lingxi_llm_client::providers::anthropic::thinking_display::ThinkingDisplayPolicy,
    ) -> Self {
        self.anthropic.native_thinking_display = Some(policy);
        self
    }

    pub(in crate::service) fn is_main_thinking_display_call(
        prepared: &crate::PreparedLlmCall,
    ) -> bool {
        prepared.anthropic_request_kind
            == lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main
            && prepared.route.protocol == crate::ProtocolFamily::AnthropicMessages
            && prepared.route.resolved_route.provider_id == crate::ProviderId::AnthropicFirstParty
    }

    fn is_main_thinking_display_updates_call(prepared: &crate::PreparedLlmCall) -> bool {
        Self::is_main_thinking_display_call(prepared)
            || (prepared.anthropic_request_kind
                == lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main
                && prepared.route.protocol == crate::ProtocolFamily::FoundryClaude)
    }

    /// Reconcile Native's source-backed TBC producer after Host request policy
    /// has finished. The SDK's late RequestDraft setter emits the header only
    /// for Some(true); a custom header by itself never upgrades this fact.
    pub(in crate::service) fn update_native_thinking_binding_controls_beta_fact(
        &self,
        prepared: &mut crate::PreparedLlmCall,
    ) {
        use crate::model::thinking_binding_controls::{
            evaluate, Facts, ProviderRoute, RegistrationEffect,
        };

        if !Self::is_main_anthropic_beta_request(prepared) {
            return;
        }
        // An explicit false is this query's final no-membership fact (including
        // QEE suppression). Do not re-evaluate it against a reset state during
        // a retry and accidentally re-add TBC to this query.
        if prepared.native_thinking_binding_controls_beta_requested == Some(false) {
            Self::remove_beta_header_token(
                prepared,
                lingxi_llm_client::providers::anthropic::beta_repair::Beta::ThinkingBindingControls
                    .header(),
            );
            return;
        }
        let provider = match (
            &prepared.route.resolved_route.provider_id,
            prepared.route.protocol,
        ) {
            (crate::ProviderId::AnthropicFirstParty, crate::ProtocolFamily::AnthropicMessages) => {
                Some(ProviderRoute::FirstParty)
            }
            (crate::ProviderId::FoundryClaude, crate::ProtocolFamily::FoundryClaude) => {
                Some(ProviderRoute::Foundry)
            }
            (crate::ProviderId::BedrockClaude, crate::ProtocolFamily::BedrockClaude) => {
                Some(ProviderRoute::Bedrock)
            }
            (crate::ProviderId::VertexClaude, crate::ProtocolFamily::VertexClaude) => {
                Some(ProviderRoute::Vertex)
            }
            _ => None,
        };
        let tbc_state = prepared
            .native_thinking_binding_controls_attempt
            .as_ref()
            .map(|attempt| attempt.state());
        let bedrock_canonical = (provider == Some(ProviderRoute::Bedrock)).then(|| {
            crate::model::betas::beta_canonical(&prepared.route.resolved_route.request_model)
        });
        let (first_party_kxt_eligible, first_party_endpoint_eligible) = if provider
            == Some(ProviderRoute::FirstParty)
        {
            let route_base_url = self
                .client()
                .prompt_cache_profile(
                    &prepared.route.resolved_route.request_model,
                    Some(&prepared.route.resolved_route.profile_name),
                )
                .map(|profile| profile.base_url);
            let process_base_url = std::env::var_os("ANTHROPIC_BASE_URL");
            let configured_base_url = native_tbc_effective_base_url(
                route_base_url.as_deref(),
                process_base_url.as_deref(),
            );
            let configured_base_url = configured_base_url.as_deref().map(std::ffi::OsStr::new);
            let tf = native_tbc_first_party_tf(configured_base_url);
            let process_assume = std::env::var_os("_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL");
            let captured_managed_assume = self
                .native_system_prefix_snapshot()
                .map(|prefix| prefix.assume_first_party_base_url);
            let assume =
                native_tbc_first_party_assume(process_assume.as_deref(), captured_managed_assume);
            (
                native_tbc_first_party_kxt(
                    native_tbc_first_party_base_url_is_defined(configured_base_url),
                    tf,
                ),
                native_tbc_first_party_sl(tf, assume),
            )
        } else {
            (None, None)
        };
        let facts = Facts {
            provider,
            // Native excludes only disabled thinking with mechanical=true.
            // Enabled/adaptive proves this conjunction false even when the
            // independent mechanical source is unavailable to utility callers.
            mechanically_disabled: if prepared.native_thinking_request_type_disabled == Some(false)
            {
                Some(false)
            } else {
                prepared.native_thinking_mechanically_disabled
            },
            request_type_disabled: prepared.native_thinking_request_type_disabled,
            simulated_proxy_usage: Some(crate::structured_output::bool_environment(
                "CLAUDE_CODE_SIMULATE_PROXY_USAGE",
            )),
            experimental_betas_disabled: Some(
                crate::model::thinking_binding_controls::native_experimental_betas_disabled(),
            ),
            hipaa_tainted: Some(lingxi_core::host::compliance_taints::is_tainted("hipaa")),
            sent_before: tbc_state.as_ref().map(|state| state.beta_sent),
            rejected_before: Some(tbc_state.as_ref().is_some_and(|state| state.beta_rejected)),
            // Without an explicit parsed ANTHROPIC_BASE_URL, Native KXt may
            // depend on bd()/h3() and selected-profile state. Only the
            // request's captured non-empty API key proves h3() false; other
            // credential kinds remain unknown.
            first_party_h3: prepared.native_tbc_first_party_h3,
            first_party_kxt_eligible,
            first_party_endpoint_eligible,
            bedrock_base_url_override_defined: Some(
                std::env::var_os("ANTHROPIC_BEDROCK_BASE_URL").is_some(),
            ),
            vertex_base_url_override_defined: Some(
                std::env::var_os("ANTHROPIC_VERTEX_BASE_URL").is_some(),
            ),
            bedrock_native_canonical_model: bedrock_canonical.as_deref(),
        };
        let rejected_before = facts.rejected_before == Some(true);
        let evaluation = evaluate(&facts);
        if evaluation.registration == RegistrationEffect::RegisteredBeforeDispatch {
            if let Some(attempt) = prepared.native_thinking_binding_controls_attempt.as_ref() {
                attempt.mark_beta_sent();
            }
        } else if evaluation.registration == RegistrationEffect::RefusedByConversationState {
            if let Some(attempt) = prepared.native_thinking_binding_controls_attempt.as_ref() {
                attempt.refuse_beta();
            }
        }
        prepared.native_thinking_binding_controls_beta_requested = evaluation.kc.or_else(|| {
            prepared
                .native_thinking_binding_controls_beta_requested
                .filter(|requested| !requested)
        });
        if rejected_before
            || evaluation.registration == RegistrationEffect::RefusedByConversationState
        {
            prepared.native_thinking_binding_controls_beta_requested = Some(false);
            Self::remove_beta_header_token(
                prepared,
                lingxi_llm_client::providers::anthropic::beta_repair::Beta::ThinkingBindingControls
                    .header(),
            );
        }
    }

    /// The `kc && oT.includes(uB.header)` portion of Native `Tp`; the caller
    /// also applies `!U6` and checks the finalized sent header. Bedrock body
    /// betas are not part of `oT` and cannot satisfy this condition.
    pub(in crate::service) fn native_thinking_binding_controls_planned(
        prepared: &crate::PreparedLlmCall,
    ) -> bool {
        const THINKING_BINDING_CONTROLS: &str = "thinking-binding-controls-2026-08-01";
        prepared.native_thinking_binding_controls_beta_requested == Some(true)
            && prepared
                .planned_native_beta_context
                .as_ref()
                .is_some_and(|planned| planned.header_contains(THINKING_BINDING_CONTROLS))
    }

    pub(in crate::service) fn native_thinking_type_key(
        request: &LlmRequest,
        provider_id: &crate::ProviderId,
        protocol: crate::ProtocolFamily,
        profile_name: &str,
    ) -> NativeThinkingTypeKey {
        NativeThinkingTypeKey {
            provider_id: provider_id.clone(),
            protocol,
            profile_name: profile_name.to_owned(),
            logical_model_utf16: native_request_model_identity(request),
        }
    }

    /// Re-resolve the ordinary Native main thinking body from the live base,
    /// nullable overlay, route-scoped type latch, and selected output ceiling.
    /// This runs before each preparation so overflow/output-budget retries keep
    /// the SDK provenance paired with the actual encoded `max_tokens` value.
    pub(in crate::service) fn apply_native_main_thinking_policy(
        &self,
        request: &mut LlmRequest,
    ) -> Result<(), LlmError> {
        if request_query_source_category(request) != Some(QuerySourceCategory::Main)
            || request.execution.anthropic_request_kind
                != lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main
        {
            return Ok(());
        }

        let route = self
            .client()
            .resolve_media_route(&request.input.model, request.profile.as_deref())?
            .main;
        let profile_name = route.profile_name.clone();
        let protocol = self
            .client()
            .protocol_for_model(&request.input.model, Some(&profile_name))?;
        if !is_native_main_anthropic_route(&route.provider_id, protocol) {
            return Ok(());
        }
        let Some(provider_default) = native_adaptive_provider_default(&route.provider_id, protocol)
        else {
            return Ok(());
        };

        let state = request
            .execution
            .native_thinking_state_snapshot
            .unwrap_or_else(|| self.thinking_state());
        // Native Lsn normalization and its provenance are resolved separately.
        request.execution.automatic_thinking_budget = false;
        let request_type_disabled = state.native_request_type_disabled();
        request.execution.native_thinking_request_type_disabled = request_type_disabled;
        // This is a standard Native Main request after the exact route fence;
        // the host has no mechanical-disable producer for this path. Preserve
        // the source distinction between a user-selected Disabled type and a
        // mechanical disable so Native `Kn` can still evaluate its beta gate.
        request.execution.native_thinking_mechanically_disabled = Some(false);
        let key =
            Self::native_thinking_type_key(request, &route.provider_id, protocol, &profile_name);
        let type_override = self
            .anthropic
            .native_thinking_type_overrides
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .copied();
        let configured_disabled = state.explicitly_disabled()
            || !crate::model::thinking::session_thinking_active(state.base())
            || !crate::model::thinking::model_supports_thinking(&route.request_model)
            || request_type_disabled == Some(true);
        if configured_disabled {
            request.set_reasoning(None);
            request.execution.native_lsn_manual_budget = None;
            request.input.temperature =
                if crate::model::thinking::model_sends_temperature(&route.request_model) {
                    Some(1.0)
                } else {
                    None
                };
            if let Some(context) = request.execution.anthropic_context_management.as_mut() {
                context.has_thinking = false;
            }
            return Ok(());
        }

        let model_option_capability = self.native_model_option_capability_for_route(
            request,
            &route.provider_id,
            protocol,
            &profile_name,
            "adaptive_thinking",
        );
        let model_capabilities = std::env::var(branding::MODEL_CAPABILITIES_ENV).ok();
        let adaptive_default = crate::model::thinking::native_adaptive_default_from_inputs(
            &route.request_model,
            model_option_capability,
            model_capabilities.as_deref(),
            provider_default,
        );
        let use_adaptive = match type_override {
            Some(lingxi_llm_client::protocol::NativeThinkingTypeRejection::Adaptive) => true,
            Some(lingxi_llm_client::protocol::NativeThinkingTypeRejection::Enabled) => false,
            None => adaptive_default,
        };
        request.execution.native_thinking_request_type_disabled = Some(false);
        request.input.temperature = None;
        if let Some(context) = request.execution.anthropic_context_management.as_mut() {
            context.has_thinking = true;
        }
        if use_adaptive {
            request.set_reasoning(Some(crate::ReasoningConfig::Adaptive));
            request.execution.native_lsn_manual_budget = None;
            return Ok(());
        }

        let resolved_output = self
            .client()
            .resolve_output_budget_in(
                &request.input.model,
                Some(&profile_name),
                request.input.max_tokens,
            )?
            .value
            .ok_or_else(|| LlmError::InvalidRequest {
                message: "Native Claude request has no resolved output-token budget".into(),
            })?;
        let selected_model_upper_limit = self
            .client()
            .output_token_limits_in(&request.input.model, Some(&profile_name))?
            .max_output_tokens;
        let budget = state.native_enabled_budget(resolved_output, selected_model_upper_limit);
        request.set_reasoning(Some(crate::ReasoningConfig::Enabled {
            budget_tokens: budget,
        }));
        request.execution.native_lsn_manual_budget = Some(
            lingxi_llm_client::client::NativeLsnManualBudget::new(budget, Some(resolved_output)),
        );
        Ok(())
    }

    /// Store the exact Native `tae` target before checking its dedicated
    /// one-shot repair gate. Repeated matching errors still
    /// update the service-lifetime per-model latch, but only the first one can
    /// claim its dedicated retry.
    pub(in crate::service) fn record_native_thinking_type_rejection(
        &self,
        request: &LlmRequest,
        prepared: &crate::PreparedLlmCall,
        error: &LlmError,
        status: u16,
        retry_scope: &ModelCallRetryScope,
    ) -> bool {
        if status != 400
            || request_query_source_category(request) != Some(QuerySourceCategory::Main)
            || request.execution.anthropic_request_kind
                != lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main
            || !is_native_main_anthropic_route(
                &prepared.route.resolved_route.provider_id,
                prepared.route.protocol,
            )
        {
            return false;
        }
        let Some(rejected) = error
            .native_anthropic_error_facts()
            .and_then(|facts| facts.native_thinking_type_rejection)
        else {
            return false;
        };
        // Native `tae` retries with the opposite mode from the one the SDK
        // reports as rejected: adaptive → enabled, enabled → adaptive.
        let target = match rejected {
            lingxi_llm_client::protocol::NativeThinkingTypeRejection::Adaptive => {
                lingxi_llm_client::protocol::NativeThinkingTypeRejection::Enabled
            }
            lingxi_llm_client::protocol::NativeThinkingTypeRejection::Enabled => {
                lingxi_llm_client::protocol::NativeThinkingTypeRejection::Adaptive
            }
        };
        let key = Self::native_thinking_type_key(
            request,
            &prepared.route.resolved_route.provider_id,
            prepared.route.protocol,
            &prepared.route.resolved_route.profile_name,
        );
        self.anthropic
            .native_thinking_type_overrides
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, target);

        let state = request
            .execution
            .native_thinking_state_snapshot
            .unwrap_or_else(|| self.thinking_state());
        if state.explicitly_disabled()
            || !crate::model::thinking::session_thinking_active(state.base())
            || !crate::model::thinking::model_supports_thinking(
                &prepared.route.resolved_route.request_model,
            )
            || request.execution.native_thinking_request_type_disabled == Some(true)
        {
            return false;
        }
        retry_scope.take_native_thinking_type_retry()
    }

    /// Native `U6`: among the provider identities currently represented by
    /// Harness, first-party and Foundry take the first-party-family branch;
    /// Bedrock and Vertex take the third-party branch. Anthropic AWS/GCP have
    /// no corresponding Harness route identity and are intentionally unknown.
    pub(in crate::service) fn native_thinking_binding_controls_u6(
        prepared: &crate::PreparedLlmCall,
    ) -> Option<bool> {
        match (
            &prepared.route.resolved_route.provider_id,
            prepared.route.protocol,
        ) {
            (crate::ProviderId::AnthropicFirstParty, crate::ProtocolFamily::AnthropicMessages)
            | (crate::ProviderId::FoundryClaude, crate::ProtocolFamily::FoundryClaude) => {
                Some(true)
            }
            (crate::ProviderId::BedrockClaude, crate::ProtocolFamily::BedrockClaude)
            | (crate::ProviderId::VertexClaude, crate::ProtocolFamily::VertexClaude) => Some(false),
            _ => None,
        }
    }

    /// Native `QEe`'s ordinary request path. The SDK reports only source
    /// classifiers (`nNe`, `aMt`, and `xL`) for a prepared request that carried
    /// the TBC beta. `nNe` applies on every route; `aMt` and `xL` are accepted
    /// only outside Native's `U6` provider group. HQ's separate broad unknown
    /// HTTP-400 stage is deliberately not inferred here.
    pub(in crate::service) fn native_thinking_binding_controls_qee_refusal(
        prepared: &crate::PreparedLlmCall,
        error: &LlmError,
        status: u16,
    ) -> bool {
        if status != 400
            || !Self::native_thinking_binding_controls_planned(prepared)
            || !Self::request_header_beta_tokens(prepared).iter().any(|beta| {
                beta == lingxi_llm_client::providers::anthropic::beta_repair::Beta::ThinkingBindingControls
                    .header()
            })
        {
            return false;
        }
        let Some(facts) = error.native_anthropic_error_facts() else {
            return false;
        };
        if facts.native_thinking_binding_controls_beta_requested != Some(true) {
            return false;
        }
        let Some(evidence) = facts
            .native_thinking_binding_controls_error_evidence
            .as_ref()
        else {
            return false;
        };
        evidence.explicit_refusal
            || Self::native_thinking_binding_controls_u6(prepared) == Some(false)
                && (evidence.token_refusal || evidence.invalid_beta_flag)
    }

    /// Native's `nB`: a speed-parameter rejection only contributes to `jk`
    /// when its quoted model normalizes to the original logical `X.model` and
    /// that model is a refusal-fallback target or is already in the Host's
    /// fast-rejection state. This is read-only: the fast-repair handler owns
    /// mutations and runs separately.
    fn native_hq_n_b_claim(
        &self,
        prepared: &crate::PreparedLlmCall,
        error: &LlmError,
        status: u16,
        body: &lingxi_llm_client::protocol::JsonPayload,
    ) -> Option<bool> {
        let facts = error.native_anthropic_error_facts()?;
        let model_source = facts.native_hq_model_source.as_ref()?;
        let model_units = model_source.root_string_code_units()?;
        let model = String::from_utf16(&model_units).ok()?;
        let normalized_status = if status == 422 { 400 } else { status };
        let matches_speed_rejection =
            lingxi_llm_client::providers::anthropic::error_recognition::speed_rejected_for(
                normalized_status,
                body,
                &model,
                |name| self.fast_rejection_identity(name),
            );
        let target = prepared
            .refusal_fallback_context
            .as_ref()
            .is_some_and(|context| {
                context.is_target(&model, |name| self.fast_rejection_identity(name))
            });
        let known_rejected = lingxi_core::host::fast_mode::ModelRejections::for_process()
            .known_rejected(&self.fast_rejection_identity(&model));
        native_hq_n_b_claim_from_facts(
            Some(matches_speed_rejection),
            Some(target),
            Some(known_rejected),
        )
    }

    /// Native's `jk = kYe || nB`. The SDK supplies the 15 exact response
    /// predicates; the prefix-lock healing marker is absent from Harness, so
    /// an AL-only match remains unknown instead of being treated as false.
    fn native_hq_jk_claim(
        &self,
        prepared: &crate::PreparedLlmCall,
        error: &LlmError,
        status: u16,
        body: &lingxi_llm_client::protocol::JsonPayload,
    ) -> Option<bool> {
        let facts = error.native_anthropic_error_facts()?;
        let k_ye = facts
            .native_hq_hrn15_evidence
            .as_ref()
            .and_then(|hrn15| hrn15.aggregate_claim(None));
        let n_b = self.native_hq_n_b_claim(prepared, error, status, body);
        native_hq_j_k_claim_from_facts(k_ye, n_b)
    }

    /// Native's later `HQ` broad QEE fallback. It is reached only for an
    /// original HTTP 400, after the ordinary context-1m RZe and explicit TBC
    /// QEE checks. Require a planned and actually sent TBC header, known
    /// `gB=false`, and known `jk=false`; unknown claims never become negatives.
    pub(in crate::service) fn native_thinking_binding_controls_hq_unknown_refusal(
        &self,
        prepared: &crate::PreparedLlmCall,
        error: &LlmError,
        status: u16,
        body: &lingxi_llm_client::protocol::JsonPayload,
    ) -> bool {
        let Some(facts) = error.native_anthropic_error_facts() else {
            return false;
        };
        let g_b = facts
            .native_hq_known_error_evidence
            .as_ref()
            .filter(|evidence| evidence.normalized_status == 400)
            .and_then(|evidence| evidence.aggregate_claim());
        let j_k = self.native_hq_jk_claim(prepared, error, status, body);
        let gate = NativeHqQeeGate {
            original_status: status,
            u6: Self::native_thinking_binding_controls_u6(prepared),
            planned_tbc_header: Self::native_thinking_binding_controls_planned(prepared),
            actual_tbc_header: Self::request_header_beta_tokens(prepared)
                .iter()
                .any(|beta| {
                    beta == lingxi_llm_client::providers::anthropic::beta_repair::Beta::ThinkingBindingControls
                        .header()
                }),
            requested_tbc: facts.native_thinking_binding_controls_beta_requested,
            g_b,
            j_k,
        };
        if !gate.should_retry() {
            return false;
        }
        true
    }

    /// Native `Wm(nt, uB)`: rejection is conversation-local, drops only this
    /// exact header token, and explicitly clears the SDK's late producer fact
    /// so request sealing cannot restore it on the retry.
    pub(in crate::service) fn refuse_native_thinking_binding_controls(
        prepared: &mut crate::PreparedLlmCall,
    ) {
        if let Some(attempt) = prepared.native_thinking_binding_controls_attempt.as_ref() {
            attempt.refuse_beta();
        }
        prepared.native_thinking_binding_controls_beta_requested = Some(false);

        Self::remove_beta_header_token(
            prepared,
            lingxi_llm_client::providers::anthropic::beta_repair::Beta::ThinkingBindingControls
                .header(),
        );
    }

    fn request_has_native_tool_change(prepared: &crate::PreparedLlmCall) -> bool {
        let body = &prepared.provider_request.body_json;
        let Some(messages) = body.array_len("/messages") else {
            return false;
        };
        (0..messages).any(|message_index| {
            let message_pointer = format!("/messages/{message_index}");
            if body
                .string_at(&format!("{message_pointer}/role"))
                .as_deref()
                != Some("system")
            {
                return false;
            }
            let content_pointer = format!("{message_pointer}/content");
            body.array_len(&content_pointer).is_some_and(|blocks| {
                (0..blocks).any(|block_index| {
                    matches!(
                        body.string_at(&format!("{content_pointer}/{block_index}/type"))
                            .as_deref(),
                        Some("tool_addition" | "tool_removal")
                    )
                })
            })
        })
    }

    fn native_tool_change_block_type_unknown(
        status: u16,
        body: &lingxi_llm_client::protocol::JsonPayload,
    ) -> bool {
        if !matches!(status, 400 | 422) {
            return false;
        }
        let message = body.string_at("/error/message").unwrap_or_default();
        ["Input tag 'tool_addition'", "Input tag 'tool_removal'"]
            .iter()
            .any(|tag| message.contains(tag))
    }

    pub(in crate::service) fn settle_thinking_display_probe(
        prepared: &crate::PreparedLlmCall,
        scope: &ModelCallRetryScope,
    ) {
        if Self::is_main_thinking_display_call(prepared) {
            scope.display_probe_succeeded(&prepared.beta_rejection_state);
        }
    }

    /// Native's streaming `Kie` branch runs before the context/TBC error
    /// handlers. Restrict it to the admitted AFK producer and actual request
    /// token so an arbitrary extra-body beta cannot acquire Kie's retry order.
    pub(in crate::service) fn probe_native_stream_afk_error(
        &self,
        prepared: &crate::PreparedLlmCall,
        status: u16,
        body: &lingxi_llm_client::protocol::JsonPayload,
    ) -> bool {
        use lingxi_llm_client::providers::anthropic::beta_repair;

        if status != 400
            || !Self::is_main_anthropic_beta_request(prepared)
            || !self.beta_context(prepared).afk_mode
        {
            return false;
        }

        let sent = Self::request_beta_tokens(prepared);
        let afk_header = beta_repair::Beta::AfkMode.header();
        if !sent.iter().any(|beta| beta == afk_header) {
            return false;
        }
        let Some(afk_rejection) = beta_repair::request_rejections(status, body, &sent)
            .into_iter()
            .find(|rejection| rejection.beta == beta_repair::Beta::AfkMode)
        else {
            return false;
        };

        let model_key = self.fast_rejection_identity(&prepared.route.resolved_route.request_model);
        beta_repair::reject_named(
            std::slice::from_ref(&afk_rejection),
            &prepared.beta_rejection_state,
            Some(&model_key),
            &beta_repair::ModelBetaRejections::for_process(),
        );
        true
    }

    /// Try the SDK's unclaimed display repair before ordinary API retries.
    pub(in crate::service) fn probe_thinking_display_error(
        &self,
        prepared: &crate::PreparedLlmCall,
        scope: &ModelCallRetryScope,
        status: u16,
        body: &lingxi_llm_client::protocol::JsonPayload,
    ) -> bool {
        use lingxi_llm_client::providers::anthropic::thinking_display;
        if !Self::is_main_anthropic_beta_request(prepared) {
            return false;
        }
        let sent = Self::request_beta_tokens(prepared);
        let candidates = Self::native_beta_rejection_candidates(prepared);
        use lingxi_llm_client::providers::anthropic::beta_repair;
        let model_key = self.fast_rejection_identity(&prepared.route.resolved_route.request_model);
        let named = beta_repair::request_rejections(status, body, &candidates)
            .into_iter()
            // Native's `iNe` registry omits TBC; its refusals are handled by
            // the QEE-specific typed evidence path below, never generic named
            // or model-wide beta rejection. AFK is handled by the earlier
            // streaming-only Kie preflight and is never a generic beta repair.
            .filter(|rejection| {
                !matches!(
                    rejection.beta,
                    beta_repair::Beta::ThinkingBindingControls | beta_repair::Beta::AfkMode
                )
            })
            .collect::<Vec<_>>();
        let model_unsupported = beta_repair::model_unsupported_rejection(status, body, &candidates);
        let mctc_header_refused = named
            .iter()
            .any(|rejection| rejection.beta == beta_repair::Beta::MidConversationToolChanges);
        let mctc_header_was_sent = sent
            .iter()
            .any(|header| header == beta_repair::Beta::MidConversationToolChanges.header());
        let mctc_block_refused = mctc_header_was_sent
            && model_unsupported.is_none()
            && Self::request_has_native_tool_change(prepared)
            && Self::native_tool_change_block_type_unknown(status, body);
        if mctc_header_refused || mctc_block_refused {
            if let Some(scope) = prepared.thinking_recovery_scope.as_ref() {
                scope.refuse_mid_conversation_tool_changes();
            }
        }

        if let Some(rejection) = model_unsupported {
            beta_repair::reject_model_unsupported(
                rejection,
                &self.model_unsupported_identity(prepared),
                &beta_repair::ModelUnsupportedBetaRejections::for_process(),
            );
            return true;
        }
        if !named.is_empty() {
            lingxi_llm_client::providers::anthropic::beta_repair::reject_named(
                &named, &prepared.beta_rejection_state,
                Some(&model_key),
                &lingxi_llm_client::providers::anthropic::beta_repair::ModelBetaRejections::for_process(),
            );
            return true;
        }
        if !Self::is_main_thinking_display_call(prepared) {
            return false;
        }
        let identity = self.fast_rejection_identity(&prepared.route.resolved_route.request_model);
        let admission = thinking_display::probe_admission(
            status,
            body,
            candidates
                .iter()
                .any(|beta| beta == thinking_display::UPDATES_BETA),
            Self::direct_anthropic_api_route(prepared),
            lingxi_llm_client::providers::anthropic::error_recognition::RecognitionContext {
                request_model: &prepared.route.resolved_route.request_model,
                refusal_fallback_target: prepared.refusal_fallback_context.as_ref().is_some_and(
                    |context| {
                        context.is_target(&prepared.route.resolved_route.request_model, |model| {
                            self.fast_rejection_identity(model)
                        })
                    },
                ),
                previous_fast_rejection:
                    lingxi_core::host::fast_mode::ModelRejections::for_process()
                        .known_rejected(&identity),
                prefix_heal_declined: false,
            },
            |model| self.fast_rejection_identity(model),
        );
        scope.display_probe_error(
            status,
            admission,
            &thinking_display::DisplayProbeBudget::for_process(),
        )
    }

    pub(in crate::service) fn apply_automatic_thinking_display(
        &self,
        prepared: &mut crate::PreparedLlmCall,
    ) -> Result<bool, LlmError> {
        use lingxi_llm_client::providers::anthropic::thinking_display::{self, UpdatesAdmission};
        // Native's rVe gate admits Foundry for automatic updates even though
        // its separate display-error probe remains limited to first-party API.
        if !Self::is_main_thinking_display_updates_call(prepared) {
            return Ok(false);
        }
        let fast_identity =
            self.fast_rejection_identity(&prepared.route.resolved_route.request_model);
        let model_unsupported_identity = self.model_unsupported_identity(prepared);
        lingxi_llm_client::providers::anthropic::beta_repair::apply_rejections(
            &mut prepared.provider_request.headers,
            &mut prepared.provider_request.body_json,
            &prepared.beta_rejection_state,
            Some(&fast_identity),
            Some(&model_unsupported_identity),
            &lingxi_llm_client::providers::anthropic::beta_repair::ModelBetaRejections::for_process(
            ),
        )
        .map_err(crate::upstream::error)?;
        if crate::model::thinking_binding_controls::native_b6_disabled() {
            return Ok(false);
        }
        let model = &prepared.route.resolved_route.request_model;
        let canonical = crate::model::thinking::canonical(model);
        let interactive = self.interactive_session.unwrap_or_else(|| {
            !lingxi_core::host::session_flags::effective_non_interactive_session()
        });
        let native_thinking_display_route = Self::is_main_thinking_display_updates_call(prepared);
        // The headless host distinguishes terse text/JSON from verbose/stream
        // output. Only hosts without that policy use the interactive fallback.
        let omit_default = self
            .anthropic
            .native_thinking_display
            .as_ref()
            .map_or(!interactive, |policy| policy.omit_default);
        let explicit_display = self
            .anthropic
            .native_thinking_display
            .as_ref()
            .and_then(|policy| policy.explicit.as_deref())
            .filter(|value| !value.is_empty());
        let updates_setting = std::env::var(branding::THINKING_DISPLAY_UPDATES_ENV).ok();
        let updates = updates_setting
            .as_deref()
            .map(crate::structured_output::bool_value)
            .unwrap_or(!(native_thinking_display_route && omit_default));
        let show_summaries = lingxi_core::host::session_flags::show_thinking_summaries();
        let extra_has_thinking = prepared.extra_body.as_ref().is_some_and(|extra| {
            lingxi_llm_client::providers::anthropic::request_policy::has_field(extra, "thinking")
        });
        let simulated_proxy =
            crate::structured_output::bool_environment("CLAUDE_CODE_SIMULATE_PROXY_USAGE");
        let native_headless_default_omits_display = updates_setting.is_none()
            && native_thinking_display_route
            && omit_default
            && explicit_display.is_none()
            && !show_summaries
            && !extra_has_thinking
            && !simulated_proxy;
        if native_headless_default_omits_display {
            thinking_display::apply_omitted_display(
                &mut prepared.provider_request.body_json,
                &mut prepared.provider_request.json_string_overrides,
                extra_has_thinking,
            )
            .map_err(crate::upstream::error)?;
            return Ok(false);
        }
        let mode = thinking_display::connector_mode(
            explicit_display,
            explicit_display.is_some(),
            show_summaries,
            updates,
        );
        thinking_display::apply_request_updates(
            &mut prepared.provider_request.body_json,
            &mut prepared.provider_request.headers,
            &mut prepared.provider_request.json_string_overrides,
            UpdatesAdmission {
                mode,
                supports_interleaved: !canonical.starts_with("claude-3-")
                    && canonical != "claude-haiku-4-5",
                extra_has_thinking,
                simulated_proxy,
            },
            &prepared.beta_rejection_state,
            &prepared.thinking_display_probe,
        )
        .map_err(crate::upstream::error)
    }
}
