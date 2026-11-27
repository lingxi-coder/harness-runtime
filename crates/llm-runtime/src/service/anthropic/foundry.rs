//! Foundry tool capability observations and route-scoped downgrades.

use crate::service::ApiService;
use crate::LlmRequest;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(in crate::service) struct FoundryDeploymentKey {
    endpoint: String,
    /// Exact Native X.model UTF-16 identity after removing [1m]/[2m] tags.
    model: Vec<u16>,
}

pub(in crate::service) fn native_foundry_model_key(model_units: &[u16]) -> Vec<u16> {
    let mut result = Vec::with_capacity(model_units.len());
    let mut index = 0;
    while index < model_units.len() {
        if index + 3 < model_units.len()
            && model_units[index] == u16::from(b'[')
            && (model_units[index + 1] == u16::from(b'1')
                || model_units[index + 1] == u16::from(b'2'))
            && (model_units[index + 2] == u16::from(b'm')
                || model_units[index + 2] == u16::from(b'M'))
            && model_units[index + 3] == u16::from(b']')
        {
            index += 4;
            continue;
        }
        result.push(model_units[index]);
        index += 1;
    }
    result
}

fn native_foundry_deployment_endpoint(profile_base_url: &str) -> String {
    // `pl` keys the built-in Foundry client by `fl()`, which omits the
    // `/anthropic/` suffix when `ANTHROPIC_FOUNDRY_RESOURCE` supplies the
    // endpoint. Reuse that host value only when this is exactly the profile
    // URL produced from those same environment inputs; custom profiles stay
    // isolated by their own configured endpoint.
    match (
        crate::foundry_base_host_from_env(),
        crate::foundry_messages_base_url_from_env(),
    ) {
        (Some(host), Some(messages_url)) if messages_url == profile_base_url => host,
        _ => profile_base_url.to_owned(),
    }
}

#[cfg(test)]
pub(in crate::service) const NATIVE_TOOL_SEARCH_NAME: &str = "ToolSearch";
#[cfg(test)]
pub(in crate::service) const NATIVE_TOOL_SEARCH_DESCRIPTION: &str = concat!(
    "Fetches full schema definitions for deferred tools so they can be called.\n\n",
    "Deferred tools appear by name in <system-reminder> messages. Until fetched, only the name is known ",
    "— there is no parameter schema, so the tool cannot be invoked. This tool takes a query, matches it ",
    "against the deferred tool list, and returns the matched tools' complete JSONSchema definitions inside ",
    "a <functions> block. Once a tool's schema appears in that result, it is callable exactly like any tool ",
    "defined at the top of the prompt.\n\n",
    "Result format: each matched tool appears as one <function>{\"description\": \"...\", ",
    "\"name\": \"...\", \"parameters\": {...}}</function> line inside the <functions> block — ",
    "the same encoding as the tool list at the top of this prompt.\n\n",
    "Query forms:\n- \"select:Read,Edit,Grep\" — fetch these exact tools by name\n",
    "- \"notebook jupyter\" — keyword search, up to max_results best matches\n",
    "- \"+slack send\" — require \"slack\" in the name, rank by remaining terms",
);

// Native x_s removes this exact synthetic placeholder when hosted search is unsupported.
pub(in crate::service) const NATIVE_DEFERRED_TOOL_PLACEHOLDER_NAME: &str =
    "DeferredToolPlaceholder";
pub(in crate::service) const NATIVE_DEFERRED_TOOL_PLACEHOLDER_DESCRIPTION: &str =
    "Reserved placeholder that keeps deferred tool loading active; never call this tool.";

fn native_deferred_tool_placeholder_matches(tool: &lingxi_llm_client::protocol::ToolSpec) -> bool {
    tool.defer_loading
        && tool.name == NATIVE_DEFERRED_TOOL_PLACEHOLDER_NAME
        && tool
            .description
            .root_string_code_units()
            .is_some_and(|units| {
                units
                    == NATIVE_DEFERRED_TOOL_PLACEHOLDER_DESCRIPTION
                        .encode_utf16()
                        .collect::<Vec<_>>()
            })
}

impl ApiService {
    /// Native YKe semantics for the current Foundry deployment. An unobserved
    /// deployment or feature is allowed; only an explicit unsupported feature
    /// observation makes the result false. Non-Foundry routes have no answer.
    pub fn foundry_tool_search_supported(
        &self,
        model: &str,
        profile: Option<&str>,
        model_source_units: Option<&[u16]>,
    ) -> Option<bool> {
        let fallback_units;
        let model_units = match model_source_units {
            Some(units) => units,
            None => {
                fallback_units = model.encode_utf16().collect::<Vec<_>>();
                &fallback_units
            }
        };
        let key = self.foundry_deployment_key(model, profile, model_units)?;
        let capabilities = self
            .anthropic
            .foundry_capabilities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let unsupported = capabilities.get(&key);
        Some(!unsupported.is_some_and(|observed| {
            observed.contains("tool_search_server") || observed.contains("tool_search")
        }))
    }

    fn foundry_deployment_key(
        &self,
        model: &str,
        profile: Option<&str>,
        model_source_units: &[u16],
    ) -> Option<FoundryDeploymentKey> {
        if self.client().protocol_for_model(model, profile).ok()?
            != crate::ProtocolFamily::FoundryClaude
        {
            return None;
        }
        let route = self.client().resolve_media_route(model, profile).ok()?.main;
        if route.provider_id != crate::ProviderId::FoundryClaude {
            return None;
        }
        let client = self.client();
        let selected_profile = client.search_profile(model, Some(&route.profile_name))?;
        Some(FoundryDeploymentKey {
            endpoint: native_foundry_deployment_endpoint(&selected_profile.base_url),
            model: native_foundry_model_key(model_source_units),
        })
    }

    pub(in crate::service) fn project_cached_foundry_tool_features(
        &self,
        request: &LlmRequest,
    ) -> Option<LlmRequest> {
        if !request
            .input
            .tools
            .iter()
            .any(|tool| tool.defer_loading || tool.strict)
        {
            return None;
        }
        let key = self.foundry_deployment_key(
            &request.input.model,
            request.profile.as_deref(),
            &super::endpoint::native_request_model_identity(request),
        )?;
        let (search_unsupported, strict_unsupported) = {
            let capabilities = self
                .anthropic
                .foundry_capabilities
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let features = capabilities.get(&key)?;
            (
                features.contains("tool_search_server") || features.contains("tool_search"),
                features.contains("structured_outputs"),
            )
        };
        if !request.input.tools.iter().any(|tool| {
            (search_unsupported && tool.defer_loading) || (strict_unsupported && tool.strict)
        }) {
            return None;
        }
        let mut projected = request.clone();
        projected.input.tools.retain_mut(|tool| {
            if search_unsupported {
                if native_deferred_tool_placeholder_matches(tool) {
                    return false;
                }
                tool.defer_loading = false;
            }
            if strict_unsupported {
                tool.strict = false;
            }
            true
        });
        Some(projected)
    }

    /// A rejected feature warrants another call only if this attempt sent it.
    pub(in crate::service) fn recover_foundry_tools(
        &self,
        request: &LlmRequest,
        prepared: &crate::PreparedLlmCall,
        error: &crate::LlmError,
        status: u16,
        body: &lingxi_llm_client::protocol::JsonPayload,
        scope: &crate::model::retry_scope::ModelCallRetryScope,
    ) -> bool {
        let route = &prepared.route;
        if route.protocol != crate::ProtocolFamily::FoundryClaude {
            return false;
        }
        let profile = &route.resolved_route.profile_name;
        let model = super::endpoint::native_request_model_identity(request);
        let Some(features) = self.record_foundry_capability_observation(
            &request.input.model,
            &model,
            profile,
            route.protocol,
            &route.resolved_route.provider_id,
            status,
            body,
            error.provider_message(),
        ) else {
            return false;
        };
        let search = features
            .iter()
            .any(|feature| matches!(feature.as_str(), "tool_search" | "tool_search_server"));
        let strict = features
            .iter()
            .any(|feature| feature == "structured_outputs");
        let sent = |tool: &lingxi_llm_client::protocol::JsonPayload, field| {
            tool.payload_at(field)
                .is_some_and(|value| value.as_json_source() == "true")
        };
        let Some(tools) =
            crate::protocol::json_array_items(&prepared.provider_request.body_json, "/tools")
        else {
            return false;
        };
        let search_retry = search
            && tools.iter().any(|tool| sent(tool, "/defer_loading"))
            && scope.take_foundry_tool_repair(profile, &model, "tool_search");
        let strict_retry = strict
            && tools.iter().any(|tool| sent(tool, "/strict"))
            && scope.take_foundry_tool_repair(profile, &model, "structured_outputs");
        search_retry || strict_retry
    }

    pub(in crate::service) fn record_foundry_capability_observation(
        &self,
        model: &str,
        model_source_units: &[u16],
        profile_name: &str,
        protocol: crate::ProtocolFamily,
        provider_id: &crate::ProviderId,
        status: u16,
        body: &lingxi_llm_client::protocol::JsonPayload,
        provider_message: Option<&str>,
    ) -> Option<Vec<String>> {
        if protocol != crate::ProtocolFamily::FoundryClaude
            || provider_id != &crate::ProviderId::FoundryClaude
            || status != 400
        {
            return None;
        }
        let Some(key) = self.foundry_deployment_key(model, Some(profile_name), model_source_units)
        else {
            return None;
        };
        // Native UNn first reads `error.error.message`, then falls back to the
        // provider error's message. The raw SDK body maps to the first shape;
        // `/message` is the source-body counterpart of the fallback.
        let message = body
            .string_at("/error/message")
            .or_else(|| provider_message.map(str::to_owned))?;
        let features = lingxi_llm_client::providers::anthropic::error_recognition::foundry_unsupported_feature_names(&message)?;
        self.anthropic
            .foundry_capabilities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(key)
            .or_default()
            .extend(features.iter().cloned());
        Some(features)
    }
}
