//! Regression fixtures for host conversion using the independent SDK.

pub(super) fn json_payload(value: serde_json::Value) -> lingxi_llm_client::protocol::JsonPayload {
    lingxi_llm_client::protocol::JsonPayload::from_generated_value(&value)
        .expect("test fixture is valid JSON")
}

pub(super) fn json_payload_value(
    payload: &lingxi_llm_client::protocol::JsonPayload,
) -> serde_json::Value {
    serde_json::from_str(payload.as_json_source()).expect("generated provider body is valid JSON")
}

pub(super) fn json_payload_projection(
    payload: &lingxi_llm_client::protocol::JsonPayload,
) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
    lingxi_core::types::utf16_json::Utf16JsonProjection::parse(payload.as_json_source())
        .expect("generated provider body is valid UTF-16 JSON")
}

pub(super) fn fragment_is_text(
    fragment: &lingxi_llm_client::protocol::JsonPayloadFragment,
    expected: &str,
) -> bool {
    fragment
        .utf16_code_units()
        .iter()
        .copied()
        .eq(expected.encode_utf16())
}

mod anthropic_codec_test;
mod anthropic_usage_test;
mod cited_text_replay_test;
mod error_decode_test;
mod gemini_codec_test;
mod gemini_files_fixture;
mod gemini_files_test;
mod openai_codec_test;
mod openai_responses_codec_test;
mod protocol_projection_test;
mod provider_fixture_smoke_test;
mod request_intent_test;
