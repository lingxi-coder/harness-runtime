//! Application usage projection over the shared Anthropic decoder.
use crate::Usage;
use lingxi_llm_client::{self as sdk, WireCodec};
use serde_json::{json, Value};

/// Normalize provider usage using the shared wire client's validation rules.
#[must_use]
pub fn normalize_anthropic_usage(value: &Value) -> Usage {
    let profile = serde_json::from_value(json!({
        "provider_id": "anthropic", "profile_name": "anthropic",
        "base_url": "https://api.anthropic.com", "protocol": "anthropic_messages",
        "auth": "none", "models": []
    }))
    .expect("static usage profile");
    let context = sdk::CodecContext::new(&profile, "", sdk::RequestMode::Complete);
    let response = sdk::HttpResponse {
        status: 200,
        headers: Default::default(),
        body: serde_json::to_vec(&json!({"usage": value}))
            .expect("usage JSON")
            .into(),
    };
    let codec = sdk::AnthropicMessagesCodec;
    let mut usage = crate::upstream::usage(
        &codec.response_usage(&response, &context),
        &codec.response_inference(&response, &context),
    )
    .map(|(usage, _)| usage)
    .unwrap_or_default();
    usage.provider_metadata = value.clone();
    usage
}
