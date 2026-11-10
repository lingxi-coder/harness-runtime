//! Lower Native's unconditional object properties before the SDK's JSON merge.
//!
//! JavaScript keeps an undefined property's insertion position if a later
//! spread assigns it. JSON cannot represent that placeholder. Only materialize
//! it when an actual extra-body value will survive the spread.
use lingxi_llm_client::protocol::{JsonPayload, LlmError};
use lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind;

pub(crate) fn reserve_native_request_properties(
    body: &mut JsonPayload,
    extra: &JsonPayload,
    kind: AnthropicRequestKind,
) -> Result<(), LlmError> {
    let keys: &[&str] = match kind {
        AnthropicRequestKind::Main => &[
            "model",
            "messages",
            "system",
            "tool_choice",
            "metadata",
            "max_tokens",
            "thinking",
        ],
        AnthropicRequestKind::SideQuery => {
            &["model", "max_tokens", "system", "messages", "metadata"]
        }
        AnthropicRequestKind::HookPrompt => return Ok(()),
    };
    for key in keys {
        let pointer = format!("/{key}");
        if body.raw_source_at(&pointer).is_none() {
            if let Some(value) = extra.payload_at(&pointer) {
                *body = body.with_object_field(key, &value)?;
            }
        }
    }
    Ok(())
}
