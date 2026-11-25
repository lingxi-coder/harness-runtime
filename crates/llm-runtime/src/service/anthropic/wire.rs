//! Application of Claude request policy before the SDK seals the request.

use crate::dispatch_header::{DispatchAttempt, DispatchHeaderState};
use crate::model::betas::{
    apply_beta_header_with_auth_and_custom, bedrock_extra_body_betas, BetaContext, Endpoint,
    Provider,
};
use crate::service::anthropic::endpoint::{
    native_custom_beta_provider_route, native_mctc_disabled_by_process_mode,
};
#[cfg(test)]
use crate::service::extra_body_object;
use crate::service::ApiService;
use crate::LlmError;

impl ApiService {
    /// Apply provider-aware beta, User-Agent, dispatch, and refusal headers to a
    /// prepared request. Native client request-id generation belongs to the SDK
    /// selected-route policy and is already sealed before this Host step.
    ///
    /// Reads [`Self::effective_subscriber`] directly (one resolver call per
    /// attempt — these injectors run once per prepare/execute attempt, so the
    /// live-slot read here is per-attempt, the lighter diff vs. threading the
    /// hoisted value through as a parameter).
    /// Build the per-request [`BetaContext`] (the binary's `xLr(model)` inputs)
    /// from the prepared request body: the resolved model id and `speed: "fast"`.
    /// Interactivity and `showThinkingSummaries` come from the process session
    /// flags published by the composition root, matching Claude's module-level
    /// `getIsNonInteractiveSession()` / initial-settings reads.
    pub(in crate::service) fn beta_context(
        &self,
        prepared: &crate::PreparedLlmCall,
    ) -> BetaContext {
        let model = prepared
            .provider_request
            .body_json
            .string_at("/model")
            // Vertex/Bedrock codecs move the model into the URL; keep beta
            // capability checks tied to the resolved request model.
            .unwrap_or_else(|| prepared.route.resolved_route.request_model.clone());
        let fast_mode = prepared
            .provider_request
            .body_json
            .string_at("/speed")
            .as_deref()
            == Some("fast");
        // YMe emits a beta for a resolved string or a supported main default.
        let effort_present =
            lingxi_llm_client::providers::anthropic::request_policy::has_field_path(
                &prepared.provider_request.body_json,
                &["output_config", "effort"],
            );
        let body_effort_is_string = prepared
            .provider_request
            .body_json
            .string_at("/output_config/effort")
            .is_some();
        let explicit_effort = prepared.anthropic_request_kind
            == lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main
            && prepared.extra_body.as_ref().is_some_and(|extra| {
                lingxi_llm_client::providers::anthropic::request_policy::has_field_path(
                    extra,
                    &["output_config", "effort"],
                )
            });
        let automatic_schema =
            lingxi_llm_client::providers::anthropic::request_policy::has_field_path(
                &prepared.provider_request.body_json,
                &["output_config", "format"],
            ) && crate::structured_output::native_hun_enabled(
                &prepared.route.resolved_route.provider_id,
                prepared.route.protocol,
                &model,
            )
            .unwrap_or(true);
        let has_tool_search = prepared
            .provider_request
            .body_json
            .array_len("/tools")
            .is_some_and(|length| {
                (0..length).any(|index| {
                    prepared
                        .provider_request
                        .body_json
                        .array_item_at("/tools", index)
                        .is_some_and(|tool| {
                            tool.string_at("/name").as_deref() == Some("ToolSearch")
                                || tool.bool_at("/defer_loading") == Some(true)
                        })
                })
            });
        let first_party_anthropic_route = prepared.route.protocol
            == crate::ProtocolFamily::AnthropicMessages
            && prepared.route.resolved_route.provider_id == crate::ProviderId::AnthropicFirstParty;
        let native_request_facts = (prepared.anthropic_request_kind
            == lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main
            && (first_party_anthropic_route
                || prepared.route.protocol == crate::ProtocolFamily::FoundryClaude))
            .then_some(prepared.native_anthropic_request_context.as_ref())
            .flatten();
        let native_afk_route = match prepared.route.protocol {
            crate::ProtocolFamily::AnthropicMessages => first_party_anthropic_route,
            crate::ProtocolFamily::FoundryClaude
            | crate::ProtocolFamily::VertexClaude
            | crate::ProtocolFamily::BedrockClaude => true,
            _ => false,
        };
        let native_afk_mode = native_afk_route
            && prepared.anthropic_request_kind
                == lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main
            && !prepared.beta_rejection_state.rejected(
                lingxi_llm_client::providers::anthropic::beta_repair::Beta::AfkMode,
            )
            && prepared
                .native_anthropic_request_context
                .as_ref()
                .is_some_and(|context| context.afk_mode);
        let native_per_turn_control = (first_party_anthropic_route
            || prepared.route.protocol == crate::ProtocolFamily::FoundryClaude)
            && prepared.anthropic_request_kind
                == lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main
            && Self::has_native_per_turn_effort_marker(&prepared.provider_request.body_json);
        let interactive = self.interactive_session.unwrap_or_else(|| {
            !lingxi_core::host::session_flags::effective_non_interactive_session()
        });
        let beta_provider = match prepared.route.protocol {
            crate::ProtocolFamily::FoundryClaude => Provider::Foundry,
            crate::ProtocolFamily::VertexClaude => Provider::Vertex,
            crate::ProtocolFamily::BedrockClaude => Provider::Bedrock,
            _ => Provider::Anthropic,
        };
        let supported = prepared.effort_policy.as_ref().map_or_else(
            || crate::model::betas::effort_capable(beta_provider, &model),
            |policy| policy.supported,
        );
        let selected = match prepared.effort_policy.as_ref() {
            Some(policy) => policy.value.as_ref(),
            None => None,
        };
        let selected_is_string = match prepared.effort_policy.as_ref() {
            Some(_) => selected.map_or(
                prepared.anthropic_request_kind
                    == lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main,
                lingxi_llm_client::protocol::JsonPayload::is_root_string,
            ),
            None if effort_present => body_effort_is_string,
            None => {
                prepared.anthropic_request_kind
                    == lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main
            }
        };
        let automatic_effort = supported && !explicit_effort && selected_is_string;
        let beta_model_identity = self.model_unsupported_identity(prepared);
        let native_mctc_admitted = !native_mctc_disabled_by_process_mode()
            && native_request_facts.is_some_and(|facts| {
                facts.mid_conversation_tool_changes
                    && facts.mid_conversation_tool_changes_feature_gate
            })
            && self
            .native_mid_conversation_tool_change_supported(
                &prepared.route.resolved_route.request_model,
                Some(&prepared.route.resolved_route.profile_name),
            ) == Some(true)
            && !prepared.beta_rejection_state.rejected(
                lingxi_llm_client::providers::anthropic::beta_repair::Beta::MidConversationToolChanges,
            )
            && !lingxi_llm_client::providers::anthropic::beta_repair::ModelUnsupportedBetaRejections::for_process()
                .rejected(
                    &beta_model_identity,
                    lingxi_llm_client::providers::anthropic::beta_repair::Beta::MidConversationToolChanges,
                );
        let native_inline_tools_admitted = native_mctc_admitted
            && native_request_facts.is_some_and(|facts| facts.inline_tools_feature_gate);
        BetaContext::for_model(&model)
            .with_interactive(interactive)
            .with_show_thinking_summaries(
                lingxi_core::host::session_flags::show_thinking_summaries(),
            )
            .with_fast_mode(fast_mode)
            .with_effort(automatic_effort)
            .with_per_turn_control(native_per_turn_control)
            .with_mid_conversation_tool_changes(native_mctc_admitted)
            .with_inline_tools(native_inline_tools_admitted)
            .with_afk_mode(native_afk_mode)
            .with_dangerous_tool_use(
                native_request_facts.is_some_and(|facts| facts.server_classifier.is_some()),
            )
            .with_structured_output(automatic_schema)
            .with_request_kind(prepared.anthropic_request_kind)
            .with_tool_search(has_tool_search)
            .with_context_hint(
                lingxi_llm_client::providers::anthropic::request_policy::has_field_path(
                    &prepared.provider_request.body_json,
                    &["context_hint"],
                ),
            )
    }

    fn append_native_afk_beta_after_display(&self, prepared: &mut crate::PreparedLlmCall) {
        if Self::extra_replaces_message_betas(prepared)
            || prepared.anthropic_request_kind
                != lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main
        {
            return;
        }
        let admitted = self.beta_context(prepared).afk_mode;
        Self::append_native_afk_beta_to_headers(
            prepared.route.protocol,
            prepared.anthropic_request_kind,
            admitted,
            &mut prepared.provider_request.headers,
        );
    }

    fn append_native_afk_beta_to_headers(
        protocol: crate::ProtocolFamily,
        request_kind: lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind,
        admitted: bool,
        headers: &mut std::collections::BTreeMap<String, String>,
    ) {
        if request_kind
            != lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main
        {
            return;
        }
        let provider = match protocol {
            crate::ProtocolFamily::FoundryClaude => Provider::Foundry,
            crate::ProtocolFamily::AnthropicMessages => Provider::Anthropic,
            crate::ProtocolFamily::VertexClaude => Provider::Vertex,
            _ => return,
        };
        crate::model::betas::append_afk_mode_after_thinking_display(headers, provider, admitted);
    }

    fn has_native_per_turn_effort_marker(body: &lingxi_llm_client::protocol::JsonPayload) -> bool {
        let Some(length) = body.array_len("/messages") else {
            return false;
        };
        (0..length).any(|index| {
            body.array_item_at("/messages", index)
                .is_some_and(|message| {
                    message.string_at("/role").as_deref() == Some("system")
                        && message.array_len("/content") == Some(0)
                        && lingxi_llm_client::providers::anthropic::request_policy::has_field_path(
                            &message,
                            &["output_config", "effort"],
                        )
                })
        })
    }

    #[cfg(test)]
    pub(in crate::service) fn interactive_session_for_test(&self) -> bool {
        self.interactive_session.unwrap_or_else(|| {
            !lingxi_core::host::session_flags::effective_non_interactive_session()
        })
    }

    /// Apply Native's `Rkr`/`vh` filtering to host-validated CLI beta additions
    /// on the provider families represented by an exact Native Host route.
    /// FirstParty and Foundry keep the full list only while experimental betas
    /// are enabled; Bedrock and Vertex always use Native's static `rS` allowlist.
    /// Generic Anthropic-compatible providers have no Native family evidence.
    pub(in crate::service) fn custom_cli_betas(
        &self,
        prepared: &crate::PreparedLlmCall,
    ) -> Vec<String> {
        let Some(provider) = native_custom_beta_provider_route(
            &prepared.route.resolved_route.provider_id,
            prepared.route.protocol,
        ) else {
            return Vec::new();
        };
        crate::model::thinking_binding_controls::native_cli_beta_additions(
            provider,
            crate::model::thinking_binding_controls::native_experimental_betas_disabled(),
            lingxi_core::host::compliance_taints::is_tainted("hipaa"),
            &self.anthropic.custom_cli_betas,
        )
    }

    /// Bedrock's extra-parameter beta IDs are projected through the SDK request
    /// policy into `anthropic_beta`, never appended to the HTTP beta header.
    fn custom_cli_betas_for_header(&self, prepared: &crate::PreparedLlmCall) -> Vec<String> {
        let mut betas = self.custom_cli_betas(prepared);
        if prepared.route.protocol == crate::ProtocolFamily::BedrockClaude {
            betas.retain(|beta| {
                !crate::model::betas::BEDROCK_EXTRA_PARAMS_HEADERS.contains(&beta.as_str())
            });
        }
        betas
    }

    /// Port of claude-code's `B0t` (2.1.207): parse `CLAUDE_CODE_EXTRA_BODY` into a
    /// JSON object to be spread into the outgoing Anthropic-family request body.
    ///
    /// * Non-object env value → the object is ignored and an error is logged with
    ///   the byte-exact claude-code string
    ///   `CLAUDE_CODE_EXTRA_BODY env var must be a JSON object, but was given {t}`.
    /// * A parse failure logs `Error parsing CLAUDE_CODE_EXTRA_BODY: {err}`.
    /// * `betas` (claude-code's `ol` arg — the bedrock/body beta list, empty on the
    ///   first-party path where betas ride the `anthropic-beta` header) is folded
    ///   into `anthropic_beta`: append-dedupe when the extra body already carries
    ///   that array, else set it.
    ///
    /// Kept under the original `CLAUDE_CODE_` env name (like the sibling
    /// `CLAUDE_CODE_EXTRA_METADATA` at [`ApiService::build_api_metadata_user_id`])
    /// — these are wire-parity vars preserved verbatim through the `LINGXI_` rename.
    #[cfg(test)]
    pub(in crate::service) fn parse_extra_body(betas: &[String]) -> serde_json::Value {
        let payload = lingxi_llm_client::providers::anthropic::request_policy::beta_body(
            extra_body_object()
                .expect("valid extra body")
                .unwrap_or_else(|| {
                    lingxi_llm_client::protocol::JsonPayload::parse("{}")
                        .expect("empty object is valid JSON")
                }),
            betas,
        )
        .expect("beta body is valid JSON");
        serde_json::from_str(payload.as_json_source()).expect("beta body is valid JSON")
    }

    pub(in crate::service) fn is_main_anthropic_beta_request(
        prepared: &crate::PreparedLlmCall,
    ) -> bool {
        prepared.anthropic_request_kind
            == lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main
            && Self::is_anthropic_family_protocol(&prepared.route.protocol)
    }

    pub(in crate::service) fn tool_change_dispatch_observation(
        prepared: &crate::PreparedLlmCall,
    ) -> Option<crate::thinking_scope::ThinkingRecoveryScope> {
        use lingxi_llm_client::providers::anthropic::beta_repair::Beta;
        if !prepared.route.protocol.is_anthropic_family() {
            return None;
        }
        let scope = prepared.thinking_recovery_scope.as_ref()?;
        Self::request_beta_tokens(prepared)
            .iter()
            .any(|beta| beta == Beta::MidConversationToolChanges.header())
            .then(|| scope.clone())
    }

    pub(in crate::service) fn request_beta_tokens(
        prepared: &crate::PreparedLlmCall,
    ) -> Vec<String> {
        lingxi_llm_client::providers::anthropic::request_policy::message_beta_tokens(
            prepared.route.protocol,
            prepared.computed_beta_headers.clone(),
            &prepared.provider_request.body_json,
        )
    }

    fn capture_native_beta_rejection_candidates(
        &self,
        prepared: &mut crate::PreparedLlmCall,
        provider: Option<Provider>,
        endpoint: Endpoint,
        context_hint_beta: bool,
        display_updates_applied: bool,
    ) -> Result<(), LlmError> {
        if !Self::is_main_anthropic_beta_request(prepared) || provider.is_none() {
            prepared.native_beta_rejection_candidates.clear();
            return Ok(());
        }

        // Native's `oT` is computed before spreading caller extra-body fields.
        // Recompute it from Host's exact provider/model context even when a
        // caller `betas` field suppresses the generated wire header. The
        // existing provider helper owns the beta admission algorithm; the SDK
        // owns parsing and rejection classification.
        // The provider helper only reads the request header map. Start with an
        // empty request so neither profile.extra.headers nor any other
        // caller-owned beta header is mistaken for Native's computed list.
        let mut candidate_request = crate::ProviderRequest::default();
        if let Some(provider) = provider {
            let context = self.beta_context(prepared).with_context_hint(
                context_hint_beta
                    || lingxi_llm_client::providers::anthropic::request_policy::has_field_path(
                        &prepared.provider_request.body_json,
                        &["context_hint"],
                    ),
            );
            // Native calls `zdr` before it materializes Ww/oT, so an admitted
            // structured-output beta belongs in this snapshot. The SDK's
            // fixed `iNe` registry still decides whether it can be rejected.
            let custom_betas = self.custom_cli_betas_for_header(prepared);
            apply_beta_header_with_auth_and_custom(
                &mut candidate_request,
                provider,
                endpoint,
                &context,
                self.effective_subscriber().is_subscriber,
                &custom_betas,
            );
        }

        let fast_identity =
            self.fast_rejection_identity(&prepared.route.resolved_route.request_model);
        let model_unsupported_identity = self.model_unsupported_identity(prepared);
        candidate_request.body_json = lingxi_llm_client::protocol::JsonPayload::parse("{}")
            .expect("empty candidate body is valid JSON");
        lingxi_llm_client::providers::anthropic::beta_repair::apply_rejections(
            &mut candidate_request.headers,
            &mut candidate_request.body_json,
            &prepared.beta_rejection_state,
            Some(&fast_identity),
            Some(&model_unsupported_identity),
            &lingxi_llm_client::providers::anthropic::beta_repair::ModelBetaRejections::for_process(
            ),
        )
        .map_err(crate::upstream::error)?;

        if display_updates_applied {
            use lingxi_llm_client::providers::anthropic::beta_repair;
            let mut betas = beta_repair::request_betas(&candidate_request.headers);
            betas.retain(|beta| beta != crate::model::betas::REDACT_THINKING);
            if !betas.iter().any(|beta| {
                beta == lingxi_llm_client::providers::anthropic::thinking_display::UPDATES_BETA
            }) {
                betas.push(
                    lingxi_llm_client::providers::anthropic::thinking_display::UPDATES_BETA.into(),
                );
            }
            lingxi_llm_client::providers::anthropic::request_policy::set_header(
                &mut candidate_request.headers,
                "anthropic-beta",
                &betas.join(","),
            );
        }

        let afk_admitted = self.beta_context(prepared).afk_mode;
        Self::append_native_afk_beta_to_headers(
            prepared.route.protocol,
            prepared.anthropic_request_kind,
            afk_admitted,
            &mut candidate_request.headers,
        );
        prepared.native_beta_rejection_candidates =
            lingxi_llm_client::providers::anthropic::beta_repair::request_betas(
                &candidate_request.headers,
            );
        Ok(())
    }

    pub(in crate::service) fn native_beta_rejection_candidates(
        prepared: &crate::PreparedLlmCall,
    ) -> Vec<String> {
        prepared.native_beta_rejection_candidates.clone()
    }

    pub(in crate::service) fn request_header_beta_tokens(
        prepared: &crate::PreparedLlmCall,
    ) -> Vec<String> {
        lingxi_llm_client::providers::anthropic::beta_repair::request_betas(
            &prepared.provider_request.headers,
        )
    }

    /// Native-owned Bedrock body betas before the `CLAUDE_CODE_EXTRA_BODY`
    /// spread: model policy plus route-filtered CLI additions that Bedrock
    /// requires in `anthropic_beta`. Arbitrary extra-body values are excluded.
    fn planned_bedrock_body_betas(&self, prepared: &crate::PreparedLlmCall) -> Vec<String> {
        if prepared.route.protocol != crate::ProtocolFamily::BedrockClaude {
            return Vec::new();
        }
        let mut betas = bedrock_extra_body_betas(&self.beta_context(prepared));
        for beta in self.custom_cli_betas(prepared) {
            if crate::model::betas::BEDROCK_EXTRA_PARAMS_HEADERS.contains(&beta.as_str())
                && !betas.iter().any(|existing| existing == &beta)
            {
                betas.push(beta);
            }
        }
        betas
    }

    /// Native's `DJe`: planned beta membership is recorded from the final
    /// host/header producer plus the Bedrock body producer set above, before
    /// the `CLAUDE_CODE_EXTRA_BODY` spread. A token that appears only in an
    /// arbitrary extra-body array is not a planned beta request.
    fn capture_planned_native_beta_context(&self, prepared: &mut crate::PreparedLlmCall) {
        let generated_body = self.planned_bedrock_body_betas(prepared);
        let suppressed = prepared.context_1m_beta_suppressed;
        let mut header = prepared
            .computed_beta_headers
            .iter()
            .filter(|beta| !suppressed || beta.as_str() != crate::model::betas::CONTEXT_1M)
            .cloned()
            .collect::<Vec<_>>();
        if prepared.native_thinking_binding_controls_beta_requested == Some(true)
            && !header.iter().any(|beta| {
                beta == lingxi_llm_client::providers::anthropic::beta_repair::Beta::ThinkingBindingControls.header()
            })
        {
            header.push(
                lingxi_llm_client::providers::anthropic::beta_repair::Beta::ThinkingBindingControls
                    .header()
                    .to_owned(),
            );
        }
        prepared.planned_native_beta_context = Some(crate::client::PlannedNativeBetaContext {
            header,
            generated_body: generated_body
                .into_iter()
                .filter(|beta| !suppressed || beta.as_str() != crate::model::betas::CONTEXT_1M)
                .collect(),
        });
    }

    pub(in crate::service) fn remove_beta_header_token(
        prepared: &mut crate::PreparedLlmCall,
        beta: &str,
    ) {
        if let Some((name, value)) = prepared
            .provider_request
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
            .map(|(name, value)| (name.clone(), value.clone()))
        {
            let kept = value
                .split(',')
                .map(str::trim)
                .filter(|candidate| *candidate != beta)
                .collect::<Vec<_>>();
            if kept.is_empty() {
                prepared.provider_request.headers.remove(&name);
            } else {
                prepared
                    .provider_request
                    .headers
                    .insert(name, kept.join(","));
            }
        }
        prepared
            .computed_beta_headers
            .retain(|candidate| candidate != beta);
    }

    pub(in crate::service) fn prepared_per_turn_effort(
        &self,
        prepared: &crate::PreparedLlmCall,
    ) -> Option<lingxi_core::types::utf16_json::Utf16JsonProjection> {
        let route_admitted = prepared.route.protocol == crate::ProtocolFamily::FoundryClaude
            || self.native_anthropic_request_selected(prepared);
        (prepared.anthropic_request_kind
            == lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main
            && route_admitted
            && lingxi_llm_client::providers::anthropic::supports_per_message_effort(
                &prepared.route.resolved_route.request_model,
            ))
        .then(|| {
            prepared
                .effort_policy
                .as_ref()
                .and_then(|policy| policy.value.as_ref())
        })
        .flatten()
        .filter(|_| Self::has_native_per_turn_effort_marker(&prepared.provider_request.body_json))
        .and_then(|value| {
            lingxi_core::types::utf16_json::Utf16JsonProjection::parse(value.as_json_source()).ok()
        })
        .filter(|value| value.string_units("").is_some())
    }

    fn apply_refusal_headers(prepared: &mut crate::PreparedLlmCall) {
        let Some(context) = &prepared.refusal_fallback_context else {
            return;
        };
        let first_party = Self::dispatch_first_party(prepared);
        lingxi_llm_client::providers::anthropic::refusal_fallback::apply_request_headers(
            &mut prepared.provider_request.headers,
            first_party,
            context.refusal_header_armed,
            context.refusal_occurred,
            context.refusal_lane_enabled,
            context.refusal_origin_request_id.as_deref(),
        );
    }

    fn apply_server_fallback(prepared: &mut crate::PreparedLlmCall) -> Result<(), LlmError> {
        if prepared.anthropic_request_kind
            != lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main
            || !matches!(
                prepared.route.protocol,
                crate::ProtocolFamily::AnthropicMessages
                    | crate::ProtocolFamily::FoundryClaude
                    | crate::ProtocolFamily::BedrockClaude
                    | crate::ProtocolFamily::VertexClaude
            )
        {
            return Ok(());
        }
        use lingxi_llm_client::providers::anthropic::fallback_request::{
            LaneMode, RequestPolicy, ServerLane,
        };
        let mut policy = prepared
            .server_fallback
            .clone()
            .or_else(|| {
                let facts = prepared
                    .refusal_fallback_context
                    .as_ref()?
                    .server_fallback
                    .as_ref()?;
                let mut host_policy = facts.policy.clone();
                let sticky = prepared.server_fallback_betas.snapshot();
                host_policy.explicit_beta_rejected |= sticky.explicit_rejected;
                host_policy.default_beta_rejected |= sticky.default_rejected;
                let decision = host_policy.decide(&facts.model, &facts.query);
                Some(RequestPolicy {
                    lane: decision.server_lane.map(|lane| ServerLane {
                        for_model: lane.for_model,
                        model: lane.model,
                        mode: match lane.mode {
                            lingxi_core::host::refusal_server::Mode::Default => LaneMode::Default,
                            lingxi_core::host::refusal_server::Mode::Explicit => LaneMode::Explicit,
                        },
                    }),
                    explicit_target_eligible: host_policy.explicit_target_eligible,
                    silent_arm: host_policy.silent_arm,
                    threaded_request: host_policy.threaded_request,
                    beta_transport_enabled: host_policy.beta_transport_enabled,
                    simulated_proxy_usage: false,
                })
            })
            .unwrap_or(RequestPolicy {
                beta_transport_enabled: true,
                ..Default::default()
            });
        policy.simulated_proxy_usage |=
            crate::structured_output::bool_environment("CLAUDE_CODE_SIMULATE_PROXY_USAGE");
        let first_party = Self::dispatch_first_party(prepared);
        prepared.server_fallback_lane = first_party.then(|| policy.lane.clone()).flatten();
        policy
            .apply(
                &prepared.route.resolved_route.request_model,
                first_party,
                &prepared.server_fallback_betas,
                &mut prepared.provider_request.body_json,
                &mut prepared.provider_request.headers,
                &mut prepared.provider_request.json_string_overrides,
            )
            .map_err(crate::upstream::error)
    }

    /// Apply the main/side extra-body policy after automatic fields and beta
    /// selection, preserving JS spread collisions and property ordering.
    pub(in crate::service) fn merge_extra_body(
        &self,
        prepared: &mut crate::PreparedLlmCall,
    ) -> Result<(), LlmError> {
        if !Self::is_anthropic_family_protocol(&prepared.route.protocol) {
            return Ok(());
        }
        // Bedrock carries a narrow beta subset in `anthropic_beta` inside the
        // body. Other Anthropic-family routes carry their betas in headers.
        let body_betas = self.planned_bedrock_body_betas(prepared);
        let extra_body = prepared.extra_body.clone().unwrap_or_else(|| {
            lingxi_llm_client::protocol::JsonPayload::parse("{}")
                .expect("empty object is valid JSON")
        });
        let policy = lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestPolicy {
            protocol: prepared.route.protocol,
            request_kind: prepared.anthropic_request_kind,
            plain_text_message_content: prepared.plain_text_message_content,
            plain_text_message_content_indices: prepared.plain_text_message_content_indices.clone(),
            explicit_empty_tools: prepared.anthropic_request_kind
                == lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main,
            extra_body,
            body_betas,
            effort: prepared.effort_policy.clone(),
            context_management: (self.anthropic.anthropic_client_metadata.is_some()
                && prepared.route.protocol == crate::ProtocolFamily::AnthropicMessages
                && self.native_anthropic_endpoint_selected(prepared))
            .then(|| {
                lingxi_llm_client::providers::anthropic::request_policy::AnthropicContextManagement {
                    has_thinking: matches!(
                        prepared.provider_request.body_json.string_at("/thinking/type").as_deref(),
                        Some("enabled" | "adaptive")
                    ),
                    tool_clearing: None,
                }
            }),
            message_header_parameters: matches!(
                prepared.route.protocol,
                crate::ProtocolFamily::AnthropicMessages
                    | crate::ProtocolFamily::FoundryClaude
                    | crate::ProtocolFamily::BedrockClaude
                    | crate::ProtocolFamily::VertexClaude
            ),
            ..Default::default()
        };
        policy
            .apply_with_native_layout(
                &mut prepared.provider_request.body_json,
                &mut prepared.provider_request.headers,
                &mut prepared.provider_request.json_string_overrides,
                &mut prepared.provider_request.url,
                prepared.stream_fallback,
            )
            .map_err(crate::upstream::error)
    }

    fn apply_structured_output_beta(prepared: &mut crate::PreparedLlmCall) {
        if matches!(
            prepared.route.protocol,
            crate::ProtocolFamily::AnthropicMessages
                | crate::ProtocolFamily::FoundryClaude
                | crate::ProtocolFamily::BedrockClaude
                | crate::ProtocolFamily::VertexClaude
        ) && !Self::extra_replaces_message_betas(prepared)
            && crate::structured_output::native_hun_enabled(
                &prepared.route.resolved_route.provider_id,
                prepared.route.protocol,
                &prepared.route.resolved_route.request_model,
            )
            .unwrap_or(true)
            && lingxi_llm_client::providers::anthropic::request_policy::has_field_path(
                &prepared.provider_request.body_json,
                &["output_config", "format"],
            )
        {
            lingxi_llm_client::providers::anthropic::request_policy::merge_beta_header(
                &mut prepared.provider_request.headers,
                &[crate::model::betas::STRUCTURED_OUTPUTS.to_string()],
            );
        }
    }

    fn extra_replaces_message_betas(prepared: &crate::PreparedLlmCall) -> bool {
        matches!(
            prepared.route.protocol,
            crate::ProtocolFamily::AnthropicMessages
                | crate::ProtocolFamily::FoundryClaude
                | crate::ProtocolFamily::BedrockClaude
                | crate::ProtocolFamily::VertexClaude
        ) && prepared.extra_body.as_ref().is_some_and(|extra| {
            lingxi_llm_client::providers::anthropic::request_policy::has_field(extra, "betas")
        })
    }

    pub(in crate::service) fn inject_headers(
        &self,
        prepared: &mut crate::PreparedLlmCall,
        dispatch: DispatchHeaderState,
    ) -> Result<(DispatchAttempt, bool), LlmError> {
        self.apply_attempt_headers(prepared, dispatch, Endpoint::MessagesCreate)
    }

    pub(in crate::service) fn inject_stream_headers(
        &self,
        prepared: &mut crate::PreparedLlmCall,
        dispatch: DispatchHeaderState,
    ) -> Result<(DispatchAttempt, bool), LlmError> {
        self.apply_attempt_headers(prepared, dispatch, Endpoint::MessagesCreateStream)
    }

    fn apply_attempt_headers(
        &self,
        prepared: &mut crate::PreparedLlmCall,
        dispatch: DispatchHeaderState,
        endpoint: Endpoint,
    ) -> Result<(DispatchAttempt, bool), LlmError> {
        // Provider-specific tool-search beta: first-party/Foundry use
        // advanced-tool-use, Vertex uses tool-search-tool, and Bedrock carries
        // tool-search-tool in the request body's anthropic_beta array.
        let beta_provider = match prepared.route.protocol {
            crate::ProtocolFamily::AnthropicMessages
                if prepared.route.resolved_route.provider_id
                    == crate::ProviderId::AnthropicFirstParty =>
            {
                Some(Provider::Anthropic)
            }
            crate::ProtocolFamily::FoundryClaude => Some(Provider::Foundry),
            crate::ProtocolFamily::VertexClaude => Some(Provider::Vertex),
            crate::ProtocolFamily::BedrockClaude => Some(Provider::Bedrock),
            _ => None,
        };
        if let Some(provider) =
            beta_provider.filter(|_| !Self::extra_replaces_message_betas(prepared))
        {
            let ctx = self.beta_context(prepared).with_context_hint(
                dispatch.context_hint_beta
                    || lingxi_llm_client::providers::anthropic::request_policy::has_field_path(
                        &prepared.provider_request.body_json,
                        &["context_hint"],
                    ),
            );
            let custom_betas = self.custom_cli_betas_for_header(prepared);
            apply_beta_header_with_auth_and_custom(
                &mut prepared.provider_request,
                provider,
                endpoint,
                &ctx,
                self.effective_subscriber().is_subscriber,
                &custom_betas,
            );
        }
        Self::apply_structured_output_beta(prepared);
        // User-Agent (Task 3) — provider-aware (see apply_user_agent).
        self.apply_user_agent(prepared);
        // (cc 2.1.219) opt-in dispatch-routing header (see apply_dispatch_header).
        let attempt = Self::apply_dispatch_header(prepared, dispatch);
        Self::apply_refusal_headers(prepared);
        Self::apply_server_fallback(prepared)?;
        let server_fallback_parameter_added =
            lingxi_llm_client::providers::anthropic::request_policy::has_field_path(
                &prepared.provider_request.body_json,
                &["fallbacks"],
            );
        // CLAUDE_CODE_EXTRA_BODY merge — after the beta header is computed from the
        // pre-merge body (claude-code `B0t` spread; 2.1.207).
        let display_updates_applied = self.apply_automatic_thinking_display(prepared)?;
        self.append_native_afk_beta_after_display(prepared);
        self.capture_native_beta_rejection_candidates(
            prepared,
            beta_provider,
            endpoint,
            dispatch.context_hint_beta,
            display_updates_applied,
        )?;
        self.merge_extra_body(prepared)?;
        // Final resolved-route guard. `CLAUDE_CODE_EXTRA_BODY` is merged above,
        // so this must run last to prevent it from reintroducing first-party
        // speed/beta fields on custom or unsupported routes.
        self.enforce_fast_route(prepared)?;
        self.update_native_thinking_binding_controls_beta_fact(prepared);
        prepared.computed_beta_headers =
            lingxi_llm_client::providers::anthropic::beta_repair::request_betas(
                &prepared.provider_request.headers,
            );
        self.capture_planned_native_beta_context(prepared);
        Ok((attempt, server_fallback_parameter_added))
    }
}
