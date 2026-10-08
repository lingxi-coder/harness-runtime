//! Host policy for automatically requested Anthropic JSON schemas.
//! Claude Code 2.1.288 Tvn/BK use independent typed environment gates.
//! Explicit extra-body formats are merged separately.

use crate::{LlmRequest, ProtocolFamily};

pub(crate) fn disabled_for(
    request: &LlmRequest,
    protocol: ProtocolFamily,
    model: &str,
    capability: bool,
    extra: Option<&serde_json::Map<String, serde_json::Value>>,
) -> bool {
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
        || experimental_betas_disabled()
        || bool_environment(branding::DISABLE_STRUCTURED_OUTPUTS_ENV)
        || (request.execution.anthropic_request_kind
            == lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main
            && extra
                .and_then(|extra| extra.get("output_config"))
                .and_then(serde_json::Value::as_object)
                .is_some_and(|config| config.contains_key("format"))))
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
