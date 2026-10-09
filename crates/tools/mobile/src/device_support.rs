//! Shared parsing, permission and result handling for native device tools.

use permission::result::{PermissionMetadata, PermissionPrompt};
use permission::{PermissionDecisionReason, PermissionResult};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use std::future::Future;
use tool_api::context::ToolUseContext;
use tool_api::tool_trait::{ToolCallResult, ToolError, ValidationError};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EmptyInput {}

pub(crate) fn parse<T: DeserializeOwned>(input: &Value) -> Result<T, ValidationError> {
    if !input.is_object() {
        return Err(ValidationError("tool input must be a JSON object".into()));
    }
    serde_json::from_value(input.clone()).map_err(|error| ValidationError(error.to_string()))
}

pub(crate) fn invalid(error: ValidationError) -> ToolError {
    ToolError::InvalidInput(error.0)
}

pub(crate) fn allow(message: &str) -> PermissionResult {
    PermissionResult::Allow {
        reason: PermissionDecisionReason::Other {
            reason: message.into(),
        },
        updated_input: None,
        update_destination: None,
        metadata: PermissionMetadata::default(),
    }
}

pub(crate) fn ask(name: &str, message: &str) -> PermissionResult {
    PermissionResult::Ask {
        reason: PermissionDecisionReason::Other {
            reason: message.into(),
        },
        prompt: PermissionPrompt {
            title: name.into(),
            message: message.into(),
            options: vec![],
        },
        pending_classifier_check: None,
        metadata: PermissionMetadata::default(),
    }
}

pub(crate) fn failure(code: &str, message: impl ToString) -> ToolCallResult {
    let mut result = ToolCallResult::from_data(json!({
        "error": { "code": code, "message": message.to_string() }
    }));
    result.is_error = true;
    result
}

pub(crate) fn bound_text(text: &mut String, max_chars: usize) -> bool {
    if let Some((byte, _)) = text.char_indices().nth(max_chars) {
        text.truncate(byte);
        true
    } else {
        false
    }
}

/// Cap serialized model output as well as row counts. JSON escaping is counted
/// so long native strings cannot bypass the budget through expansion.
pub(crate) fn bounded_records(
    key: &str,
    records: impl IntoIterator<Item = Value>,
    max_bytes: usize,
    mut truncated: bool,
) -> ToolCallResult {
    let mut kept = Vec::new();
    let mut remaining = max_bytes.saturating_sub(64);
    for record in records {
        let size = record.to_string().len() + 1;
        if size > remaining {
            truncated = true;
            break;
        }
        remaining -= size;
        kept.push(record);
    }
    let mut data = json!({ "truncated": truncated });
    data[key] = Value::Array(kept);
    ToolCallResult::from_data(data)
}

/// Check before polling the native future so an already-cancelled call cannot
/// perform a native side effect. Cancellation drops our wait; these one-shot
/// host traits have no native cancellation API and may finish on the device.
pub(crate) async fn execute<T>(
    context: &ToolUseContext,
    future: impl Future<Output = T>,
) -> Result<T, ToolError> {
    if let Some(cancel) = &context.cancel {
        if cancel.is_cancelled() {
            return Err(ToolError::Aborted);
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(ToolError::Aborted),
            result = future => Ok(result),
        }
    } else {
        Ok(future.await)
    }
}
