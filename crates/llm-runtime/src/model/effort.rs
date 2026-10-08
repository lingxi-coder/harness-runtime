//! Host-selected native effort, pinned to the physical preparation.
use super::betas::{self, Provider};
use crate::{LlmRequest, ProtocolFamily, ProviderId};
use lingxi_core::host::effort::{clamp_side_effort, resolve_effort, EffortCapabilities};
use lingxi_llm_client::protocol::{CapabilitySupport, EffortSupport, ReasoningEffort};
use lingxi_llm_client::providers::anthropic::request_policy::{
    AnthropicEffortPolicy, AnthropicRequestKind,
};
use serde_json::Value;

pub(crate) fn capabilities(provider: Provider, model: &str) -> EffortCapabilities {
    let canonical = betas::beta_canonical(model);
    let first_party = matches!(provider, Provider::Anthropic | Provider::Foundry);
    let supported = betas::effort_capable(provider, model);
    let max = !canonical.contains("claude-3-")
        && ![
            "claude-opus-4-0",
            "claude-opus-4-1",
            "claude-opus-4-5",
            "claude-sonnet-4-0",
            "claude-sonnet-4-5",
            "claude-haiku-4-5",
        ]
        .contains(&canonical.as_str())
        && (first_party || canonical == "claude-mythos-5");
    let xhigh = max && !["claude-opus-4-6", "claude-sonnet-4-6"].contains(&canonical.as_str());
    EffortCapabilities {
        supported,
        max,
        xhigh,
        thinking_disabled_cap: canonical == "claude-opus-5",
    }
}

// Native we(): baked defaults remain available when the directory/configured
// model has no default. Current served/default overrides are separate facts.
fn baked_catalog_default(model: &str) -> &'static str {
    match betas::beta_canonical(model).as_str() {
        "claude-opus-4-7" => "xhigh",
        "claude-opus-5-5" | "claude-sonnet-5-5" => "medium",
        _ => "high",
    }
}

// Native jw/J/pQ/xXe: explicit support precedes a runtime level list, and
// explicit per-level support requires effort support. Unknown facts retain
// the native provider/model fallback instead of becoming a false observation.
fn with_catalog_facts(mut native: EffortCapabilities, facts: &EffortSupport) -> EffortCapabilities {
    native.supported = match facts.support {
        CapabilitySupport::Supported => true,
        CapabilitySupport::Unsupported => false,
        CapabilitySupport::Unknown => facts
            .levels
            .as_ref()
            .map_or(native.supported, |v| !v.is_empty()),
    };
    let level = |effort, fallback| {
        if facts.support == CapabilitySupport::Unsupported {
            return false;
        }
        match facts
            .level_support
            .get(&effort)
            .copied()
            .unwrap_or_default()
        {
            CapabilitySupport::Supported => native.supported,
            CapabilitySupport::Unsupported => false,
            CapabilitySupport::Unknown => facts
                .levels
                .as_ref()
                .map_or(fallback, |v| v.contains(&effort)),
        }
    };
    native.max = level(ReasoningEffort::Max, native.max);
    native.xhigh = level(ReasoningEffort::XHigh, native.xhigh);
    native
}

// Resolve admitted picker aliases from the selected SDK profile. Native
// served Rt/Be override state remains a separate, explicit integration.
fn settings_identity(profile: &lingxi_llm_client::protocol::ProviderProfile, key: &str) -> String {
    let key = lingxi_core::host::effort::trim_js_whitespace(key).to_lowercase();
    let lookup = key.strip_suffix("[1m]").unwrap_or(&key);
    let identity = profile
        .models
        .iter()
        .find(|entry| {
            entry.request_model.to_lowercase() == lookup
                || entry.display_model.to_lowercase() == lookup
                || entry
                    .aliases
                    .iter()
                    .any(|alias| alias.to_lowercase() == lookup)
        })
        .map_or(key.as_str(), |entry| {
            entry
                .foundry
                .as_ref()
                .map_or(entry.request_model.as_str(), |foundry| {
                    foundry.model_id.as_str()
                })
        });
    let identity = betas::beta_canonical(identity);
    identity
        .strip_suffix("[1m]")
        .unwrap_or(&identity)
        .to_string()
}

pub(crate) fn command_snapshot(
    request: &LlmRequest,
    protocol: ProtocolFamily,
    provider_id: &ProviderId,
    profile: &lingxi_llm_client::protocol::ProviderProfile,
    model: &str,
) -> Option<lingxi_core::host::effort::EffortCommandSnapshot> {
    if !request.execution.resolve_native_effort {
        return None;
    }
    let provider = match protocol {
        // A native first-party identity remains native with a configured base
        // URL. Endpoint trust is relevant to auth/security, not effort defaults.
        ProtocolFamily::AnthropicMessages if *provider_id == ProviderId::AnthropicFirstParty => {
            Provider::Anthropic
        }
        ProtocolFamily::FoundryClaude => Provider::Foundry,
        ProtocolFamily::VertexClaude => Provider::Vertex,
        ProtocolFamily::BedrockClaude => Provider::Bedrock,
        _ => return None,
    };
    let selected = profile
        .models
        .iter()
        .find(|entry| entry.request_model == model);
    let identity = selected
        .and_then(|entry| entry.foundry.as_ref())
        .map_or(model, |foundry| foundry.model_id.as_str());
    let facts = selected.map(|entry| entry.info.features.on_connection(&profile.info.features));
    let native = capabilities(provider, identity);
    let capabilities = facts
        .as_ref()
        .map_or(native, |facts| with_catalog_facts(native, &facts.effort));
    let mut primary = request
        .input
        .thinking
        .as_ref()
        .and_then(|thinking| thinking.effort)
        .map(|effort| Value::String(effort.as_str().into()));
    let mut state = request.execution.effort_state.clone();
    if state.catalog_default.is_none() {
        state.catalog_default = facts
            .as_ref()
            .and_then(|facts| facts.effort.default)
            .filter(|effort| !matches!(effort, ReasoningEffort::None | ReasoningEffort::Minimal))
            .map(|effort| Value::String(effort.as_str().to_owned()))
            .or_else(|| Some(Value::String(baked_catalog_default(identity).into())));
    }
    if let Some(layers) = &request.execution.effort_settings {
        state.settings_cap = lingxi_core::host::effort::settings_cap(layers, model, |key| {
            settings_identity(profile, key)
        });
    }
    let side = request.execution.anthropic_request_kind == AnthropicRequestKind::SideQuery;
    let table = request
        .execution
        .inherited_effort_settings
        .as_ref()
        .map(|layers| {
            lingxi_core::host::effort_table::settings_table(
                layers,
                &request.execution.effort_table_options,
                |key| settings_identity(profile, key),
            )
        });
    let settings_key = settings_identity(profile, model);
    if !side && primary.is_none() {
        primary = request
            .execution
            .session_effort
            .resolve(table.as_ref(), &settings_key);
    }
    let organization_start_effort = request
        .execution
        .effort_table_options
        .override_model
        .as_ref()
        .filter(|model| settings_identity(profile, model) == settings_key)
        .map(|_| {
            let inherited = table
                .as_ref()
                .and_then(|table| table.inherited(&settings_key))
                .map(Value::String);
            let mut defaults = state.clone();
            defaults.turn = None;
            defaults.hook = None;
            defaults.carried = None;
            let value = resolve_effort(
                EffortCapabilities {
                    supported: true,
                    ..capabilities
                },
                &defaults,
                inherited.as_ref(),
                None,
            );
            value
                .as_ref()
                .and_then(Value::as_str)
                .filter(|level| lingxi_core::host::effort::LEVELS.contains(level))
                .unwrap_or("high")
                .to_owned()
        });
    Some(lingxi_core::host::effort::EffortCommandSnapshot {
        model: model.to_owned(),
        settings_key,
        session: request.execution.session_effort.clone(),
        primary,
        capabilities,
        state,
        save_default: true,
        organization_start_effort,
        user_settings_path: request
            .execution
            .effort_settings
            .as_ref()
            .and_then(|layers| {
                layers
                    .iter()
                    .find(|layer| layer.source == lingxi_core::settings::tracer::Source::User)
                    .and_then(|layer| layer.path.clone())
            }),
    })
}

pub(crate) fn prepare(
    request: &LlmRequest,
    protocol: ProtocolFamily,
    provider_id: &ProviderId,
    profile: &lingxi_llm_client::protocol::ProviderProfile,
    model: &str,
) -> Option<AnthropicEffortPolicy> {
    let snapshot = command_snapshot(request, protocol, provider_id, profile, model)?;
    let capabilities = snapshot.capabilities;
    let primary = snapshot.primary;
    let mut state = snapshot.state;
    let side = request.execution.anthropic_request_kind == AnthropicRequestKind::SideQuery;
    if side && state.hook.is_none() {
        state.hook = primary.clone();
    }
    let environment = std::env::var(branding::EFFORT_LEVEL_ENV).ok();
    let value = if side && state.hook.is_none() {
        None
    } else {
        let selected = resolve_effort(
            capabilities,
            &state,
            primary.as_ref(),
            environment.as_deref(),
        );
        if side {
            clamp_side_effort(
                selected,
                capabilities,
                request.execution.side_thinking_disabled
                    || request.input.thinking.as_ref().is_some_and(|thinking| {
                        thinking.mode == Some(lingxi_llm_client::protocol::ThinkingMode::Disabled)
                    }),
            )
        } else {
            selected
        }
    };
    Some(AnthropicEffortPolicy {
        supported: capabilities.supported,
        value,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baked_defaults_and_sdk_launch_facts_match_native_catalog() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/effort_catalog_2_1_287.json"
        ))
        .unwrap();
        let sdk = lingxi_llm_client::builtin_providers()
            .unwrap()
            .into_iter()
            .find(|p| p.profile_name == "anthropic")
            .unwrap();
        for row in fixture["bakedDefaults"].as_array().unwrap() {
            let model = row["model"].as_str().unwrap();
            assert_eq!(
                baked_catalog_default(model),
                row["default"].as_str().unwrap(),
                "{model}"
            );
            if let Some(selected) = sdk.models.iter().find(|entry| {
                entry.request_model == model || entry.aliases.iter().any(|alias| alias == model)
            }) {
                if let Some(default) = selected.info.features.effort.default {
                    assert_eq!(
                        default.as_str(),
                        row["default"].as_str().unwrap(),
                        "SDK default {model}"
                    );
                }
                for (capability, level) in [
                    ("max_effort", ReasoningEffort::Max),
                    ("xhigh_effort", ReasoningEffort::XHigh),
                ] {
                    if row["capabilities"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|cap| cap == capability)
                    {
                        assert!(
                            selected
                                .info
                                .features
                                .effort
                                .levels
                                .as_ref()
                                .unwrap()
                                .contains(&level),
                            "SDK {capability} {model}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn catalog_precedence_matches_native_287() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/effort_catalog_2_1_287.json"
        ))
        .unwrap();
        let fact = |value: &Value| match value.as_bool() {
            Some(true) => CapabilitySupport::Supported,
            Some(false) => CapabilitySupport::Unsupported,
            None => CapabilitySupport::Unknown,
        };
        for case in fixture["cases"].as_array().unwrap() {
            let input = &case["input"];
            let base = &input["native"];
            let facts = &input["facts"];
            let result = with_catalog_facts(
                EffortCapabilities {
                    supported: base["supported"].as_bool().unwrap(),
                    max: base["max"].as_bool().unwrap(),
                    xhigh: base["xhigh"].as_bool().unwrap(),
                    thinking_disabled_cap: false,
                },
                &EffortSupport {
                    support: fact(&facts["effort"]),
                    levels: facts
                        .get("levels")
                        .map(|v| serde_json::from_value(v.clone()).unwrap()),
                    level_support: [
                        (ReasoningEffort::Max, fact(&facts["max_effort"])),
                        (ReasoningEffort::XHigh, fact(&facts["xhigh_effort"])),
                    ]
                    .into_iter()
                    .collect(),
                    ..Default::default()
                },
            );
            assert_eq!(json_result(result), case["expected"], "{input}");
        }
    }

    fn json_result(caps: EffortCapabilities) -> Value {
        serde_json::json!({"supported": caps.supported, "max":caps.max,"xhigh":caps.xhigh})
    }
    #[test]
    fn native_identity_keeps_effort_with_base_override_but_custom_provider_does_not_gain_it() {
        let mut profile = lingxi_llm_client::builtin_providers()
            .unwrap()
            .into_iter()
            .find(|profile| profile.profile_name == "anthropic")
            .unwrap();
        profile.base_url = "http://configured-anthropic-base.test".into();
        let mut request = LlmRequest::new("claude-sonnet-5-5");
        request.execution.resolve_native_effort = true;
        let policy = prepare(
            &request,
            ProtocolFamily::AnthropicMessages,
            &ProviderId::AnthropicFirstParty,
            &profile,
            "claude-sonnet-5-5",
        )
        .unwrap();
        assert!(policy.supported);
        assert_eq!(policy.value, Some(serde_json::json!("medium")));
        let custom = ProviderId::Custom {
            name: "custom".into(),
        };
        assert!(prepare(
            &request,
            ProtocolFamily::AnthropicMessages,
            &custom,
            &profile,
            "claude-sonnet-5-5"
        )
        .is_none());
    }
}
