use lingxi_llm_client::protocol::JsonPayload;

/// Parse a generated payload for semantic assertions. Keep byte-preservation
/// checks on `as_json_source()` so they do not pass through Value formatting.
pub fn value(payload: &JsonPayload) -> serde_json::Value {
    serde_json::from_str(payload.as_json_source()).expect("provider payload is valid JSON")
}
