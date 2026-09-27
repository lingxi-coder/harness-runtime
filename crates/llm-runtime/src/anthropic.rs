//! Application usage projection over the shared Anthropic decoder.
use crate::{Usage, WireCodec};
use serde_json::Value;

/// Normalize provider usage using the shared wire client's validation rules.
#[must_use]
pub fn normalize_anthropic_usage(value: &Value) -> Usage {
    let mut usage = crate::AnthropicMessagesCodec::new("https://api.anthropic.com", "2023-06-01")
        .response_usage(&crate::ProviderResponse::json(
            200,
            serde_json::json!({"usage":value}),
        ))
        .map(|(usage, _)| usage)
        .unwrap_or_default();
    usage.provider_metadata = value.clone();
    usage
}
