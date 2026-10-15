//! Host policy for automatically requested Anthropic JSON schemas.
//! Native 2.1.295 HUn/B6 gates apply only to represented Native route families;
//! generic compatible providers retain the existing host policy.
//! Explicit extra-body formats are merged separately.

use crate::{LlmRequest, ProtocolFamily, ProviderId};

/// Native `HUn(model)` is reached only for provider families accepted by
/// `DW(cc(model))`. In this Host, those are FirstParty and Foundry; Bedrock
/// and Vertex are recognized Native transports whose `DW` branch is false.
/// `None` leaves generic Anthropic-compatible callers on their existing policy.
pub(crate) fn native_hun_enabled_for(
    provider_id: &ProviderId,
    protocol: ProtocolFamily,
    model: &str,
    b6_disabled: bool,
    structured_outputs_disabled: bool,
) -> Option<bool> {
    match (provider_id, protocol) {
        (ProviderId::AnthropicFirstParty, ProtocolFamily::AnthropicMessages)
        | (ProviderId::FoundryClaude, ProtocolFamily::FoundryClaude) => Some(
            crate::model::betas::structured_outputs_model_capable(model)
                && !b6_disabled
                && !structured_outputs_disabled,
        ),
        (ProviderId::BedrockClaude, ProtocolFamily::BedrockClaude)
        | (ProviderId::VertexClaude, ProtocolFamily::VertexClaude) => Some(false),
        _ => None,
    }
}

pub(crate) fn native_hun_enabled(
    provider_id: &ProviderId,
    protocol: ProtocolFamily,
    model: &str,
) -> Option<bool> {
    native_hun_enabled_for(
        provider_id,
        protocol,
        model,
        crate::model::thinking_binding_controls::native_b6_disabled(),
        bool_environment(branding::DISABLE_STRUCTURED_OUTPUTS_ENV),
    )
}

pub(crate) fn disabled_for(
    request: &LlmRequest,
    provider_id: &ProviderId,
    protocol: ProtocolFamily,
    model: &str,
    capability: bool,
    extra: Option<&lingxi_llm_client::protocol::JsonPayload>,
) -> bool {
    let native_hun = native_hun_enabled(provider_id, protocol, model);
    matches!(
        protocol,
        ProtocolFamily::AnthropicMessages
            | ProtocolFamily::BedrockClaude
            | ProtocolFamily::VertexClaude
            | ProtocolFamily::FoundryClaude
    ) && matches!(
        request.input.output_format,
        lingxi_llm_client::protocol::OutputFormat::JsonSchema { .. }
    ) && (!capability
        || !crate::model::betas::structured_outputs_model_capable(model)
        || native_hun == Some(false)
        || (native_hun.is_none() && experimental_betas_disabled())
        || bool_environment(branding::DISABLE_STRUCTURED_OUTPUTS_ENV)
        || (request.execution.anthropic_request_kind
            == lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main
            && extra.is_some_and(|extra| {
                lingxi_llm_client::providers::anthropic::request_policy::has_field_path(
                    extra,
                    &["output_config", "format"],
                )
            })))
}

pub(crate) fn experimental_betas_disabled() -> bool {
    bool_environment("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS")
}

pub(crate) fn bool_environment(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .is_some_and(|value| bool_value(&value))
}

pub(crate) fn bool_value(value: &str) -> bool {
    // ECMAScript trim includes BOM and excludes U+0085.
    let value = value.trim_matches(|c| matches!(c, '\u{0009}'..='\u{000D}' | '\u{0020}' | '\u{00A0}' | '\u{1680}' | '\u{2000}'..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}'));
    matches!(
        value.to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(test)]
mod native_hun_tests {
    use super::{bool_value, native_hun_enabled_for};
    use crate::{ProtocolFamily, ProviderId};

    const MODEL: &str = "claude-sonnet-4-5-20250929";

    #[test]
    fn native_structured_output_disable_uses_native_typed_boolean_parser() {
        for (value, expected) in [
            ("true", true),
            ("1", true),
            ("yes", true),
            ("on", true),
            ("false", false),
            ("0", false),
            ("no", false),
            ("off", false),
            ("wat", false),
            ("", false),
        ] {
            assert_eq!(bool_value(value), expected, "M.bool({value:?})");
        }
    }

    #[test]
    fn native_hun_applies_b6_and_structured_output_disable_only_to_supported_routes() {
        let first_party = native_hun_enabled_for(
            &ProviderId::AnthropicFirstParty,
            ProtocolFamily::AnthropicMessages,
            MODEL,
            false,
            false,
        );
        assert_eq!(first_party, Some(true));
        assert_eq!(
            native_hun_enabled_for(
                &ProviderId::AnthropicFirstParty,
                ProtocolFamily::AnthropicMessages,
                MODEL,
                true,
                false,
            ),
            Some(false),
            "Native B6 includes raw experimental-beta disable and HIPAA taint"
        );
        assert_eq!(
            native_hun_enabled_for(
                &ProviderId::FoundryClaude,
                ProtocolFamily::FoundryClaude,
                MODEL,
                false,
                true,
            ),
            Some(false)
        );
        assert_eq!(
            native_hun_enabled_for(
                &ProviderId::FoundryClaude,
                ProtocolFamily::FoundryClaude,
                "claude-sonnet-3-7",
                false,
                false,
            ),
            Some(false),
            "HUn also rejects models below the Native structured-output threshold"
        );
    }

    #[test]
    fn native_hun_rejects_cloud_routes_and_preserves_generic_provider_policy() {
        assert_eq!(
            native_hun_enabled_for(
                &ProviderId::BedrockClaude,
                ProtocolFamily::BedrockClaude,
                MODEL,
                false,
                false,
            ),
            Some(false)
        );
        assert_eq!(
            native_hun_enabled_for(
                &ProviderId::VertexClaude,
                ProtocolFamily::VertexClaude,
                MODEL,
                false,
                false,
            ),
            Some(false)
        );
        assert_eq!(
            native_hun_enabled_for(
                &ProviderId::Custom {
                    name: "compatible-anthropic".into(),
                },
                ProtocolFamily::AnthropicMessages,
                MODEL,
                true,
                true,
            ),
            None,
            "generic providers retain their existing host policy"
        );
    }
}
