//! MCP builtin tools — generic dispatcher (`MCPTool`), auth inspector
//! (`McpAuthTool`), resources listing (`ListMcpResourcesTool`), and
//! resource read (`ReadMcpResourceTool`).
//!
//! All four wrap `mcp::McpClient` (M2-02b) via an
//! `Arc<McpRegistry>` injected through [`tool_api::BuiltinToolContext`].
//!
//! Wire identifiers locked in spec §7 lines 685-700 and reproduced
//! byte-for-byte in `parity/fixtures/mcp_lsp_tools.json`.
//!
//! no-truncation: MCPTool forwards the upstream server response verbatim
//! (the server itself is the trust boundary); McpAuthTool returns
//! `{ authenticated: bool }`; ListMcpResourcesTool returns a bounded
//! resource list; ReadMcpResourceTool exposes server-controlled payloads.
//! Variable-length user content (e.g. tool dispatch results from MCP) is
//! bounded upstream by mcp::McpClient response handling.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use mcp::registry::McpRegistry;
use mcp::McpClientError;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{McpToolMaxPermission, PermissionDecisionReason, PermissionResult};
use platform_api::{McpPermissionCeiling, McpTransportSpec};
use serde_json::{json, Value};
use telemetry::pii::Verified;
use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use telemetry::tengu::mcp::{self as tengu_mcp, LargeResultHandledPayload};
use telemetry::tengu::tool::{
    LIST_MCP_RESOURCES_COMPLETED, LIST_MCP_RESOURCES_FAILED, LIST_MCP_RESOURCES_STARTED,
    MCP_AUTH_COMPLETED, MCP_AUTH_FAILED, MCP_AUTH_STARTED, MCP_COMPLETED, MCP_FAILED, MCP_STARTED,
    MCP_TOOL_AUTO_BACKGROUNDED, READ_MCP_RESOURCE_COMPLETED, READ_MCP_RESOURCE_FAILED,
    READ_MCP_RESOURCE_STARTED,
};
use telemetry::AnalyticsBus;

use tool_api::context::ToolUseContext;
use tool_api::progress::{ToolProgress, ToolProgressSender};
use tool_api::tool_trait::{
    DescriptionOptions, InterruptBehavior, PromptOptions, Tool, ToolCallResult, ToolError,
    ToolStaticContext,
};

// -- Wire identifier locks (spec §7 lines 685-700) ---------------------------

/// Tool name for the generic MCP dispatcher.
pub const MCP_TOOL_NAME: &str = "MCP";

/// Tool name for `McpAuthTool`.
pub const MCP_AUTH_TOOL_NAME: &str = "McpAuth";

/// Tool name for `ListMcpResourcesTool`. Wire name carries the `Tool` suffix
/// (claude-code `ListMcpResourcesTool/prompt.ts:1`
/// `LIST_MCP_RESOURCES_TOOL_NAME = 'ListMcpResourcesTool'`).
pub const LIST_MCP_RESOURCES_TOOL_NAME: &str = "ListMcpResourcesTool";

/// Tool name for `ReadMcpResourceTool`. Wire name carries the `Tool` suffix
/// (claude-code `ReadMcpResourceTool.ts:60` `name: 'ReadMcpResourceTool'`).
pub const READ_MCP_RESOURCE_TOOL_NAME: &str = "ReadMcpResourceTool";

/// Full-name prefix (`mcp__`) — M2-02b lock.
pub const MCP_TOOL_FULL_NAME_PREFIX: &str = "mcp__";

/// Separator between `server` and `tool` segments in the full name.
pub const MCP_TOOL_FULL_NAME_SEPARATOR: &str = "__";

// -- Helpers -----------------------------------------------------------------

/// Split a `mcp__<server>__<tool>` full-name into `(server, tool)`.
///
/// Splits on the FIRST `__` AFTER the `mcp__` prefix, so tool names may
/// themselves contain `__` substrings (e.g. `mcp__fs__read_file`).
pub(crate) fn parse_full_name(full_name: &str) -> Result<(&str, &str), ToolError> {
    let rest = full_name
        .strip_prefix(MCP_TOOL_FULL_NAME_PREFIX)
        .ok_or_else(|| {
            ToolError::InvalidInput(format!(
                "MCPTool: full_name {full_name:?} does not match mcp__<server>__<tool> pattern"
            ))
        })?;
    let idx = rest.find(MCP_TOOL_FULL_NAME_SEPARATOR).ok_or_else(|| {
        ToolError::InvalidInput(format!(
            "MCPTool: full_name {full_name:?} does not match mcp__<server>__<tool> pattern"
        ))
    })?;
    let server = &rest[..idx];
    let tool = &rest[idx + MCP_TOOL_FULL_NAME_SEPARATOR.len()..];
    if server.is_empty() {
        return Err(ToolError::InvalidInput(format!(
            "MCPTool: full_name {full_name:?} has empty server segment"
        )));
    }
    if tool.is_empty() {
        return Err(ToolError::InvalidInput(format!(
            "MCPTool: full_name {full_name:?} has empty tool segment"
        )));
    }
    Ok((server, tool))
}

/// Claude Code 2.1.246 accepts non-object MCP arguments when the server's
/// input schema is the literal empty schema `{}`. Model providers can surface
/// those arguments as a JSON-encoded string; decode that one transport artifact
/// before forwarding the value to the MCP server. A non-empty schema retains
/// the provider value verbatim, and malformed JSON remains a string.
fn normalize_empty_schema_arguments(schema: Option<&Value>, input: Value) -> Value {
    let has_empty_schema = matches!(schema, Some(Value::Object(object)) if object.is_empty());
    if !has_empty_schema {
        return input;
    }

    match input {
        Value::String(raw) => serde_json::from_str(&raw).unwrap_or_else(|_| Value::String(raw)),
        other => other,
    }
}

/// Assemble the optional `mcp_meta` passthrough for a surfaced tool result.
///
/// Mirrors claude-code (`services/mcp/client.ts:1897-1909`): the surfaced
/// result carries a `mcpMeta` object ONLY when the server returned at least
/// one of `_meta` / `structuredContent`, and that object contains ONLY the
/// keys that were present (no empty-object, no `null` placeholders). The JSON
/// values are forwarded byte-for-byte with no transformation.
fn build_mcp_meta(meta: Option<Value>, structured_content: Option<Value>) -> Option<Value> {
    if meta.is_none() && structured_content.is_none() {
        return None;
    }
    let mut obj = serde_json::Map::new();
    if let Some(m) = meta {
        obj.insert("_meta".to_string(), m);
    }
    if let Some(sc) = structured_content {
        obj.insert("structuredContent".to_string(), sc);
    }
    Some(Value::Object(obj))
}

const MAX_OUTPUT_SCHEMA_ERRORS: usize = 8;

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn push_schema_error(errors: &mut Vec<String>, message: String) {
    if errors.len() < MAX_OUTPUT_SCHEMA_ERRORS {
        errors.push(message);
    }
}

fn value_matches_type(value: &Value, expected: &str) -> bool {
    match expected {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "number" => value.as_f64().is_some(),
        "string" => value.is_string(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => true,
    }
}

fn validate_schema_value(schema: &Value, value: &Value, path: &str, errors: &mut Vec<String>) {
    let Some(object) = schema.as_object() else {
        return;
    };

    if let Some(expected) = object.get("const") {
        if value != expected {
            push_schema_error(errors, format!("at '{path}': expected const {expected}"));
            return;
        }
    }
    if let Some(enum_values) = object.get("enum").and_then(Value::as_array) {
        if !enum_values.iter().any(|candidate| candidate == value) {
            push_schema_error(errors, format!("at '{path}': value is not in enum"));
        }
    }
    if let Some(type_value) = object.get("type") {
        let matches = match type_value {
            Value::String(expected) => value_matches_type(value, expected),
            Value::Array(items) => items
                .iter()
                .filter_map(Value::as_str)
                .any(|expected| value_matches_type(value, expected)),
            _ => true,
        };
        if !matches {
            push_schema_error(
                errors,
                format!(
                    "at '{path}': expected {type_value}, got {}",
                    json_type_name(value)
                ),
            );
            return;
        }
    }
    if let Some(min) = object.get("minimum").and_then(Value::as_f64) {
        if value.as_f64().is_some_and(|actual| actual < min) {
            push_schema_error(errors, format!("at '{path}': value is below minimum {min}"));
        }
    }
    if let Some(max) = object.get("maximum").and_then(Value::as_f64) {
        if value.as_f64().is_some_and(|actual| actual > max) {
            push_schema_error(errors, format!("at '{path}': value exceeds maximum {max}"));
        }
    }
    if let Some(min) = object.get("minLength").and_then(Value::as_u64) {
        if value
            .as_str()
            .is_some_and(|actual| actual.chars().count() < min as usize)
        {
            push_schema_error(
                errors,
                format!("at '{path}': string shorter than minLength {min}"),
            );
        }
    }
    if let Some(max) = object.get("maxLength").and_then(Value::as_u64) {
        if value
            .as_str()
            .is_some_and(|actual| actual.chars().count() > max as usize)
        {
            push_schema_error(
                errors,
                format!("at '{path}': string exceeds maxLength {max}"),
            );
        }
    }
    if let Some(min) = object.get("minItems").and_then(Value::as_u64) {
        if value
            .as_array()
            .is_some_and(|actual| actual.len() < min as usize)
        {
            push_schema_error(
                errors,
                format!("at '{path}': array shorter than minItems {min}"),
            );
        }
    }
    if let Some(max) = object.get("maxItems").and_then(Value::as_u64) {
        if value
            .as_array()
            .is_some_and(|actual| actual.len() > max as usize)
        {
            push_schema_error(errors, format!("at '{path}': array exceeds maxItems {max}"));
        }
    }
    if let Some(items_schema) = object.get("items") {
        if let Some(items) = value.as_array() {
            for (index, item) in items.iter().enumerate() {
                validate_schema_value(items_schema, item, &format!("{path}/{index}"), errors);
            }
        }
    }
    let properties = object.get("properties").and_then(Value::as_object);
    // `required` is valid even when `properties` is omitted (for example a
    // schema that only constrains a closed object or uses additional
    // properties).  Keep this check independent of the properties map.
    if let Some(required) = object.get("required").and_then(Value::as_array) {
        for key in required.iter().filter_map(Value::as_str) {
            if !value.as_object().is_some_and(|map| map.contains_key(key)) {
                push_schema_error(
                    errors,
                    format!("at '{path}': missing required property {key:?}"),
                );
            }
        }
    }
    if let Some(map) = value.as_object() {
        let closed = object.get("additionalProperties").and_then(Value::as_bool) == Some(false);
        let additional_schema = object
            .get("additionalProperties")
            .filter(|schema| schema.is_object());
        for (key, item) in map {
            if properties.is_none_or(|known| !known.contains_key(key)) {
                if closed {
                    push_schema_error(errors, format!("at '{path}': unexpected property {key:?}"));
                } else if let Some(schema) = additional_schema {
                    validate_schema_value(schema, item, &format!("{path}/{key}"), errors);
                }
            }
        }
        if let Some(properties) = properties {
            for (key, schema) in properties {
                if let Some(item) = map.get(key) {
                    validate_schema_value(schema, item, &format!("{path}/{key}"), errors);
                }
            }
        }
    }
    for key in ["allOf", "anyOf", "oneOf"] {
        let Some(variants) = object.get(key).and_then(Value::as_array) else {
            continue;
        };
        let mut matched = 0usize;
        for variant in variants {
            let mut scratch = Vec::new();
            validate_schema_value(variant, value, path, &mut scratch);
            if scratch.is_empty() {
                matched += 1;
            }
        }
        match key {
            "allOf" if matched != variants.len() => {
                push_schema_error(errors, format!("at '{path}': allOf branch mismatch"));
            }
            "anyOf" if matched == 0 => {
                push_schema_error(errors, format!("at '{path}': anyOf branch mismatch"));
            }
            "oneOf" if matched != 1 => {
                push_schema_error(errors, format!("at '{path}': oneOf branch mismatch"));
            }
            _ => {}
        }
    }
}

fn validate_structured_content_against_output_schema(
    schema: &Value,
    structured_content: Option<&Value>,
) -> Result<(), String> {
    let Some(value) = structured_content else {
        return Err("missing structuredContent for outputSchema-bound MCP result".into());
    };
    let mut errors = Vec::new();
    validate_schema_value(schema, value, "(root)", &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

#[derive(Debug, PartialEq, Eq)]
struct MissingRequiredInputPreflight {
    missing_keys: Vec<String>,
    required_count: u64,
    missing_count: u64,
    present_key_count: u64,
    max_string_value_len: u64,
    has_pseudo_tag_debris: bool,
}

fn inspect_missing_required_input(
    input: &Value,
    schema: Option<&Value>,
) -> Option<MissingRequiredInputPreflight> {
    let Value::Object(map) = input else {
        return None;
    };
    let schema = schema?.as_object()?;
    let required = schema.get("required")?.as_array()?;
    let required_keys: Vec<String> = required
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    if required_keys.is_empty() {
        return None;
    }

    let missing_keys: Vec<String> = required_keys
        .iter()
        .filter(|key| !map.contains_key(key.as_str()))
        .cloned()
        .collect();
    if missing_keys.is_empty() {
        return None;
    }

    let suspicious_closing_tags: Vec<String> = schema
        .get("properties")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|properties| properties.keys())
        .chain(required_keys.iter())
        .filter(|key| js_string_len(key) >= 4)
        .map(|key| format!("</{}>", key.to_lowercase()))
        .collect();

    let mut max_string_value_len = 0_u64;
    let mut has_pseudo_tag_debris = false;
    for value in map.values() {
        let Some(text) = value.as_str() else {
            continue;
        };
        max_string_value_len = max_string_value_len.max(js_string_len(text));
        if has_pseudo_tag_debris {
            continue;
        }
        let lower = text.to_lowercase();
        if lower.contains("</parameter>")
            || lower.contains("<parameter name=")
            || lower.contains("</invoke>")
            || suspicious_closing_tags
                .iter()
                .any(|needle| lower.contains(needle))
        {
            has_pseudo_tag_debris = true;
        }
    }

    Some(MissingRequiredInputPreflight {
        required_count: required_keys.len() as u64,
        missing_count: missing_keys.len() as u64,
        present_key_count: map.len() as u64,
        max_string_value_len,
        has_pseudo_tag_debris,
        missing_keys,
    })
}

/// Build the model-facing `mcp_progress` / `progress` event payload for one
/// forwarded MCP `notifications/progress` (MCP.4). Mirrors
/// `services/mcp/client.ts:3104-3112`:
/// `{ type:'mcp_progress', status:'progress', serverName, toolName, progress,
/// total?, progressMessage? }` — the optional `total` / `progressMessage` keys
/// are omitted when the server left them absent.
fn mcp_progress_event_data(server: &str, tool: &str, ev: &mcp::client::McpProgressEvent) -> Value {
    let mut data = serde_json::Map::new();
    data.insert("type".into(), json!("mcp_progress"));
    data.insert("status".into(), json!("progress"));
    data.insert("serverName".into(), json!(server));
    data.insert("toolName".into(), json!(tool));
    data.insert("progress".into(), json!(ev.progress));
    if let Some(total) = ev.total {
        data.insert("total".into(), json!(total));
    }
    if let Some(message) = &ev.message {
        data.insert("progressMessage".into(), json!(message));
    }
    Value::Object(data)
}

/// Lift the first content block's `text` out of an MCP result, mirroring the TS
/// `result.content[0].text` lookup on an `isError: true` result
/// (`services/mcp/client.ts:3124-3142`). Returns `None` unless `content` is a
/// non-empty array whose first element is an object carrying a string `text`
/// (the caller falls back to `"Unknown error"`).
fn first_content_block_text(content: &Value) -> Option<String> {
    content
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|block| block.get("text"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Render an MCP result `content` value to a model-facing string IFF it is a
/// non-empty array of TEXT blocks only (`[{type:"text", text:…}, …]`), joining
/// the text by newline. Returns `None` for a bare-string `content` (already
/// model-faithful via the `content` key) or an array containing any non-text
/// block (image / resource — carrying those faithfully needs the block-array
/// protocol change). A single text block (the common MCP result) renders to
/// exactly its text, matching claude-code's single-block tool_result content.
fn mcp_all_text_content_to_string(content: &Value) -> Option<String> {
    let arr = content.as_array()?;
    if arr.is_empty() {
        return None;
    }
    let mut parts = Vec::with_capacity(arr.len());
    for block in arr {
        let obj = block.as_object()?;
        if obj.get("type").and_then(Value::as_str) != Some("text") {
            return None;
        }
        parts.push(obj.get("text").and_then(Value::as_str)?.to_string());
    }
    Some(parts.join("\n"))
}

/// Inspect an [`McpTransportSpec`] and return `(transport_kind, auth_kind)`.
///
/// `transport_kind`: lowercase discriminator (`"stdio"`, `"sse"`, etc.).
/// `auth_kind`: `"oauth"` / `"headers_helper"` / `"static_headers"` / `"none"`.
pub(crate) fn auth_kind_from_spec(spec: &McpTransportSpec) -> (&'static str, &'static str) {
    match spec {
        McpTransportSpec::Stdio { .. } => ("stdio", "none"),
        McpTransportSpec::Sse { oauth: Some(_), .. } => ("sse", "oauth"),
        McpTransportSpec::Sse {
            headers_helper: Some(_),
            ..
        } => ("sse", "headers_helper"),
        McpTransportSpec::Sse { headers, .. } if !headers.is_empty() => ("sse", "static_headers"),
        McpTransportSpec::Sse { .. } => ("sse", "none"),
        McpTransportSpec::Http { oauth: Some(_), .. } => ("http", "oauth"),
        McpTransportSpec::Http {
            headers_helper: Some(_),
            ..
        } => ("http", "headers_helper"),
        McpTransportSpec::Http { headers, .. } if !headers.is_empty() => ("http", "static_headers"),
        McpTransportSpec::Http { .. } => ("http", "none"),
        McpTransportSpec::WebSocket {
            headers_helper: Some(_),
            ..
        } => ("websocket", "headers_helper"),
        McpTransportSpec::WebSocket { headers, .. } if !headers.is_empty() => {
            ("websocket", "static_headers")
        }
        McpTransportSpec::WebSocket { .. } => ("websocket", "none"),
        McpTransportSpec::InProcess { .. } => ("inProcess", "none"),
        McpTransportSpec::SseIde { .. } => ("sseIde", "none"),
        McpTransportSpec::WsIde { .. } => ("wsIde", "none"),
        McpTransportSpec::SdkControl { .. } => ("sdkControl", "none"),
    }
}

fn pii(s: &str) -> AnalyticsValue {
    use telemetry::pii::PiiTagged;
    AnalyticsValue::String(PiiTagged::assert_pii_tagged_column(s.to_string()).into_inner())
}
fn verified_int(n: u64) -> AnalyticsValue {
    AnalyticsValue::Int(n as i64)
}
fn verified_str(s: &str) -> AnalyticsValue {
    use telemetry::pii::Verified;
    AnalyticsValue::String(Verified::assert_safe(s.to_string()).into_inner())
}

/// JavaScript `String.prototype.length`: count UTF-16 code units, not UTF-8
/// bytes or Unicode scalar values.
fn js_string_len(s: &str) -> u64 {
    u64::try_from(s.encode_utf16().count()).unwrap_or(u64::MAX)
}

async fn emit(bus: &Arc<AnalyticsBus>, event: &'static str, fields: &[(&str, AnalyticsValue)]) {
    let mut md: LogEventMetadata = HashMap::new();
    for (k, v) in fields {
        md.insert((*k).into(), v.clone());
    }
    bus.log_event(event, md).await;
}

async fn emit_mcp_input_missing_required(
    bus: &Arc<AnalyticsBus>,
    tool_use_id: Option<&str>,
    message_id: &protocol::MessageId,
    tool_input_size_bytes: u64,
    preflight: &MissingRequiredInputPreflight,
) {
    let mut md: LogEventMetadata = HashMap::new();
    // Oracle `Un(e.name)` intentionally collapses every `mcp__*` tool name to
    // the low-cardinality literal `mcp_tool`. Raw server/tool names are not
    // fields on this event.
    md.insert("toolName".into(), verified_str("mcp_tool"));
    md.insert("isMcp".into(), AnalyticsValue::Bool(true));
    md.insert(
        "toolInputSizeBytes".into(),
        verified_int(tool_input_size_bytes),
    );
    md.insert(
        "requiredCount".into(),
        verified_int(preflight.required_count),
    );
    md.insert("missingCount".into(), verified_int(preflight.missing_count));
    md.insert(
        "presentKeyCount".into(),
        verified_int(preflight.present_key_count),
    );
    md.insert(
        "maxStringValueLen".into(),
        verified_int(preflight.max_string_value_len),
    );
    md.insert(
        "hasPseudoTagDebris".into(),
        AnalyticsValue::Bool(preflight.has_pseudo_tag_debris),
    );
    if let Some(tool_use_id) = tool_use_id {
        md.insert("toolUseID".into(), verified_str(tool_use_id));
    }
    md.insert(
        "messageID".into(),
        verified_str(&message_id.as_uuid().to_string()),
    );
    bus.log_event(tengu_mcp::INPUT_MISSING_REQUIRED, md).await;
}

async fn emit_mcp_large_result_handled(
    bus: &Arc<AnalyticsBus>,
    handled: LargeResultHandledPayload,
) {
    let mut md: LogEventMetadata = HashMap::new();
    md.insert(
        "outcome".into(),
        AnalyticsValue::String(handled.outcome.as_str().to_string()),
    );
    md.insert(
        "reason".into(),
        AnalyticsValue::String(handled.reason.as_str().to_string()),
    );
    md.insert(
        "sizeEstimateTokens".into(),
        verified_int(handled.size_estimate_tokens),
    );
    if let Some(persisted_size_chars) = handled.persisted_size_chars {
        md.insert(
            "persistedSizeChars".into(),
            verified_int(persisted_size_chars),
        );
    }
    if let Some(result_type) = handled.result_type.as_ref() {
        md.insert(
            "resultType".into(),
            AnalyticsValue::String(result_type.as_str().to_string()),
        );
    }
    if let Some(block_count) = handled.block_count {
        md.insert("blockCount".into(), verified_int(u64::from(block_count)));
    }
    if let Some(persisted_as) = handled.persisted_as.as_ref() {
        md.insert(
            "persistedAs".into(),
            AnalyticsValue::String(persisted_as.as_str().to_string()),
        );
    }
    bus.log_event(tengu_mcp::LARGE_RESULT_HANDLED, md).await;
}

// `tengu_feature_ok` / `tengu_feature_sad` / `tengu_feature_bad` — the generic
// feature success / soft-error / hard-error counters the binary's `ve`/`Ue`/`me`
// telemetry helpers emit:
//   `ve(e)   = M("tengu_feature_ok",  {feature_name: e})`
//   `Ue(e,t) = M("tengu_feature_sad", {feature_name: e, error_code: t})`
//   `me(e,t) = M("tengu_feature_bad", {feature_name: e, error_code: t})`
const TENGU_FEATURE_OK: &str = "tengu_feature_ok";
const TENGU_FEATURE_SAD: &str = "tengu_feature_sad";
const TENGU_FEATURE_BAD: &str = "tengu_feature_bad";

/// `feature_name` value for the auto-background outcome counter — the bare
/// `"mcp_auto_background"` string the binary's settle callback `E` passes to
/// `ve`/`Ue`/`me`. Distinct from BOTH the `tengu_mcp_auto_background` feature
/// flag (`getMcpAutoBackgroundMs`) AND the `tengu_mcp_tool_auto_backgrounded`
/// background-TIME event already emitted at `M("tengu_mcp_tool_auto_backgrounded",{})`.
const MCP_AUTO_BACKGROUND_FEATURE: &str = "mcp_auto_background";

/// Terminal outcome of an auto-backgrounded MCP call, mirroring the discriminant
/// the binary's settle callback `E` feeds into `ve`/`Ue`/`me`
/// (`p.then(completed, err => E("failed", …, BZu(err) ? "tool_error" : "call_failed"))`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AutoBackgroundOutcome {
    /// The detached call resolved — `ve("mcp_auto_background")`.
    Completed,
    /// The call produced a recognized MCP failure (an `isError` tool result, a
    /// timeout, or an rpc/protocol error — every `process_mcp_call_result` `Err`
    /// falls in `BZu`'s tool-error set) — `Ue("mcp_auto_background","tool_error")`.
    ToolError,
    /// The call failed unexpectedly (the settle-waiter's spawned task panicked —
    /// `BZu` false fallback) — `me("mcp_auto_background","call_failed")`.
    CallFailed,
}

/// Emit the `mcp_auto_background` outcome counter for a settled auto-backgrounded
/// call — the binary's `E` callback firing `ve`/`Ue`/`me` (gated on the terminal
/// transition actually happening; see [`settle_mcp_task`]'s `Ok(true)`).
async fn emit_auto_background_outcome(bus: &Arc<AnalyticsBus>, outcome: AutoBackgroundOutcome) {
    let feature = verified_str(MCP_AUTO_BACKGROUND_FEATURE);
    match outcome {
        AutoBackgroundOutcome::Completed => {
            emit(bus, TENGU_FEATURE_OK, &[("feature_name", feature)]).await;
        }
        AutoBackgroundOutcome::ToolError => {
            emit(
                bus,
                TENGU_FEATURE_SAD,
                &[
                    ("feature_name", feature),
                    ("error_code", verified_str("tool_error")),
                ],
            )
            .await;
        }
        AutoBackgroundOutcome::CallFailed => {
            emit(
                bus,
                TENGU_FEATURE_BAD,
                &[
                    ("feature_name", feature),
                    ("error_code", verified_str("call_failed")),
                ],
            )
            .await;
        }
    }
}

/// Produce the `(now_millis, rand_tag)` seed for a blob `persistId`, mirroring
/// the TS `Date.now()` + `Math.random().toString(36).slice(2, 8)` pair that
/// `persistBlobToTextBlock` feeds into its persistId template
/// (`client.ts:2604`). The exact value is non-load-bearing (it only has to be
/// unique per blob); only the *template shape* (`mcp-<server>-blob-<now>-<rand>`)
/// is locked, and that lives in `transform_result::persist_blob_to_text_block`.
/// Mirrors the private `mcp::client::persist_id_seed` (kept in sync; no RNG dep).
fn persist_id_seed() -> (u128, String) {
    const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let now_millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()))
        .unwrap_or(0)
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    let mut tag = String::with_capacity(6);
    for _ in 0..6 {
        tag.push(ALPHABET[(seed % 36) as usize] as char);
        seed /= 36;
    }
    (now_millis, tag)
}

// -- Permission shape (shared by all four MCP tools) -------------------------

fn allow_mcp(reason: &str) -> PermissionResult {
    PermissionResult::Allow {
        reason: PermissionDecisionReason::Other {
            reason: reason.into(),
        },
        updated_input: None,
        update_destination: None,
        metadata: PermissionMetadata::default(),
    }
}

fn deny_mcp(reason: &str) -> PermissionResult {
    PermissionResult::Deny {
        reason: PermissionDecisionReason::Other {
            reason: reason.into(),
        },
        explanation: Some(reason.into()),
        metadata: PermissionMetadata::default(),
    }
}

// -- Tool structs ------------------------------------------------------------

/// MCP tool — forwards to `McpClient::call_tool`.
///
/// Two shapes share this one struct:
/// - **Generic dispatcher** (the legacy [`MCPTool::new`] constructor): its
///   wire `name()` is the constant [`MCP_TOOL_NAME`] (`"MCP"`) and its
///   `input_schema()` is the generic `{full_name, arguments}` envelope; the
///   model addresses an MCP tool by passing `full_name` in the payload.
/// - **Per-tool wire entry** (the [`MCPTool::new_for_tool`] constructor,
///   Batch 3): one instance per discovered server tool. Its `name()` is the
///   real `mcp__<server>__<tool>` FQN, its `input_schema()` is the server's
///   own `inputSchema`, and its `description()`/`prompt()` is the server's
///   (truncated) description. The model addresses it BY NAME and supplies the
///   raw tool arguments as the tool-use `input` directly — there is no
///   `{full_name, arguments}` envelope. Mirrors claude-code's
///   `fetchToolsForClient` building one `Tool` per server tool with
///   `name = fullyQualifiedName`, `inputJSONSchema = tool.inputSchema`
///   (`services/mcp/client.ts:1766-1990`, description truncation `:1786-1794`).
pub struct MCPTool {
    pub(crate) ctx: tool_api::BuiltinToolContext,
    /// `Some(fqn)` for a per-tool wire entry; `None` for the generic
    /// dispatcher (whose `name()` is [`MCP_TOOL_NAME`]).
    full_name: Option<String>,
    /// The server's own `inputSchema` for a per-tool wire entry; `None`
    /// falls back to the generic `{full_name, arguments}` schema.
    bound_schema: Option<Value>,
    /// The server's (truncated) description for a per-tool wire entry; `None`
    /// falls back to the generic dispatcher blurb.
    bound_desc: Option<String>,
    /// Optional per-tool structured output schema from the MCP wire definition.
    bound_output_schema: Option<Value>,
    /// Optional server-provided search hint used by ToolSearch ranking.
    search_hint: Option<String>,
    /// `_meta.anthropic/alwaysLoad` / server-level `alwaysLoad` opt-out.
    always_load: bool,
    /// `_meta.anthropic/requiresUserInteraction` (§27b). `true` means this
    /// tool needs a fresh interaction on every call, so a persisted "always
    /// allow" grant must never be offered — see
    /// `Tool::requires_user_interaction` below.
    requires_user_interaction: bool,
    /// Tighten-only ceiling resolved from the MCP server/config policy. Kept
    /// separate from the wire DTO so discovery-cache entries remain unchanged.
    effective_max_permission: Option<McpToolMaxPermission>,
    /// §24b — explicit dispatch target for a per-SUBAGENT inline `mcpServers`
    /// entry. `None` (every existing construction site) preserves today's
    /// behaviour exactly: the server is derived from `full_name`'s parsed
    /// segment, which is also the `McpRegistry` table key. `Some(key)` means
    /// this instance was built over an agent-scoped connection registered
    /// under `key` ([`mcp::registry::McpRegistry::connect_agent_scoped`]) —
    /// dispatch (registry lookups) uses `key`, while `full_name` STAYS
    /// `mcp__<server>__<tool>` with the PLAIN server name, so the model sees
    /// the same tool name the oracle emits, `mcp__<server>__*` permission
    /// rules still match, and `oauth::server_key` (which hashes
    /// `config.name`) is untouched. Set only via
    /// [`Self::with_bound_server_key`] so every OTHER construction site's
    /// argument list is unaffected by this addition.
    bound_server_key: Option<String>,
    /// Optional server role propagated from MCP configuration metadata.
    mcp_role: Option<String>,
}

/// Inspect a configured MCP server's auth/transport surface.
pub struct McpAuthTool {
    pub(crate) ctx: tool_api::BuiltinToolContext,
}

/// List resources advertised by an MCP server.
pub struct ListMcpResourcesTool {
    pub(crate) ctx: tool_api::BuiltinToolContext,
}

/// Read a single resource by URI from an MCP server.
pub struct ReadMcpResourceTool {
    pub(crate) ctx: tool_api::BuiltinToolContext,
}

impl MCPTool {
    /// Construct the generic MCP dispatcher over the supplied context.
    ///
    /// Its wire `name()` is [`MCP_TOOL_NAME`] and the model addresses an MCP
    /// tool by passing `full_name` in the `{full_name, arguments}` payload.
    #[must_use]
    pub fn new(ctx: tool_api::BuiltinToolContext) -> Self {
        Self {
            ctx,
            full_name: None,
            bound_schema: None,
            bound_desc: None,
            bound_output_schema: None,
            search_hint: None,
            always_load: true,
            requires_user_interaction: false,
            effective_max_permission: None,
            bound_server_key: None,
            mcp_role: None,
        }
    }

    /// Construct a per-tool wire entry for ONE discovered server tool (Batch 3).
    ///
    /// `full_name` is the `mcp__<server>__<tool>` FQN (the wire `name()`),
    /// `input_schema` is the server's own `inputSchema` (the wire
    /// `input_schema()`), and `description` is the server's description
    /// (truncated to [`mcp::MAX_MCP_DESCRIPTION_LENGTH`] —
    /// `services/mcp/client.ts:1786-1794`). Invoking it dispatches the FQN +
    /// the raw `input` object to `McpClient::call_tool`. Mirrors claude-code's
    /// `fetchToolsForClient` per-tool `Tool` (`client.ts:1766-1990`).
    #[must_use]
    pub fn new_for_tool(
        ctx: tool_api::BuiltinToolContext,
        full_name: String,
        description: String,
        input_schema: Value,
        output_schema: Option<Value>,
        effective_max_permission: Option<McpPermissionCeiling>,
        search_hint: Option<String>,
        always_load: bool,
        requires_user_interaction: bool,
    ) -> Self {
        Self {
            ctx,
            full_name: Some(full_name),
            bound_schema: Some(input_schema),
            // Truncate to the TS limit (client.ts:1786-1794). The DTO is
            // already truncated on receipt (client.rs:54-55), so this is a
            // defensive no-op for in-band descriptions but keeps the per-tool
            // wire entry within the documented bound for any out-of-band source.
            bound_desc: Some(mcp::truncate_description(&description).into_owned()),
            bound_output_schema: output_schema,
            search_hint,
            always_load,
            requires_user_interaction,
            effective_max_permission: effective_max_permission.map(max_permission_from_ceiling),
            bound_server_key: None,
            mcp_role: None,
        }
    }

    /// Attach a resolved MCP permission ceiling without changing the existing
    /// constructor ABI used by shared and agent-scoped registry builders.
    #[must_use]
    pub fn with_effective_max_permission(mut self, ceiling: McpToolMaxPermission) -> Self {
        self.effective_max_permission = Some(ceiling);
        self
    }

    /// Attach a ceiling from the transport-facing MCP DTO. This conversion
    /// keeps the permission crate's rule enum independent from the traits
    /// crate while allowing a registry/client integration to pass through the
    /// resolved `allow`/`ask`/`deny` value directly.
    #[must_use]
    pub fn with_mcp_permission_ceiling(self, ceiling: platform_api::McpPermissionCeiling) -> Self {
        self.with_effective_max_permission(max_permission_from_ceiling(ceiling))
    }

    /// §24b: bind this per-tool wire entry's DISPATCH target to `key` — the
    /// [`mcp::registry::McpRegistry`] table key an agent-scoped connect
    /// registered the underlying connection under. `full_name` (the wire
    /// `name()`, already stamped by [`Self::new_for_tool`]) is left
    /// untouched, so the model-facing FQN and permission-rule matching stay
    /// on the plain server name; only registry lookups inside [`Self::call`]
    /// use `key`.
    #[must_use]
    pub fn with_bound_server_key(mut self, key: String) -> Self {
        self.bound_server_key = Some(key);
        self
    }

    /// Preserve the configured MCP routing role on a freshly built tool.
    #[must_use]
    pub fn with_mcp_role(mut self, role: Option<String>) -> Self {
        self.mcp_role = role;
        self
    }

    fn mcp_registry(&self) -> Option<&Arc<McpRegistry>> {
        self.ctx.mcp_registry.as_ref()
    }

    /// Resolve permission metadata for a generic dispatcher request. The
    /// generic `MCP` tool carries only the model-supplied FQN in its input, so
    /// it must recover the discovered DTO from the live state before allowing
    /// dispatch. Agent-scoped entries are intentionally excluded: they are
    /// reachable only through their bound per-tool entries.
    async fn generic_permission_metadata(
        &self,
        full_name: &str,
    ) -> Option<(bool, Option<McpToolMaxPermission>)> {
        let registry = self.mcp_registry()?;
        let connections = registry.connections.read().await;
        connections.iter().find_map(|(table_key, state)| {
            let (config, tools) = match state {
                mcp::McpConnectionState::Connected { config, tools, .. }
                | mcp::McpConnectionState::Cached { config, tools, .. } => (config, tools),
                _ => return None,
            };
            if table_key != &config.name {
                return None;
            }
            let dto = tools.iter().find(|dto| dto.full_name == full_name)?;
            let ceiling = configured_permission_ceiling(config, &dto.tool_name)
                .map(max_permission_from_ceiling);
            Some((dto.requires_user_interaction, ceiling))
        })
    }

    async fn generic_input_schema(&self, full_name: &str) -> Option<Value> {
        let registry = self.mcp_registry()?;
        let connections = registry.connections.read().await;
        connections.iter().find_map(|(table_key, state)| {
            let (config, tools) = match state {
                mcp::McpConnectionState::Connected { config, tools, .. }
                | mcp::McpConnectionState::Cached { config, tools, .. } => (config, tools),
                _ => return None,
            };
            if table_key != &config.name {
                return None;
            }
            tools
                .iter()
                .find(|dto| dto.full_name == full_name)
                .map(|dto| dto.input_schema.clone())
        })
    }

    fn bus(&self) -> &Arc<AnalyticsBus> {
        &self.ctx.bus
    }
}
impl McpAuthTool {
    /// Construct a new [`McpAuthTool`] over the supplied context.
    #[must_use]
    pub fn new(ctx: tool_api::BuiltinToolContext) -> Self {
        Self { ctx }
    }
}
impl ListMcpResourcesTool {
    /// Construct a new [`ListMcpResourcesTool`] over the supplied context.
    #[must_use]
    pub fn new(ctx: tool_api::BuiltinToolContext) -> Self {
        Self { ctx }
    }
}
impl ReadMcpResourceTool {
    /// Construct a new [`ReadMcpResourceTool`] over the supplied context.
    #[must_use]
    pub fn new(ctx: tool_api::BuiltinToolContext) -> Self {
        Self { ctx }
    }
}

// -- Schemas -----------------------------------------------------------------

// H4.inputSchema = c({}).passthrough(); inputJSONSchema is wire-only.
static MCP_RUNTIME_INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| json!({"type":"object"}));

static MCP_TOOL_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "properties": {
            "full_name": { "type": "string" },
            "arguments": { "type": "object" }
        },
        "required": ["full_name"]
    })
});

static MCP_AUTH_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "properties": { "server_name": { "type": "string", "minLength": 1 } },
        "required": ["server_name"]
    })
});

static LIST_MCP_RESOURCES_SCHEMA: Lazy<Value> = Lazy::new(|| {
    // `server_name` is OPTIONAL (Batch 5c): absent → list EVERY connected
    // server's resources, each tagged with its `server`. Mirrors
    // `ListMcpResourcesTool.ts:15-22` (`server: z.string().optional()`).
    json!({
        "type": "object",
        "properties": { "server_name": { "type": "string", "minLength": 1 } }
    })
});

static READ_MCP_RESOURCE_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "properties": {
            "server_name": { "type": "string", "minLength": 1 },
            "uri":         { "type": "string", "minLength": 1 }
        },
        "required": ["server_name", "uri"]
    })
});

/// Transform a settled `tools/call` result into the model-facing
/// [`ToolCallResult`] (or [`ToolError`]).
///
/// This is the whole post-`call_tool_with_progress` body extracted so it can run
/// in EITHER place: inline on the foreground path, or on a detached
/// `tokio::spawn` when the call is auto-backgrounded (G07/G08). Every ambient
/// input the body needs is an owned argument (`bus`/`cwd` cloned off the
/// `BuiltinToolContext`) so the future is `'static + Send` and can outlive the
/// turn. Byte-identical to the previous inline logic — only the `self.bus()` /
/// `self.ctx.cwd()` reads became the `bus` / `cwd` parameters.
#[allow(clippy::too_many_arguments)]
async fn process_mcp_call_result(
    bus: Arc<AnalyticsBus>,
    output_dir: std::path::PathBuf,
    token_counter: Option<Arc<dyn tool_api::McpTokenCounter>>,
    default_model: String,
    output_schema: Option<Value>,
    server: String,
    tool: String,
    tool_use_id: Option<protocol::ToolUseId>,
    progress: ToolProgressSender,
    started: Instant,
    res: Result<platform_api::McpToolResultDto, McpClientError>,
) -> Result<ToolCallResult, ToolError> {
    match res {
        Ok(dto) => {
            // MCP.1: a server-flagged error result (`isError: true`) is mapped
            // to a tool ERROR before the success path, mirroring the TS throw
            // (`client.ts:3124-3148`) which fires BEFORE COMPLETED telemetry /
            // result transform. We lift the first content block's `text` (else
            // `"Unknown error"`) into the error message and emit MCP_FAILED.
            // The orchestrator derives block-level is_error purely from Ok/Err,
            // so returning `Err` renders the model "Error: <text>" — matching
            // the TS throw — instead of silently embedding `is_error` as data.
            if dto.is_error {
                let error_details = first_content_block_text(&dto.content)
                    .unwrap_or_else(|| "Unknown error".to_string());
                emit(
                    &bus,
                    MCP_FAILED,
                    &[
                        ("_PROTO_server_name", pii(&server)),
                        ("_PROTO_tool_name", pii(&tool)),
                        ("error_kind", verified_str("tool_error")),
                    ],
                )
                .await;
                return Err(ToolError::Io(error_details));
            }
            if let Some(schema) = output_schema.as_ref() {
                if let Err(detail) = validate_structured_content_against_output_schema(
                    schema,
                    dto.structured_content.as_ref(),
                ) {
                    emit(
                        &bus,
                        MCP_FAILED,
                        &[
                            ("_PROTO_server_name", pii(&server)),
                            ("_PROTO_tool_name", pii(&tool)),
                            ("error_kind", verified_str("output_schema_mismatch")),
                        ],
                    )
                    .await;
                    return Err(ToolError::Io(format!(
                        "mcp_output_schema_mismatch: {detail}"
                    )));
                }
            }

            emit(
                &bus,
                MCP_COMPLETED,
                &[
                    ("_PROTO_server_name", pii(&server)),
                    ("_PROTO_tool_name", pii(&tool)),
                    (
                        "duration_ms",
                        verified_int(started.elapsed().as_millis() as u64),
                    ),
                    ("is_error", AnalyticsValue::Bool(dto.is_error)),
                ],
            )
            .await;

            // `completed` progress event (`client.ts:1883-1895`). Emitted
            // only on a non-error result (the TS `callMCPTool` throws on
            // `isError` before reaching this point — handled above by the
            // MCP.1 mapping).
            if let Some(tuid) = tool_use_id {
                let _ = progress.try_send(ToolProgress {
                    tool_use_id: tuid,
                    data: json!({
                        "type": "mcp_progress",
                        "status": "completed",
                        "serverName": server,
                        "toolName": tool,
                        "elapsedTimeMs": started.elapsed().as_millis() as u64,
                    }),
                });
            }

            let (now_millis, rand_tag) = persist_id_seed();
            // MCP.2: structuredContent takes PRIORITY over `content` for the
            // model (transformMCPResult, `client.ts:2675-2684`): when the
            // server returns `structuredContent` we hand the model
            // `jsonStringify(structuredContent)` (compact JSON) instead of
            // walking `content` — so a structured-only result no longer
            // surfaces as `content:null`, and a both-present result shows the
            // structured JSON. When it is absent we reshape the raw `content`
            // blocks into their model-facing form (text passthrough, image →
            // base64 image block, audio + non-image resource-blob → persisted
            // text block, resource-text prefixing, resource-image-blob →
            // prefix + image block, resource_link), mirroring
            // `transformResultContent` (`client.ts:2478-2697`). Only ARRAY
            // `content` is walked — a bare value is forwarded verbatim (MCP-5e).
            let persist_ctx = crate::transform_result::PersistContext {
                output_dir: &output_dir,
                now_millis,
                rand_tag: &rand_tag,
            };
            let (model_content, large_result_type) = match &dto.structured_content {
                Some(sc) => {
                    let sc_json = serde_json::to_string(sc).unwrap_or_default();
                    // jqd (binary @198966347): when the result ALSO carries
                    // non-`text` content blocks (images, audio, resources),
                    // those survive ALONGSIDE the structured JSON as a
                    // contentArray `[...transformed-non-text, {text:<json>}]`.
                    // Original `text` blocks are dropped (the JSON represents
                    // them). Only a structured-only result collapses to the
                    // bare JSON string. Previously the non-text blocks (e.g.
                    // images) were silently dropped.
                    let non_text: Vec<Value> = dto
                        .content
                        .as_array()
                        .map(|items| {
                            items
                                .iter()
                                .filter(|b| {
                                    b.get("type")
                                        .and_then(Value::as_str)
                                        .is_some_and(|t| t != "text")
                                })
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default();
                    if non_text.is_empty() {
                        (
                            Value::String(sc_json),
                            crate::large_output::McpLargeResultType::StructuredContent,
                        )
                    } else {
                        let transformed = crate::transform_result::transform_result_content(
                            &Value::Array(non_text),
                            &server,
                            persist_ctx,
                        );
                        let mut arr = transformed.as_array().cloned().unwrap_or_default();
                        arr.push(json!({ "type": "text", "text": sc_json }));
                        (
                            Value::Array(arr),
                            crate::large_output::McpLargeResultType::ContentArray,
                        )
                    }
                }
                None => {
                    let transformed = crate::transform_result::transform_result_content(
                        &dto.content,
                        &server,
                        persist_ctx,
                    );
                    let result_type = if transformed.is_array() {
                        crate::large_output::McpLargeResultType::ContentArray
                    } else {
                        crate::large_output::McpLargeResultType::ToolResult
                    };
                    (transformed, result_type)
                }
            };
            // MCP large-output guard (claude-code `processMCPResult`): over-
            // threshold non-image content is persisted to disk and replaced
            // with read-it-from-file instructions; images / a falsy
            // ENABLE_MCP_LARGE_OUTPUT_FILES / a failed write fall back to
            // truncation. Under-threshold content is forwarded verbatim.
            // Second stage of the oracle's `AJr` guard: once the rough estimate
            // clears the 12 500-token threshold, count the FULL content (image
            // blocks INCLUDED — the oracle's `aHt` passes the whole content to
            // the count endpoint, so the count is not skipped for image-bearing
            // results). The count route drives which fallback the large-output
            // handler applies (see `ExactCountOutcome`).
            let exact_token_count =
                if crate::large_output::mcp_content_needs_exact_count(&model_content) {
                    match match token_counter.as_ref() {
                        Some(counter) => {
                            counter
                                .count_mcp_content_tokens(&default_model, &model_content)
                                .await
                        }
                        None => Ok(None),
                    } {
                        // The active route returned an exact count.
                        Ok(Some(count)) => crate::large_output::ExactCountOutcome::Counted(count),
                        // The route has NO exact-count endpoint (a non-Anthropic
                        // multi-provider route): stay conservative (accepted
                        // divergence — the oracle is always Anthropic).
                        Ok(None) => crate::large_output::ExactCountOutcome::Unsupported,
                        // The route HAS a count endpoint (Anthropic) but the
                        // count call FAILED. Mirror the oracle's `AJr`
                        // `catch`→`false`: forward the content verbatim rather
                        // than losing a valid MCP result to a transient failure.
                        Err(_error) => crate::large_output::ExactCountOutcome::CountFailed,
                    }
                } else {
                    crate::large_output::ExactCountOutcome::Unsupported
                };
            let processed = crate::large_output::process_mcp_result_detailed(
                &model_content,
                &server,
                &tool,
                &output_dir,
                now_millis,
                exact_token_count,
                large_result_type,
            );
            if let Some(handled) = processed.telemetry {
                emit_mcp_large_result_handled(
                    &bus,
                    LargeResultHandledPayload {
                        outcome: Verified::assert_safe(handled.outcome.to_string()),
                        reason: Verified::assert_safe(handled.reason.to_string()),
                        size_estimate_tokens: handled.size_estimate_tokens,
                        persisted_size_chars: handled.persisted_size_chars,
                        result_type: handled
                            .result_type
                            .map(|value| Verified::assert_safe(value.to_string())),
                        block_count: handled
                            .block_count
                            .map(|value| u32::try_from(value).unwrap_or(u32::MAX)),
                        persisted_as: handled
                            .persisted_as
                            .map(|value| Verified::assert_safe(value.to_string())),
                    },
                )
                .await;
            }
            // 1:1 with the binary's MCPTool result `data`: claude-code sets
            // `data = mcpResult.content` DIRECTLY (the content-block ARRAY, a
            // bare string, or a large-output file replacement) — there is NO
            // `{server_name, tool_name, is_error}` wrapper. The egress sends
            // `data` VERBATIM as the `tool_result` content blocks when it is an
            // array. `dto.is_error` rides the analytics path only (the dispatch
            // sets the block's `is_error` separately — it never read this key).
            let data = processed.content;
            // Model-facing render: claude-code passes the MCP content directly
            // as the `tool_result` content (`MCPTool.ts:70-76`). The dispatch's
            // `tool_result_to_model_text` would JSON-dump an ARRAY/string `data`
            // (which `data` now IS), so hand it the joined text (all-text
            // result) or the bare string here via `ToolCallResult.model_content`
            // (a non-text array carries its structure via the egress
            // `content_blocks` instead, leaving `model_content` as `None`).
            let model_content =
                mcp_all_text_content_to_string(&data).or_else(|| data.as_str().map(str::to_string));
            Ok(ToolCallResult {
                data,
                model_content,
                new_messages: vec![],
                context_modifier: None,
                // claude-code flags the tool_result block with the MCP server's
                // `isError` (a logical-error RESULT, not a transport failure).
                // The dispatch reads this onto the block's is_error.
                is_error: dto.is_error,
                mcp_meta: build_mcp_meta(dto.meta, dto.structured_content),
            })
        }
        Err(e) => {
            let kind = match &e {
                // Both the overall (`BHs`) and idle (`GLd`) timeouts are
                // timeout-family aborts (parity 2.1.207 P2-01 remainder).
                McpClientError::Timeout { .. } | McpClientError::IdleTimeout { .. } => "timeout",
                _ => "rpc",
            };
            emit(
                &bus,
                MCP_FAILED,
                &[
                    ("_PROTO_server_name", pii(&server)),
                    ("_PROTO_tool_name", pii(&tool)),
                    ("error_kind", verified_str(kind)),
                ],
            )
            .await;
            // For Timeout, the Display string IS the wire-locked literal
            // (M2-02b lock); propagate verbatim. For other errors, wrap
            // with a clear MCPTool prefix.
            Err(match &e {
                McpClientError::Timeout { .. } | McpClientError::IdleTimeout { .. } => {
                    ToolError::Io(e.to_string())
                }
                _ => ToolError::Io(format!("MCPTool: server {server:?} rpc error: {e}")),
            })
        }
    }
}

// -- impl Tool for MCPTool ---------------------------------------------------

#[async_trait]
impl Tool for MCPTool {
    fn name(&self) -> &str {
        // Per-tool wire entry → the `mcp__<server>__<tool>` FQN; generic
        // dispatcher → the constant `MCP` (client.ts:1768 `name = fqn`).
        self.full_name.as_deref().unwrap_or(MCP_TOOL_NAME)
    }
    fn input_schema(&self) -> &Value {
        // Per-tool wire entry → the server's own `inputSchema`
        // (client.ts:1808 `inputJSONSchema = tool.inputSchema`); generic
        // dispatcher → the `{full_name, arguments}` envelope schema.
        self.bound_schema.as_ref().unwrap_or(&MCP_TOOL_SCHEMA)
    }
    fn input_validation_schema(&self) -> &Value {
        if self.full_name.is_some() {
            &MCP_RUNTIME_INPUT_SCHEMA
        } else {
            &MCP_TOOL_SCHEMA
        }
    }
    fn output_schema(&self) -> Option<&Value> {
        self.bound_output_schema.as_ref()
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn is_mcp(&self) -> bool {
        true
    }
    async fn on_input_schema_rejected(
        &self,
        input: &Value,
        tool_use_id: Option<&str>,
        assistant_message_id: Option<&protocol::MessageId>,
    ) {
        let Some(preflight) = inspect_missing_required_input(input, Some(self.input_schema()))
        else {
            return;
        };
        let Some(message_id) = assistant_message_id else {
            return;
        };
        // Oracle `b(input).length`: JSON serialization followed by JavaScript
        // UTF-16 code-unit length, despite the historical `Bytes` suffix.
        let tool_input_size_bytes = serde_json::to_string(input)
            .map(|json| js_string_len(&json))
            .unwrap_or(0);
        emit_mcp_input_missing_required(
            self.bus(),
            tool_use_id,
            message_id,
            tool_input_size_bytes,
            &preflight,
        )
        .await;
    }
    fn mcp_role(&self) -> Option<&str> {
        self.mcp_role.as_deref()
    }
    fn should_defer(&self) -> bool {
        self.full_name.is_some() && !self.always_load
    }
    fn always_load(&self) -> bool {
        self.always_load
    }
    fn search_hint(&self) -> Option<&str> {
        self.search_hint.as_deref()
    }
    /// §27b — `_meta.anthropic/requiresUserInteraction`. Oracle:
    /// `requiresUserInteraction(){return Ee}` (@182520425), where `Ee` is the
    /// same bit read from `_meta` at tool-list time. The dispatcher's
    /// consumers use this to suppress a persistent "always allow" grant for
    /// this tool (`suppressesAlwaysAllowRule:()=>Ee||Zt(x,v.name)` —
    /// @182520462; the `Zt(x,v.name)` disjunct has no port equivalent and is
    /// out of scope here).
    fn requires_user_interaction(&self) -> bool {
        self.requires_user_interaction
    }
    fn max_result_size_chars(&self) -> usize {
        30_000
    }
    /// claude-code's MCP factory (2.1.220 BIN off 232139111):
    /// ```js
    /// maxResultSizeChars: N ? Math.min(L, gor) : Gar.maxResultSizeChars,
    /// persistenceThresholdCeiling: N ? gor : void 0,
    /// ```
    /// `N` is "this server declared its own output cap", `L` is that cap,
    /// `gor = 500000`, and the generic MCP descriptor `Gar` declares
    /// `maxResultSizeChars: 1e5` (BIN off 231406237).
    ///
    /// [`MCPTool`] models no server-declared cap, so `N` is always false here
    /// and the raw value is `Gar`'s 100 000. The ceiling is left unset (see
    /// the sibling default), which selects `AKr = 50000`, so the orchestrator's
    /// fold yields an effective 50 000.
    ///
    /// NOT [`Self::max_result_size_chars`] (30 000): that is the truncation
    /// cap, a separate oracle field. Folding it in here would persist MCP
    /// output 20 000 chars earlier than claude does.
    fn persistence_threshold(&self) -> Option<usize> {
        Some(100_000)
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    fn is_open_world(&self, _: &Value) -> bool {
        true
    }
    fn interrupt_behavior(&self, _: &Value) -> InterruptBehavior {
        InterruptBehavior::Cancel
    }

    /// §27b — the MCP factory's own `checkPermissions` arm (@182520945):
    ///
    /// ```js
    /// if(Ee)return{behavior:"ask",message:"MCPTool requires permission.",
    ///              suggestions:[],suppressAlwaysAllowRule:!0};
    /// return{behavior:"passthrough",message:"MCPTool requires permission.",…}
    /// ```
    ///
    /// `Ee` is the same `_meta.anthropic/requiresUserInteraction` bit
    /// [`Self::requires_user_interaction`] reads. Without this arm the dialog
    /// change landed with §27b never engages at all: the tool is allowed
    /// outright and no prompt is ever built for the suppressed always-allow
    /// row to be missing from.
    ///
    /// `suppressAlwaysAllowRule` is not a field here — the port carries it as
    /// [`platform_api::permission_gate::PermissionCheckContext::requires_user_interaction`],
    /// which `turn_loop` fills from [`Self::requires_user_interaction`] and
    /// `TuiPermissionGate` reads to hide the persistent-grant row.
    ///
    /// KNOWN NARROWING (reported, not fixed here): the oracle raises this ask
    /// inside `Aon`, whose tool-wide ALLOW-rule arm (`W=NLe(he(r),e)`) runs
    /// AFTER it, so a pre-existing `mcp__srv__tool` allow rule still prompts.
    /// `turn_loop` only consults a tool's `check_permissions` when no rule
    /// matched, so an allow rule still bypasses this arm in the port.
    async fn check_permissions(&self, input: &Value, _: &ToolUseContext) -> PermissionResult {
        if self.full_name.is_none() {
            let Some(full_name) = input.get("full_name").and_then(Value::as_str) else {
                return deny_mcp("MCP tool metadata unavailable; refusing dispatch");
            };
            let Some((requires_user_interaction, effective_max_permission)) =
                self.generic_permission_metadata(full_name).await
            else {
                return deny_mcp("MCP tool metadata unavailable; refusing dispatch");
            };
            return permission::clamp_mcp_permission_result(
                allow_mcp("MCP server tool dispatch"),
                full_name,
                effective_max_permission,
                None,
                requires_user_interaction,
                true,
            );
        }

        // MCP client-side ceilings and tool-owned interaction requirements are
        // local, tighten-only overlays. No app-capability signal exists in
        // this path, so capability authorization stays permissive until a
        // real host signal is available; treating absence as false would deny
        // every ordinary MCP tool.
        permission::clamp_mcp_permission_result(
            allow_mcp("MCP server tool dispatch"),
            self.name(),
            self.effective_max_permission,
            None,
            self.requires_user_interaction,
            true,
        )
    }

    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        // Per-tool wire entry → the server's (truncated) description
        // (client.ts:1786-1794); generic dispatcher → the dispatcher blurb.
        match &self.bound_desc {
            Some(d) => d.clone(),
            None => "Invoke a tool on a registered MCP server via mcp__<server>__<tool> full-name."
                .into(),
        }
    }
    async fn prompt(&self, _: &PromptOptions) -> String {
        // The system-prompt `<tools>` blurb is the same server description as
        // `description()` for a per-tool wire entry (client.ts:1786-1794).
        match &self.bound_desc {
            Some(d) => d.clone(),
            None => "Use MCP to dispatch to an MCP-server-provided tool.".into(),
        }
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolUseContext,
        progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let started = Instant::now();
        // Per-tool wire entry (Batch 3): the FQN is the bound `name()` and the
        // RAW `input` object is the tool arguments — there is NO
        // `{full_name, arguments}` envelope to unwrap (the model addressed this
        // tool by name and supplied the server schema's payload directly).
        // Mirrors claude-code's per-tool `call(args)` →
        // `callMCPToolWithUrlElicitationRetry` (client.ts:1766-1990). The
        // generic dispatcher path keeps reading `full_name`/`arguments`.
        let (full_name, arguments) = match &self.full_name {
            Some(fqn) => (
                fqn.clone(),
                normalize_empty_schema_arguments(self.bound_schema.as_ref(), input),
            ),
            None => {
                let fqn = input
                    .get("full_name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ToolError::InvalidInput("MCPTool: missing or non-string full_name".into())
                    })?
                    .to_string();
                let args = input.get("arguments").cloned().unwrap_or_else(|| json!({}));
                (fqn, args)
            }
        };

        // Parse name first; malformed names short-circuit BEFORE STARTED.
        let (server, tool) = parse_full_name(&full_name)?;
        let server = server.to_string();
        let tool = tool.to_string();
        // §24b: the registry table key to dispatch against. `full_name`
        // above stays the PLAIN `mcp__<server>__<tool>` the model addressed
        // (permission rules / telemetry / progress events below all keep
        // using `server`); only registry LOOKUPS use `dispatch_key`, which
        // is `server` itself unless this instance is bound to an
        // agent-scoped connection (`with_bound_server_key`).
        let dispatch_key: String = self
            .bound_server_key
            .clone()
            .unwrap_or_else(|| server.clone());
        // Despite the oracle field's historical `Bytes` suffix, `W` is
        // `JSON.stringify(input).length`: JavaScript UTF-16 code units.
        let tool_input_size_bytes = serde_json::to_string(&arguments)
            .map(|json| js_string_len(&json))
            .unwrap_or(0);

        let registry = match self.mcp_registry() {
            Some(r) => r,
            None => {
                emit(
                    self.bus(),
                    MCP_FAILED,
                    &[
                        ("_PROTO_server_name", pii(&server)),
                        ("error_kind", verified_str("registry_unconfigured")),
                    ],
                )
                .await;
                return Err(ToolError::Internal(
                    "MCPTool: MCP registry not configured on this host".into(),
                ));
            }
        };

        if !registry.has_callable_server(&dispatch_key).await {
            emit(
                self.bus(),
                MCP_FAILED,
                &[
                    ("_PROTO_server_name", pii(&server)),
                    ("error_kind", verified_str("server_not_registered")),
                ],
            )
            .await;
            return Err(ToolError::InvalidInput(format!(
                "MCPTool: MCP server {server:?} is not registered"
            )));
        }

        let generic_input_schema = if self.full_name.is_none() {
            self.generic_input_schema(&full_name).await
        } else {
            None
        };
        if let Some(preflight) = inspect_missing_required_input(
            &arguments,
            self.bound_schema.as_ref().or(generic_input_schema.as_ref()),
        ) {
            let tool_use_id = ctx.tool_use_id.as_ref().map(|id| id.to_string());
            // P6n is telemetry only; the MCP server owns its advertised schema.
            if let Some(message_id) = ctx.assistant_message_id.as_ref() {
                emit_mcp_input_missing_required(
                    self.bus(),
                    tool_use_id.as_deref(),
                    message_id,
                    tool_input_size_bytes,
                    &preflight,
                )
                .await;
            }
        }

        // STARTED.
        emit(
            self.bus(),
            MCP_STARTED,
            &[
                ("_PROTO_server_name", pii(&server)),
                ("_PROTO_tool_name", pii(&tool)),
            ],
        )
        .await;

        // MCP.3 + MCP.4: thread the model's toolUseId into the request and wire
        // MCP progress-notification forwarding. claude-code reads the toolUseId
        // off the parent assistant message (`extractToolUseId`,
        // `client.ts:3247-3252`); here it travels on the `ToolUseContext`. It is
        // stamped into the tools/call request `_meta` as `claudecode/toolUseId`
        // (`client.ts:1840-1843`) and gates the `started`/`progress`/`completed`
        // progress events (`onProgress && toolUseId`,
        // `client.ts:1846`/`:1871`/`:1884`).
        // MCP FQN: `full_name`'s tool segment is the NORMALIZED model-facing
        // name (1:1 with claude-code `buildMcpToolName`, which normalizes BOTH
        // the server AND the tool segment — `client.ts:1768`/`mcpStringUtils.ts:51`).
        // The server expects the RAW wire tool name, carried alongside the dto
        // like claude-code's `mcpInfo.toolName` (`client.ts:1774`). Resolve it
        // from the registry and shadow `tool`; fall back to the parsed segment
        // when the tool isn't registered (defensive — e.g. a hand-built /
        // generic-dispatcher `full_name`). For valid-identifier tool names the
        // raw and normalized forms are identical, so this is a no-op except for
        // tool names containing characters outside `[a-zA-Z0-9_-]`.
        let tool = registry
            .resolve_wire_tool_name(&server, &full_name, self.bound_server_key.as_deref())
            .await
            .unwrap_or(tool);
        // Reconstruct the raw-tool FQN so the client's `mcp__<server>__` prefix
        // strip (`call_tool_with_meta`) recovers the RAW wire name. The server
        // segment is unchanged (server normalization is identical); only the
        // tool segment may differ from the model-facing `full_name`.
        let dispatch_full_name = format!("mcp__{server}__{tool}");

        let tool_use_id = ctx.tool_use_id.clone();

        // `started` progress event (`client.ts:1845-1856`).
        if let Some(tuid) = tool_use_id.clone() {
            let _ = progress.try_send(ToolProgress {
                tool_use_id: tuid,
                data: json!({
                    "type": "mcp_progress",
                    "status": "started",
                    "serverName": server,
                    "toolName": tool,
                }),
            });
        }

        // `progress` forwarder (`client.ts:1871-1879` + `:3102-3114`): each MCP
        // `notifications/progress` becomes an `mcp_progress`/`progress` event.
        // Only wired when a toolUseId is present (the `onProgress && toolUseId`
        // gate). Sends are best-effort (`try_send`), like the synchronous TS
        // `onProgress`.
        let on_progress: Option<mcp::client::McpProgressCallback> =
            tool_use_id.clone().map(|tuid| {
                let sender = progress.clone();
                let server_name = server.clone();
                let tool_name = tool.clone();
                Arc::new(move |ev: mcp::client::McpProgressEvent| {
                    let _ = sender.try_send(ToolProgress {
                        tool_use_id: tuid.clone(),
                        data: mcp_progress_event_data(&server_name, &tool_name, &ev),
                    });
                }) as mcp::client::McpProgressCallback
            });

        // The wire form of a `ToolUseId` is its bare (serde-transparent) string.
        let tool_use_id_str = tool_use_id.as_ref().map(|tuid| tuid.to_string());

        // -- G07/G08: MCP tool-call auto-background race ---------------------
        // Resolve the transport kind for `getMcpAutoBackgroundMs` (`Wc_`) from
        // the registry's connection kind (`"stdio"`/`"sse"`/`"ws"`/`"http"`).
        // lingxi has no `"sse-ide"`/`"ws-ide"` IDE variants, so that IDE
        // exclusion is inert here but PRESERVED inside the helper. A missing
        // config falls back to an empty kind (never IDE) — the default gate.
        let transport_kind = match registry.get_config(&dispatch_key).await {
            Some(cfg) => auth_kind_from_spec(&cfg.spec).0.to_string(),
            None => String::new(),
        };
        let auto_bg_ms = crate::auto_background::get_mcp_auto_background_ms(
            &transport_kind,
            ctx.options.is_non_interactive_session,
        );

        // Owned handles for the post-call processing (bus/cwd off the shared
        // `BuiltinToolContext`), cloned so the future is `'static + Send` and
        // can be detached on the background path.
        let bus = self.ctx.bus.clone();
        let output_dir = self.ctx.tool_results_dir();
        let token_counter = self.ctx.mcp_token_counter.clone();
        let default_model = self.ctx.default_model.clone();

        // Direct-await path: auto-background disabled (`auto_bg_ms == 0`) OR no
        // task registry wired (nothing to detach into). This is the pre-G08
        // behavior — the call awaits inline with no regression.
        let bg_registry = if auto_bg_ms > 0 {
            self.ctx.task_registry.clone()
        } else {
            None
        };
        let Some(task_registry) = bg_registry else {
            let res = registry
                .call_tool_with_auth_retry(
                    &dispatch_key,
                    &dispatch_full_name,
                    arguments,
                    tool_use_id_str.as_deref(),
                    on_progress,
                )
                .await;
            return process_mcp_call_result(
                bus,
                output_dir,
                token_counter,
                default_model,
                self.bound_output_schema.clone(),
                server,
                tool,
                tool_use_id,
                progress,
                started,
                res,
            )
            .await;
        };

        // Race path (`callMcpToolWithAutoBackground`/`Gc_`): spawn the call +
        // its post-processing onto a task so it can outlive the turn, then race
        // that against the auto-background timeout.
        //
        // F3-2: the cancel token is minted BEFORE the spawn and threaded INTO
        // the call task (the oracle passes the abort signal into the call,
        // `p=e(c.signal)`), and the parent turn/abort token (`ctx.cancel`, the
        // oracle's `cUr(o,c)` link) is observed alongside it. On cancel the
        // in-flight call future is DROPPED (cancel-on-drop, restoring the
        // pre-G08 direct-await semantics) and the completed-side-effects
        // (`process_mcp_call_result` — MCP_COMPLETED telemetry, large-output
        // persistence) are SKIPPED, mirroring the oracle's `notified` guard
        // (`if(O.notified)return O`). Without this, on `TaskStop` the token
        // merely woke the settle-waiter's `select!`, dropping the JoinHandle
        // (which DETACHES, not aborts) — the RPC kept running and still emitted
        // COMPLETED telemetry / persisted result files for a killed task, and an
        // Esc during the pre-background window leaked an untracked in-flight
        // task with no cancel path at all.
        let cancel = tokio_util::sync::CancellationToken::new();
        let parent_cancel = ctx.cancel.clone();
        let bound_output_schema = self.bound_output_schema.clone();
        let task_raw_content = Arc::new(std::sync::Mutex::new(None));
        let mut call_task = {
            let task_raw_content = task_raw_content.clone();
            let registry = registry.clone();
            let bus = bus.clone();
            let output_dir = output_dir.clone();
            let token_counter = token_counter.clone();
            let default_model = default_model.clone();
            let server = server.clone();
            let call_server = dispatch_key.clone();
            let tool = tool.clone();
            let tool_use_id = tool_use_id.clone();
            let progress = progress.clone();
            let dispatch_full_name = dispatch_full_name.clone();
            let tool_use_id_str = tool_use_id_str.clone();
            let output_schema = bound_output_schema.clone();
            let cancel = cancel.clone();
            let parent_cancel = parent_cancel.clone();
            tokio::spawn(async move {
                let call_fut = registry.call_tool_with_auth_retry(
                    &call_server,
                    &dispatch_full_name,
                    arguments,
                    tool_use_id_str.as_deref(),
                    on_progress,
                );
                tokio::pin!(call_fut);
                // Fires on the mcp_task cancel token (`TaskStop`, once
                // registered) OR the parent turn/abort token (`ctx.cancel`).
                let cancelled = async {
                    match &parent_cancel {
                        Some(pc) => {
                            tokio::select! {
                                () = cancel.cancelled() => {}
                                () = pc.cancelled() => {}
                            }
                        }
                        None => cancel.cancelled().await,
                    }
                };
                // `biased` toward cancellation: a cancelled call must NOT run the
                // completed-side-effects even if it resolves at the same instant
                // (the oracle's `notified` guard suppresses post-processing).
                tokio::select! {
                    biased;
                    () = cancelled => Err(ToolError::Aborted),
                    res = &mut call_fut => {
                        if let Ok(dto) = &res {
                            if !dto.is_error {
                                *task_raw_content.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(dto.structured_content.as_ref().map_or_else(|| dto.content.clone(), |value| Value::String(value.to_string())));
                            }
                        }
                        process_mcp_call_result(
                            bus,
                            output_dir,
                            token_counter,
                            default_model,
                            output_schema,
                            server,
                            tool,
                            tool_use_id,
                            progress,
                            started,
                            res,
                        )
                        .await
                    }
                }
            })
        };

        // `while(!0){ Promise.race([f, xr(s)]); if(B?.())continue; break }`:
        // settled-first returns the result directly; timeout-first consults the
        // elicitation predicate and, while a dialog is OPEN, re-arms a whole
        // fresh window instead of detaching. `biased` polls the call before the
        // timer so a call that finishes exactly at the deadline still returns
        // inline (matching the binary's `==="settled"` check).
        //
        // MON-08: the port had a bare single-shot select here, so a `tools/call`
        // waiting on the user was backgrounded at the first deadline — turning a
        // question into an orphan. The window is 120 s by default and the port's
        // Elicitation hook has a 10-minute default timeout, so this was a real
        // window, not a theoretical one.
        //
        // NOTE the granularity: each `continue` mints a NEW full-length timer,
        // exactly as the oracle does. Polling more tightly would be a divergence
        // — a dialog closing at T=121 s backgrounds at T=240 s upstream, not at
        // T=121 s.
        loop {
            tokio::select! {
                biased;
                joined = &mut call_task => {
                    return match joined {
                        Ok(result) => result,
                        Err(join_err) => Err(ToolError::Internal(format!(
                            "MCPTool: background task join error: {join_err}"
                        ))),
                    };
                }
                () = tokio::time::sleep(std::time::Duration::from_millis(auto_bg_ms as u64)) => {}
            }
            // `B?.()` — an absent predicate is falsy, so a server with no live
            // client backgrounds exactly as before.
            let dialog_open = registry
                .get_client(server.as_str())
                .await
                .is_some_and(|client| client.has_pending_elicitation());
            if !dialog_open {
                break;
            }
        }

        // Timeout — move the still-running call to the background as an
        // `mcp_task`. `NZu`/`i.register(g)`: the pre-minted `cancel` token
        // (already threaded into the call task above) is handed to the registry
        // so `TaskStop` fires it (the port equivalent of the state's
        // `abortController`), which now genuinely cancels the in-flight call.
        let registration = platform_api::task_registry::McpTaskRegistration {
            server_name: server.clone(),
            tool_name: tool.clone(),
            tool_use_id: tool_use_id_str.clone(),
            creator_teammate_name: ctx.agent_name.clone(),
            creator_team_name: ctx.team_name.clone(),
            creator_agent_id: ctx.agent_id,
        };
        let task_id = match task_registry
            .register_mcp_task(registration, cancel.clone())
            .await
        {
            Ok(id) => id,
            // Registration unavailable (unwired / registry error) → fall back to
            // awaiting the call inline, so the turn still gets a real result.
            Err(_) => {
                return match call_task.await {
                    Ok(result) => result,
                    Err(join_err) => Err(ToolError::Internal(format!(
                        "MCPTool: background task join error: {join_err}"
                    ))),
                };
            }
        };

        // `M("tengu_mcp_tool_auto_backgrounded", {})` — empty payload.
        emit(&bus, MCP_TOOL_AUTO_BACKGROUNDED, &[]).await;

        // Detached settle waiter (`p.then(E)`): when the call finishes, write its
        // model-facing text into the `mcp_task` spool + mark it terminal, so the
        // next turn-boundary `take_pending_task_notifications` drain surfaces a
        // `<task-notification>` whose `output-file` carries the real result.
        // `TaskStop` fires `cancel`, which abandons the wait (the registry's
        // `kill` already marked the task killed, so a late settle no-ops).
        {
            let task_registry = task_registry.clone();
            let task_id = task_id.clone();
            let bus = bus.clone();
            tokio::spawn(async move {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => {}
                    joined = &mut call_task => {
                        // Outcome discriminant mirrors the binary's settle
                        // callback `E`: resolve → completed; a recognized MCP
                        // failure (`Ok(Err)` — every `process_mcp_call_result`
                        // `Err` is an `isError` tool result / timeout / rpc
                        // error, all inside `BZu`'s tool-error set) → tool_error;
                        // an unexpected task panic (`Err(join_err)`, `BZu` false)
                        // → call_failed.
                        let (text, failed, outcome) = match joined {
                            Ok(Ok(result)) => (
                                result
                                    .model_content
                                    .clone()
                                    .unwrap_or_else(|| result.data.to_string()),
                                result.is_error,
                                AutoBackgroundOutcome::Completed,
                            ),
                            Ok(Err(e)) => {
                                (e.to_string(), true, AutoBackgroundOutcome::ToolError)
                            }
                            Err(join_err) => (
                                format!("background task join error: {join_err}"),
                                true,
                                AutoBackgroundOutcome::CallFailed,
                            ),
                        };
                        // Emit the `mcp_auto_background` outcome counter ONLY when
                        // this settle won the terminal transition (the binary's
                        // `!k` guard — a killed / already-settled task never
                        // re-emits). `settle_mcp_task` reports that via `Ok(true)`.
                        let raw = task_raw_content.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
                        let receipt = if !failed {
                            raw.map(|raw| {
                                let (now, random) = persist_id_seed();
                                crate::task_result::prepare(&raw, &output_dir, &format!("{now}-{random}"))
                            })
                        } else { None };
                        let (text, hint) = receipt.map_or((text, None), |receipt| (receipt.text, receipt.saved_hint));
                        if let Ok(true) =
                            task_registry.settle_mcp_task_with_hint(&task_id, &text, failed, hint.as_deref()).await
                        {
                            emit_auto_background_outcome(&bus, outcome).await;
                        }
                    }
                }
            });
        }

        // Return the byte-exact background message to the model
        // (`{data:[{type:"text",text:…}]}`). Elapsed is `Math.round`ed seconds
        // since the call began (`Math.round((Date.now()-d)/1000)`).
        let elapsed_ms = started.elapsed().as_millis();
        let elapsed_secs = ((elapsed_ms + 500) / 1000) as u64;
        let message = crate::auto_background::background_message(
            &format!("{server}/{tool}"),
            &task_id,
            elapsed_secs,
        );
        Ok(ToolCallResult {
            data: json!([{ "type": "text", "text": message }]),
            model_content: Some(message),
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

// -- impl Tool for McpAuthTool -----------------------------------------------

#[async_trait]
impl Tool for McpAuthTool {
    fn name(&self) -> &str {
        MCP_AUTH_TOOL_NAME
    }
    fn input_schema(&self) -> &Value {
        &MCP_AUTH_SCHEMA
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        4_096
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    fn is_open_world(&self, _: &Value) -> bool {
        false
    }
    fn interrupt_behavior(&self, _: &Value) -> InterruptBehavior {
        InterruptBehavior::Block
    }

    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        allow_mcp("MCP auth inspection")
    }

    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        "Inspect the configured auth/transport surface of a registered MCP server.".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> String {
        "Use McpAuth to read an MCP server's transport+auth config.".into()
    }

    async fn call(
        &self,
        input: Value,
        _ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let started = Instant::now();
        let server_name = input
            .get("server_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ToolError::InvalidInput("McpAuthTool: missing or non-string server_name".into())
            })?
            .to_string();
        let bus = &self.ctx.bus;
        emit(
            bus,
            MCP_AUTH_STARTED,
            &[("_PROTO_server_name", pii(&server_name))],
        )
        .await;

        let registry = match self.ctx.mcp_registry.as_ref() {
            Some(r) => r,
            None => {
                emit(
                    bus,
                    MCP_AUTH_FAILED,
                    &[
                        ("_PROTO_server_name", pii(&server_name)),
                        ("error_kind", verified_str("registry_unconfigured")),
                    ],
                )
                .await;
                return Err(ToolError::Internal(
                    "McpAuthTool: MCP registry not configured on this host".into(),
                ));
            }
        };

        let cfg = match registry.get_config(&server_name).await {
            Some(c) => c,
            None => {
                emit(
                    bus,
                    MCP_AUTH_FAILED,
                    &[
                        ("_PROTO_server_name", pii(&server_name)),
                        ("error_kind", verified_str("server_not_registered")),
                    ],
                )
                .await;
                return Err(ToolError::InvalidInput(format!(
                    "McpAuthTool: MCP server {server_name:?} is not registered"
                )));
            }
        };
        let (kind, auth_kind) = auth_kind_from_spec(&cfg.spec);

        let (client_id, callback_port, auth_meta_url, has_headers, has_helper) = match &cfg.spec {
            McpTransportSpec::Sse {
                oauth,
                headers,
                headers_helper,
                ..
            } => {
                let (cid, port, url) = oauth
                    .as_ref()
                    .map(|o| {
                        (
                            o.client_id.clone(),
                            o.callback_port,
                            o.auth_server_metadata_url.clone(),
                        )
                    })
                    .unwrap_or_default();
                (
                    cid,
                    port,
                    url,
                    !headers.is_empty(),
                    headers_helper.is_some(),
                )
            }
            McpTransportSpec::Http { oauth, headers, .. } => {
                let (cid, port, url) = oauth
                    .as_ref()
                    .map(|o| {
                        (
                            o.client_id.clone(),
                            o.callback_port,
                            o.auth_server_metadata_url.clone(),
                        )
                    })
                    .unwrap_or_default();
                (cid, port, url, !headers.is_empty(), false)
            }
            McpTransportSpec::WebSocket { headers, .. } => {
                (None, None, None, !headers.is_empty(), false)
            }
            _ => (None, None, None, false, false),
        };

        emit(
            bus,
            MCP_AUTH_COMPLETED,
            &[
                ("_PROTO_server_name", pii(&server_name)),
                ("auth_kind", verified_str(auth_kind)),
                (
                    "duration_ms",
                    verified_int(started.elapsed().as_millis() as u64),
                ),
            ],
        )
        .await;

        Ok(ToolCallResult {
            data: json!({
                "server_name": server_name,
                "transport_kind": kind,
                "auth_kind": auth_kind,
                "client_id": client_id,
                "callback_port": callback_port,
                "auth_server_metadata_url": auth_meta_url,
                "has_static_headers": has_headers,
                "has_headers_helper": has_helper,
            }),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

// -- impl Tool for ListMcpResourcesTool --------------------------------------

#[async_trait]
impl Tool for ListMcpResourcesTool {
    fn name(&self) -> &str {
        LIST_MCP_RESOURCES_TOOL_NAME
    }
    /// 2.1.206 tool-definition `searchHint` (byte-verified).
    fn search_hint(&self) -> Option<&str> {
        Some("list resources from connected MCP servers")
    }
    fn input_schema(&self) -> &Value {
        &LIST_MCP_RESOURCES_SCHEMA
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        4_096
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    fn is_open_world(&self, _: &Value) -> bool {
        false
    }
    fn interrupt_behavior(&self, _: &Value) -> InterruptBehavior {
        InterruptBehavior::Block
    }

    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        allow_mcp("list MCP resources")
    }

    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        "List resources advertised by a registered MCP server.".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> String {
        "Use ListMcpResourcesTool to enumerate an MCP server's resources.".into()
    }

    async fn call(
        &self,
        input: Value,
        _ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let started = Instant::now();
        // `server_name` is OPTIONAL (Batch 5c). Present → list one server's
        // resources (legacy behavior). Absent → list EVERY connected server's
        // resources, error-isolated per server. Mirrors
        // `ListMcpResourcesTool.ts:66-101` (`targetServer` filter vs all clients).
        let target_server = input
            .get("server_name")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        let bus = &self.ctx.bus;
        emit(
            bus,
            LIST_MCP_RESOURCES_STARTED,
            &[(
                "_PROTO_server_name",
                pii(target_server.as_deref().unwrap_or("<all>")),
            )],
        )
        .await;

        let registry = match self.ctx.mcp_registry.as_ref() {
            Some(r) => r,
            None => {
                emit(
                    bus,
                    LIST_MCP_RESOURCES_FAILED,
                    &[
                        (
                            "_PROTO_server_name",
                            pii(target_server.as_deref().unwrap_or("<all>")),
                        ),
                        ("error_kind", verified_str("registry_unconfigured")),
                    ],
                )
                .await;
                return Err(ToolError::Internal(
                    "ListMcpResourcesTool: MCP registry not configured on this host".into(),
                ));
            }
        };

        if let Some(requested) = target_server.as_deref() {
            let server_name = match resolve_resource_server_name(registry, requested).await {
                Ok(name) => name,
                Err(ResourceServerLookupError::NotRegistered) => {
                    emit(
                        bus,
                        LIST_MCP_RESOURCES_FAILED,
                        &[
                            ("_PROTO_server_name", pii(requested)),
                            ("error_kind", verified_str("server_not_registered")),
                        ],
                    )
                    .await;
                    return Err(ToolError::InvalidInput(format!(
                        "ListMcpResourcesTool: MCP server {requested:?} is not registered"
                    )));
                }
                Err(ResourceServerLookupError::NotConnected { raw_name }) => {
                    emit(
                        bus,
                        LIST_MCP_RESOURCES_FAILED,
                        &[
                            ("_PROTO_server_name", pii(&raw_name)),
                            ("error_kind", verified_str("server_not_connected")),
                        ],
                    )
                    .await;
                    return Err(ToolError::InvalidInput(format!(
                        "ListMcpResourcesTool: MCP server {raw_name:?} is not connected"
                    )));
                }
                Err(ResourceServerLookupError::NoResources { raw_name }) => {
                    emit(
                        bus,
                        LIST_MCP_RESOURCES_FAILED,
                        &[
                            ("_PROTO_server_name", pii(&raw_name)),
                            ("error_kind", verified_str("unsupported_capability")),
                        ],
                    )
                    .await;
                    return Err(ToolError::InvalidInput(format!(
                        "ListMcpResourcesTool: MCP server {raw_name:?} does not support resources"
                    )));
                }
                Err(ResourceServerLookupError::Ambiguous {
                    requested,
                    candidates,
                }) => {
                    emit(
                        bus,
                        LIST_MCP_RESOURCES_FAILED,
                        &[
                            ("_PROTO_server_name", pii(&requested)),
                            ("error_kind", verified_str("ambiguous_server_name")),
                        ],
                    )
                    .await;
                    return Err(ToolError::InvalidInput(format!(
                        "ListMcpResourcesTool: {}",
                        ambiguous_resource_server_message(&requested, &candidates)
                    )));
                }
            };

            let client = match registry.ensure_connected_client(&server_name).await {
                Ok(client) => client,
                Err(e) => {
                    emit(
                        bus,
                        LIST_MCP_RESOURCES_FAILED,
                        &[
                            ("_PROTO_server_name", pii(&server_name)),
                            ("error_kind", verified_str("rpc")),
                        ],
                    )
                    .await;
                    return Err(ToolError::Io(format!(
                        "ListMcpResourcesTool: MCP server {server_name:?} connection error: {e}"
                    )));
                }
            };
            let server_name = match resolve_live_resource_server_name(registry, &server_name).await
            {
                Ok(server_name) => server_name,
                Err(ResourceServerLookupError::NotRegistered) => {
                    emit(
                        bus,
                        LIST_MCP_RESOURCES_FAILED,
                        &[
                            ("_PROTO_server_name", pii(&server_name)),
                            ("error_kind", verified_str("server_not_registered")),
                        ],
                    )
                    .await;
                    return Err(ToolError::InvalidInput(format!(
                        "ListMcpResourcesTool: MCP server {server_name:?} is not registered"
                    )));
                }
                Err(ResourceServerLookupError::NotConnected { raw_name }) => {
                    emit(
                        bus,
                        LIST_MCP_RESOURCES_FAILED,
                        &[
                            ("_PROTO_server_name", pii(&raw_name)),
                            ("error_kind", verified_str("server_not_connected")),
                        ],
                    )
                    .await;
                    return Err(ToolError::InvalidInput(format!(
                        "ListMcpResourcesTool: MCP server {raw_name:?} is not connected"
                    )));
                }
                Err(ResourceServerLookupError::NoResources { raw_name }) => {
                    emit(
                        bus,
                        LIST_MCP_RESOURCES_FAILED,
                        &[
                            ("_PROTO_server_name", pii(&raw_name)),
                            ("error_kind", verified_str("unsupported_capability")),
                        ],
                    )
                    .await;
                    return Err(ToolError::InvalidInput(format!(
                        "ListMcpResourcesTool: MCP server {raw_name:?} does not support resources"
                    )));
                }
                Err(ResourceServerLookupError::Ambiguous {
                    requested,
                    candidates,
                }) => {
                    emit(
                        bus,
                        LIST_MCP_RESOURCES_FAILED,
                        &[
                            ("_PROTO_server_name", pii(&requested)),
                            ("error_kind", verified_str("ambiguous_server_name")),
                        ],
                    )
                    .await;
                    return Err(ToolError::InvalidInput(format!(
                        "ListMcpResourcesTool: {}",
                        ambiguous_resource_server_message(&requested, &candidates)
                    )));
                }
            };

            let mut resources: Vec<Value> = Vec::new();
            match client.list_resources().await {
                Ok(list) => {
                    for r in list {
                        resources.push(tag_resource_with_server(&r, &server_name));
                    }
                }
                Err(_e) => {
                    emit(
                        bus,
                        LIST_MCP_RESOURCES_FAILED,
                        &[
                            ("_PROTO_server_name", pii(&server_name)),
                            ("error_kind", verified_str("rpc")),
                        ],
                    )
                    .await;
                }
            }
            let count = resources.len() as u64;
            emit(
                bus,
                LIST_MCP_RESOURCES_COMPLETED,
                &[
                    ("_PROTO_server_name", pii(requested)),
                    ("count", verified_int(count)),
                    (
                        "duration_ms",
                        verified_int(started.elapsed().as_millis() as u64),
                    ),
                ],
            )
            .await;
            return Ok(ToolCallResult {
                data: json!({ "server_name": target_server, "resources": resources }),
                model_content: None,
                new_messages: vec![],
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
            });
        }

        // All-servers path: enumerate every `Connected` OR `Cached`
        // resource-capable server, then fetch best-effort per server.
        let servers = resource_capable_server_names(registry).await;
        let mut resources: Vec<Value> = Vec::new();
        for server in &servers {
            let client = match registry.ensure_connected_client(server).await {
                Ok(client) => client,
                Err(_e) => {
                    emit(
                        bus,
                        LIST_MCP_RESOURCES_FAILED,
                        &[
                            ("_PROTO_server_name", pii(server)),
                            ("error_kind", verified_str("rpc")),
                        ],
                    )
                    .await;
                    continue;
                }
            };
            let server = match resolve_live_resource_server_name(registry, server).await {
                Ok(server) => server,
                Err(ResourceServerLookupError::NotRegistered)
                | Err(ResourceServerLookupError::NotConnected { .. })
                | Err(ResourceServerLookupError::NoResources { .. })
                | Err(ResourceServerLookupError::Ambiguous { .. }) => {
                    emit(
                        bus,
                        LIST_MCP_RESOURCES_FAILED,
                        &[
                            ("_PROTO_server_name", pii(server)),
                            ("error_kind", verified_str("unsupported_capability")),
                        ],
                    )
                    .await;
                    continue;
                }
            };
            match client.list_resources().await {
                Ok(list) => {
                    for r in list {
                        resources.push(tag_resource_with_server(&r, &server));
                    }
                }
                Err(_e) => {
                    // Isolate: emit the per-server failure telemetry + continue
                    // (do NOT fail the whole call). Mirrors the TS `catch` that
                    // logs via `logMCPError` and returns [] for that client
                    // (`ListMcpResourcesTool.ts:90-94`).
                    emit(
                        bus,
                        LIST_MCP_RESOURCES_FAILED,
                        &[
                            ("_PROTO_server_name", pii(&server)),
                            ("error_kind", verified_str("rpc")),
                        ],
                    )
                    .await;
                }
            }
        }

        let count = resources.len() as u64;
        emit(
            bus,
            LIST_MCP_RESOURCES_COMPLETED,
            &[
                (
                    "_PROTO_server_name",
                    pii(target_server.as_deref().unwrap_or("<all>")),
                ),
                ("count", verified_int(count)),
                (
                    "duration_ms",
                    verified_int(started.elapsed().as_millis() as u64),
                ),
            ],
        )
        .await;
        Ok(ToolCallResult {
            // `resources` is now a flat array of `{uri, name, mimeType?, server}`
            // objects (matches `ListMcpResourcesTool.ts:26-34` output rows). The
            // optional `server_name` echo is retained for the single-server path.
            data: json!({ "server_name": target_server, "resources": resources }),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

enum ResourceServerLookupError {
    NotRegistered,
    NotConnected {
        raw_name: String,
    },
    NoResources {
        raw_name: String,
    },
    Ambiguous {
        requested: String,
        candidates: Vec<String>,
    },
}

fn is_shared_registry_entry(table_key: &str, state: &mcp::McpConnectionState) -> bool {
    table_key == state.config().name
}

fn ambiguous_resource_server_message(requested: &str, candidates: &[String]) -> String {
    format!(
        "MCP server name {requested:?} is ambiguous; matching servers: {}",
        candidates.join(", ")
    )
}

/// Resolve `requested` to a raw server name when the registry currently holds a
/// `Connected` or `Cached` entry that declares `capabilities.resources`.
async fn resolve_resource_server_name(
    registry: &McpRegistry,
    requested: &str,
) -> Result<String, ResourceServerLookupError> {
    use mcp::McpConnectionState;
    let conns = registry.connections.read().await;
    if let Some(state) = conns.get(requested) {
        if is_shared_registry_entry(requested, state) {
            return match state {
                McpConnectionState::Connected {
                    config,
                    capabilities,
                    ..
                }
                | McpConnectionState::Cached {
                    config,
                    capabilities,
                    ..
                } if capabilities.resources => Ok(config.name.clone()),
                McpConnectionState::Connected { config, .. }
                | McpConnectionState::Cached { config, .. } => {
                    Err(ResourceServerLookupError::NoResources {
                        raw_name: config.name.clone(),
                    })
                }
                _ => Err(ResourceServerLookupError::NotConnected {
                    raw_name: requested.to_string(),
                }),
            };
        }
    }

    let requested_normalized = mcp::normalization::normalize_name_for_mcp(requested);
    let mut matches = conns
        .iter()
        .filter_map(|(table_key, state)| {
            if !is_shared_registry_entry(table_key, state) {
                return None;
            }
            let raw_name = state.config().name.as_str();
            if mcp::normalization::normalize_name_for_mcp(raw_name) != requested_normalized {
                return None;
            }
            Some((
                raw_name.to_string(),
                match state {
                    McpConnectionState::Connected {
                        config,
                        capabilities,
                        ..
                    }
                    | McpConnectionState::Cached {
                        config,
                        capabilities,
                        ..
                    } if capabilities.resources => Ok(config.name.clone()),
                    McpConnectionState::Connected { config, .. }
                    | McpConnectionState::Cached { config, .. } => {
                        Err(ResourceServerLookupError::NoResources {
                            raw_name: config.name.clone(),
                        })
                    }
                    _ => Err(ResourceServerLookupError::NotConnected {
                        raw_name: raw_name.to_string(),
                    }),
                },
            ))
        })
        .collect::<Vec<_>>();
    matches.sort_by(|a, b| a.0.cmp(&b.0));
    match matches.len() {
        0 => Err(ResourceServerLookupError::NotRegistered),
        1 => matches.pop().unwrap().1,
        _ => Err(ResourceServerLookupError::Ambiguous {
            requested: requested.to_string(),
            candidates: matches.into_iter().map(|(name, _)| name).collect(),
        }),
    }
}

/// After a lazy dial, re-check the CURRENT registry state for `requested` and
/// return its raw server name only when it is live `Connected` and still
/// advertises resources.
async fn resolve_live_resource_server_name(
    registry: &McpRegistry,
    requested_raw: &str,
) -> Result<String, ResourceServerLookupError> {
    use mcp::McpConnectionState;
    let conns = registry.connections.read().await;
    let Some(state) = conns.get(requested_raw) else {
        return Err(ResourceServerLookupError::NotRegistered);
    };
    if !is_shared_registry_entry(requested_raw, state) {
        return Err(ResourceServerLookupError::NotRegistered);
    }
    match state {
        McpConnectionState::Connected {
            config,
            capabilities,
            ..
        } if capabilities.resources => Ok(config.name.clone()),
        McpConnectionState::Connected { config, .. } => {
            Err(ResourceServerLookupError::NoResources {
                raw_name: config.name.clone(),
            })
        }
        _ => Err(ResourceServerLookupError::NotConnected {
            raw_name: requested_raw.to_string(),
        }),
    }
}

/// Names of every resource-capable `Connected` OR `Cached` MCP server in
/// `registry`, sorted for determinism (mirrors `snapshot`'s ordering). Used by
/// the all-servers `ListMcpResources` path to know which clients to fetch.
async fn resource_capable_server_names(registry: &McpRegistry) -> Vec<String> {
    use mcp::McpConnectionState;
    let conns = registry.connections.read().await;
    let mut names: Vec<String> = conns
        .iter()
        .filter_map(|(table_key, state)| {
            if !is_shared_registry_entry(table_key, state) {
                return None;
            }
            match state {
                McpConnectionState::Connected {
                    config,
                    capabilities,
                    ..
                }
                | McpConnectionState::Cached {
                    config,
                    capabilities,
                    ..
                } if capabilities.resources => Some(config.name.clone()),
                _ => None,
            }
        })
        .collect();
    names.sort();
    names
}

/// Shape one [`platform_api::McpResourceDto`] into the output row, adding the
/// `server` tag. In-tool JSON shaping (NOT a `traits` DTO widen) so the
/// frozen `McpResourceDto` is untouched. Field names mirror
/// `ListMcpResourcesTool.ts:26-34` (`uri`, `name`, `mimeType`, `server`); a
/// `None` `mime_type` is omitted (the TS field is `optional`).
fn tag_resource_with_server(r: &platform_api::McpResourceDto, server: &str) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("uri".into(), json!(r.uri));
    obj.insert("name".into(), json!(r.name));
    if let Some(mime) = &r.mime_type {
        obj.insert("mimeType".into(), json!(mime));
    }
    obj.insert("server".into(), json!(server));
    Value::Object(obj)
}

// -- impl Tool for ReadMcpResourceTool ---------------------------------------

#[async_trait]
impl Tool for ReadMcpResourceTool {
    fn name(&self) -> &str {
        READ_MCP_RESOURCE_TOOL_NAME
    }
    /// 2.1.206 tool-definition `searchHint` (byte-verified).
    fn search_hint(&self) -> Option<&str> {
        Some("read a specific MCP resource by URI")
    }
    fn input_schema(&self) -> &Value {
        &READ_MCP_RESOURCE_SCHEMA
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        30_000
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    fn is_open_world(&self, _: &Value) -> bool {
        true
    }
    fn interrupt_behavior(&self, _: &Value) -> InterruptBehavior {
        InterruptBehavior::Cancel
    }

    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        allow_mcp("read MCP resource")
    }

    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        "Read a single resource from a registered MCP server by URI.".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> String {
        "Use ReadMcpResourceTool to fetch an MCP resource's contents.".into()
    }

    async fn call(
        &self,
        input: Value,
        _ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let started = Instant::now();
        let server_name = input
            .get("server_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ToolError::InvalidInput(
                    "ReadMcpResourceTool: missing or non-string server_name".into(),
                )
            })?
            .to_string();
        let uri = input
            .get("uri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ToolError::InvalidInput("ReadMcpResourceTool: missing or non-string uri".into())
            })?
            .to_string();
        let bus = &self.ctx.bus;
        emit(
            bus,
            READ_MCP_RESOURCE_STARTED,
            &[
                ("_PROTO_server_name", pii(&server_name)),
                ("_PROTO_resource_uri", pii(&uri)),
            ],
        )
        .await;

        let registry = match self.ctx.mcp_registry.as_ref() {
            Some(r) => r,
            None => {
                emit(
                    bus,
                    READ_MCP_RESOURCE_FAILED,
                    &[
                        ("_PROTO_server_name", pii(&server_name)),
                        ("error_kind", verified_str("registry_unconfigured")),
                    ],
                )
                .await;
                return Err(ToolError::Internal(
                    "ReadMcpResourceTool: MCP registry not configured on this host".into(),
                ));
            }
        };

        let server_name = match resolve_resource_server_name(registry, &server_name).await {
            Ok(server_name) => server_name,
            Err(ResourceServerLookupError::NotRegistered) => {
                emit(
                    bus,
                    READ_MCP_RESOURCE_FAILED,
                    &[
                        ("_PROTO_server_name", pii(&server_name)),
                        ("error_kind", verified_str("server_not_registered")),
                    ],
                )
                .await;
                return Err(ToolError::InvalidInput(format!(
                    "ReadMcpResourceTool: MCP server {server_name:?} is not registered"
                )));
            }
            Err(ResourceServerLookupError::NotConnected { raw_name }) => {
                emit(
                    bus,
                    READ_MCP_RESOURCE_FAILED,
                    &[
                        ("_PROTO_server_name", pii(&raw_name)),
                        ("error_kind", verified_str("server_not_connected")),
                    ],
                )
                .await;
                return Err(ToolError::InvalidInput(format!(
                    "ReadMcpResourceTool: MCP server {raw_name:?} is not connected"
                )));
            }
            Err(ResourceServerLookupError::NoResources { raw_name }) => {
                emit(
                    bus,
                    READ_MCP_RESOURCE_FAILED,
                    &[
                        ("_PROTO_server_name", pii(&raw_name)),
                        ("error_kind", verified_str("unsupported_capability")),
                    ],
                )
                .await;
                return Err(ToolError::InvalidInput(format!(
                    "ReadMcpResourceTool: MCP server {raw_name:?} does not support resources"
                )));
            }
            Err(ResourceServerLookupError::Ambiguous {
                requested,
                candidates,
            }) => {
                emit(
                    bus,
                    READ_MCP_RESOURCE_FAILED,
                    &[
                        ("_PROTO_server_name", pii(&requested)),
                        ("error_kind", verified_str("ambiguous_server_name")),
                    ],
                )
                .await;
                return Err(ToolError::InvalidInput(format!(
                    "ReadMcpResourceTool: {}",
                    ambiguous_resource_server_message(&requested, &candidates)
                )));
            }
        };

        let client = match registry.ensure_connected_client(&server_name).await {
            Ok(c) => c,
            Err(e) => {
                emit(
                    bus,
                    READ_MCP_RESOURCE_FAILED,
                    &[
                        ("_PROTO_server_name", pii(&server_name)),
                        ("_PROTO_resource_uri", pii(&uri)),
                        ("error_kind", verified_str("rpc")),
                    ],
                )
                .await;
                return Err(ToolError::Io(format!(
                    "ReadMcpResourceTool: server {server_name:?} read_resource error: {e}"
                )));
            }
        };
        let server_name = match resolve_live_resource_server_name(registry, &server_name).await {
            Ok(server_name) => server_name,
            Err(ResourceServerLookupError::NotRegistered) => {
                emit(
                    bus,
                    READ_MCP_RESOURCE_FAILED,
                    &[
                        ("_PROTO_server_name", pii(&server_name)),
                        ("error_kind", verified_str("server_not_registered")),
                    ],
                )
                .await;
                return Err(ToolError::InvalidInput(format!(
                    "ReadMcpResourceTool: MCP server {server_name:?} is not registered"
                )));
            }
            Err(ResourceServerLookupError::NotConnected { raw_name }) => {
                emit(
                    bus,
                    READ_MCP_RESOURCE_FAILED,
                    &[
                        ("_PROTO_server_name", pii(&raw_name)),
                        ("error_kind", verified_str("server_not_connected")),
                    ],
                )
                .await;
                return Err(ToolError::InvalidInput(format!(
                    "ReadMcpResourceTool: MCP server {raw_name:?} is not connected"
                )));
            }
            Err(ResourceServerLookupError::NoResources { raw_name }) => {
                emit(
                    bus,
                    READ_MCP_RESOURCE_FAILED,
                    &[
                        ("_PROTO_server_name", pii(&raw_name)),
                        ("error_kind", verified_str("unsupported_capability")),
                    ],
                )
                .await;
                return Err(ToolError::InvalidInput(format!(
                    "ReadMcpResourceTool: MCP server {raw_name:?} does not support resources"
                )));
            }
            Err(ResourceServerLookupError::Ambiguous {
                requested,
                candidates,
            }) => {
                emit(
                    bus,
                    READ_MCP_RESOURCE_FAILED,
                    &[
                        ("_PROTO_server_name", pii(&requested)),
                        ("error_kind", verified_str("ambiguous_server_name")),
                    ],
                )
                .await;
                return Err(ToolError::InvalidInput(format!(
                    "ReadMcpResourceTool: {}",
                    ambiguous_resource_server_message(&requested, &candidates)
                )));
            }
        };

        // MCP-5d: read the FULL multi-content `contents[]` array, carrying
        // `mimeType`, distinguishing text from base64 blobs, persisting decoded
        // blobs to disk under a project-local tool-results dir, and surfacing
        // `blobSavedTo` paths. Mirrors `ReadMcpResourceTool.ts:95-143`.
        let output_dir = self.ctx.tool_results_dir();
        match client.read_resource_rich(&uri, &output_dir).await {
            Ok(contents) => {
                let bytes_approx = serde_json::to_vec(&contents)
                    .map(|v| v.len() as u64)
                    .unwrap_or(0);
                emit(
                    bus,
                    READ_MCP_RESOURCE_COMPLETED,
                    &[
                        ("_PROTO_server_name", pii(&server_name)),
                        ("_PROTO_resource_uri", pii(&uri)),
                        ("bytes", verified_int(bytes_approx)),
                        (
                            "duration_ms",
                            verified_int(started.elapsed().as_millis() as u64),
                        ),
                    ],
                )
                .await;
                Ok(ToolCallResult {
                    // Output shape mirrors `ReadMcpResourceTool.ts` `outputSchema`
                    // `{ contents: [{uri, mimeType?, text?, blobSavedTo?}] }`,
                    // wrapped with the existing `{server_name, uri}` envelope the
                    // Rust tool surfaces.
                    data: json!({
                        "server_name": server_name,
                        "uri": uri,
                        "contents": contents,
                    }),
                    model_content: None,
                    new_messages: vec![],
                    context_modifier: None,
                    is_error: false,
                    mcp_meta: None,
                })
            }
            Err(e) => {
                emit(
                    bus,
                    READ_MCP_RESOURCE_FAILED,
                    &[
                        ("_PROTO_server_name", pii(&server_name)),
                        ("_PROTO_resource_uri", pii(&uri)),
                        ("error_kind", verified_str("rpc")),
                    ],
                )
                .await;
                Err(ToolError::Io(format!(
                    "ReadMcpResourceTool: server {server_name:?} read_resource error: {e}"
                )))
            }
        }
    }
}

// -- Per-tool wire-entry builder (Batch 3) -----------------------------------

/// Build one [`MCPTool`] per-tool wire entry for EVERY `Connected` server,
/// grouped by [`McpConnectionId`] so the engine can register them through
/// [`tool_api::ToolRegistry::register_mcp_tools`] (and drop them en masse on
/// disconnect via the matching `conn_id`).
///
/// For each `Connected` server we map each [`platform_api::McpToolDto`] →
/// `Arc::new(MCPTool::new_for_tool(ctx, dto.full_name, dto.description,
/// dto.input_schema, dto.search_hint, dto.always_load,
/// dto.requires_user_interaction))`. The resulting tool's wire `name()` is the real
/// `mcp__<server>__<tool>` FQN, its `input_schema()` is the server's own
/// `inputSchema`, and its `description()`/`prompt()` is the server's
/// (truncated) description — so the model addresses it by name with the
/// server schema's payload, and dispatch routes back through the same
/// connection's `McpClient::call_tool`. Mirrors claude-code's
/// `fetchToolsForClient` building one `Tool` per server tool
/// (`services/mcp/client.ts:1766-1990`).
///
/// Placed here (in `tool-mcp`, which already deps both `mcp` and `tool-api`)
/// rather than in the `mcp` crate, to keep `mcp` free of a `tool-api`
/// dependency. Reads the registry's `pub connections` state-map directly (the
/// same accessor the existing `snapshot` walks).
///
/// ## The MCP-resource trio
///
/// `ListMcpResourcesTool`, `ReadMcpResourceTool` and `ReadMcpResourceDirTool`
/// are NOT base-list tools. 2.1.238's `getTools` (`iJ`) strips all three out by
/// name (`let r=new Set([K8.name,Z8.name,Yme.name,Ky])`, `cc-238.js @230759264`)
/// and the discovery round pushes them into this MCP partition only when a
/// CONNECTED server declares `capabilities.resources`:
///
/// ```text
/// if(n.some((l)=>l.type==="connected"&&!!l.capabilities?.resources)){
///   if(![K8,Z8].some((c)=>o.some((u)=>il(u,c.name))))o.push(K8,Z8,Yme)}
/// ```
///
/// (`cc-238.js @225994181`; the dial-round site at `@243875044` carries the same
/// once-only `k` latch this function's `resource_tools_pushed` reproduces). A
/// session with zero resource-capable MCP servers therefore no longer ships
/// three unusable tool schemas in every request's `tools` array.
pub fn configured_permission_ceiling(
    config: &mcp::McpServerConfig,
    tool_name: &str,
) -> Option<platform_api::McpPermissionCeiling> {
    let mut ceiling = config.tool_permissions.get(tool_name).copied();
    for configured in config
        .tools
        .iter()
        .filter(|configured| configured.name == tool_name)
    {
        if let Some(policy) = configured.permission_policy {
            let policy_ceiling = match policy {
                platform_api::McpToolPermissionPolicy::AlwaysAllow => {
                    platform_api::McpPermissionCeiling::Allow
                }
                platform_api::McpToolPermissionPolicy::AlwaysAsk => {
                    platform_api::McpPermissionCeiling::Ask
                }
                platform_api::McpToolPermissionPolicy::AlwaysDeny => {
                    platform_api::McpPermissionCeiling::Deny
                }
            };
            ceiling =
                Some(ceiling.map_or(policy_ceiling, |current| current.strictest(policy_ceiling)));
        }
        if let Some(org_ceiling) = configured.org_max_permission {
            ceiling = Some(ceiling.map_or(org_ceiling, |current| current.strictest(org_ceiling)));
        }
    }
    ceiling
}

fn max_permission_from_ceiling(
    ceiling: platform_api::McpPermissionCeiling,
) -> McpToolMaxPermission {
    match ceiling {
        platform_api::McpPermissionCeiling::Allow => McpToolMaxPermission::Allow,
        platform_api::McpPermissionCeiling::Ask => McpToolMaxPermission::Ask,
        platform_api::McpPermissionCeiling::Deny => McpToolMaxPermission::Blocked,
    }
}

pub async fn build_registered_mcp_tools(
    registry: &McpRegistry,
    ctx: tool_api::BuiltinToolContext,
) -> Vec<(protocol::McpConnectionId, Vec<Arc<dyn Tool>>)> {
    use mcp::McpConnectionState;

    let conns = registry.connections.read().await;
    let mut out: Vec<(protocol::McpConnectionId, Vec<Arc<dyn Tool>>)> = Vec::new();
    // Oracle `k` in the dial round at `cc-238.js @243875044`
    // (`if(fe.capabilities?.resources&&!k)k=!0,ne=[...ne,K8,Z8,Yme]`): the three
    // resource tools are pushed ONCE, onto the first connected server that
    // declares `capabilities.resources`.
    let mut resource_tools_pushed = false;
    for (table_key, state) in conns.iter() {
        // §11 Stage 2 (LingXi discovery cache): a `Cached` server was served
        // from disk with NO transport dialed yet — its catalog fields mirror
        // `Connected`'s exactly (see `McpConnectionState::Cached`'s doc), so
        // it builds the SAME per-tool wire entries here. Dispatch (`MCPTool::
        // call` → `call_tool_with_auth_retry`) lazily dials it on first
        // invocation; a `Cached` server that never appeared here would be
        // hidden from the model entirely, defeating the point of caching it.
        if let McpConnectionState::Connected {
            config,
            connection_id,
            capabilities,
            tools,
            ..
        }
        | McpConnectionState::Cached {
            config,
            connection_id,
            capabilities,
            tools,
            ..
        } = state
        {
            // Shared/session tool registration is keyed by the raw server name.
            // Agent-scoped entries live under a synthetic table key and are
            // surfaced only by the dedicated bound-key builder path.
            if table_key != &config.name {
                continue;
            }
            let mut handles: Vec<Arc<dyn Tool>> = tools
                .iter()
                .map(|dto| {
                    let tool = MCPTool::new_for_tool(
                        ctx.clone(),
                        dto.full_name.clone(),
                        dto.description.clone(),
                        dto.input_schema.clone(),
                        dto.output_schema.clone(),
                        None,
                        dto.search_hint.clone(),
                        config.always_load || dto.always_load.unwrap_or(false),
                        dto.requires_user_interaction,
                    );
                    let tool = if let Some(ceiling) =
                        configured_permission_ceiling(config, &dto.tool_name)
                    {
                        tool.with_mcp_permission_ceiling(ceiling)
                    } else {
                        tool
                    };
                    let tool = tool.with_mcp_role(
                        (config.metadata.role == Some(mcp::McpServerRole::Comms))
                            .then(|| "comms".to_string()),
                    );
                    Arc::new(tool) as Arc<dyn Tool>
                })
                .collect();
            if capabilities.resources && !resource_tools_pushed {
                resource_tools_pushed = true;
                handles.push(Arc::new(ListMcpResourcesTool::new(ctx.clone())));
                handles.push(Arc::new(ReadMcpResourceTool::new(ctx.clone())));
                handles.push(Arc::new(
                    crate::read_mcp_resource_dir::ReadMcpResourceDirTool::new(ctx.clone()),
                ));
            }
            out.push((*connection_id, handles));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as StdHashMap;

    /// MCP results persist above the folded 50 000 threshold.
    ///
    /// claude-code's MCP factory (2.1.220 BIN off 232139111) is
    ///   `maxResultSizeChars: N ? Math.min(L, gor) : Gar.maxResultSizeChars`
    ///   `persistenceThresholdCeiling: N ? gor : void 0`
    /// where `N` is "the server declared its own output cap", `L` is that cap,
    /// `gor = 500000`, and the generic MCP descriptor `Gar` declares
    /// `maxResultSizeChars: 1e5` (BIN off 231406237).
    ///
    /// `MCPTool` models no server-declared cap, so `N` is always FALSE here:
    /// the raw threshold is `Gar`'s 100 000 and the ceiling stays unset, which
    /// selects the `AKr = 50000` default. `M0u`'s fold
    /// (`Math.min(raw, ceiling ?? 50000)`) therefore yields 50 000.
    ///
    /// Deliberately NOT `max_result_size_chars()` (30 000 here): that is the
    /// truncation cap, a different oracle field. Reusing it would persist MCP
    /// output 20 000 chars earlier than claude does.
    #[test]
    fn mcp_persistence_threshold_folds_to_the_akr_default() {
        let tool = MCPTool::new(tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            std::sync::Arc::new(telemetry::AnalyticsBus::new()),
            vec![std::path::PathBuf::from("/tmp")],
        ));
        assert_eq!(
            tool.persistence_threshold(),
            Some(100_000),
            "raw declared value is Gar.maxResultSizeChars = 1e5"
        );
        assert_eq!(
            tool.persistence_threshold_ceiling(),
            None,
            "no server-declared cap => ceiling unset => AKr default applies"
        );
        assert_ne!(
            tool.persistence_threshold(),
            Some(tool.max_result_size_chars()),
            "persistence threshold must NOT be the truncation cap"
        );
    }

    /// §27b's LOAD-BEARING half: the oracle's MCP factory
    /// `checkPermissions` raises an ASK for a tool carrying
    /// `_meta.anthropic/requiresUserInteraction`
    /// (`if(Ee)return{behavior:"ask",message:"MCPTool requires permission.",
    /// suggestions:[],suppressAlwaysAllowRule:!0}`, @182520945).
    ///
    /// Without it the §27b dialog change is unreachable: `MCPTool` allowed
    /// EVERY call outright, so no permission prompt was ever built and the
    /// suppressed "Yes, allow always" row had no dialog to be missing from.
    /// The bit alone (asserted by the sibling test) does not prove this.
    #[tokio::test]
    async fn requires_user_interaction_raises_the_factory_ask() {
        let ctx = || {
            tool_api::test_support::ctx_for_file_tools(
                tool_api::test_support::make_dummy_fs(),
                std::sync::Arc::new(telemetry::AnalyticsBus::new()),
                vec![std::path::PathBuf::from("/tmp")],
            )
        };
        let use_ctx = tool_api::test_support::fresh_ctx();

        let interactive = MCPTool::new_for_tool(
            ctx(),
            "mcp__srv__interactive".into(),
            "d".into(),
            serde_json::json!({}),
            None,
            None,
            None,
            false,
            true,
        );
        let decision = interactive
            .check_permissions(&serde_json::json!({}), &use_ctx)
            .await;
        let PermissionResult::Ask { reason, prompt, .. } = decision else {
            panic!("a requiresUserInteraction tool must ASK, got: {decision:?}");
        };
        assert!(prompt.message.contains("requires approval"));
        assert_eq!(prompt.options, vec!["Allow once", "Deny"]);
        assert!(
            matches!(&reason, PermissionDecisionReason::PermissionPromptTool { tool_name } if tool_name == "mcp__srv__interactive"),
            "unexpected reason: {reason:?}"
        );

        // A plain MCP tool is unaffected — the passthrough/allow arm stands.
        let plain = MCPTool::new_for_tool(
            ctx(),
            "mcp__srv__plain".into(),
            "d".into(),
            serde_json::json!({}),
            None,
            None,
            None,
            false,
            false,
        );
        assert!(matches!(
            plain
                .check_permissions(&serde_json::json!({}), &use_ctx)
                .await,
            PermissionResult::Allow { .. }
        ));
    }

    #[tokio::test]
    async fn configured_ceiling_reaches_the_tool_permission_gate() {
        let ctx = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            Arc::new(telemetry::AnalyticsBus::new()),
            vec![std::path::PathBuf::from("/tmp")],
        );
        let tool = MCPTool::new_for_tool(
            ctx,
            "mcp__srv__blocked".into(),
            "blocked".into(),
            serde_json::json!({"type": "object"}),
            None,
            None,
            None,
            false,
            false,
        )
        .with_mcp_permission_ceiling(platform_api::McpPermissionCeiling::Deny);
        assert!(matches!(
            tool.check_permissions(&serde_json::json!({}), &tool_api::test_support::fresh_ctx())
                .await,
            PermissionResult::Deny { .. }
        ));
    }

    /// §27b: `MCPTool::new_for_tool`'s `requires_user_interaction` param must
    /// override the `Tool` trait's `false` default (`tool-api/src/tool_trait.rs`)
    /// — oracle `requiresUserInteraction(){return Ee}` (@182520425), where
    /// `Ee` is the same per-tool `_meta.anthropic/requiresUserInteraction` bit.
    #[test]
    fn requires_user_interaction_reflects_the_per_tool_bit() {
        let ctx = || {
            tool_api::test_support::ctx_for_file_tools(
                tool_api::test_support::make_dummy_fs(),
                std::sync::Arc::new(telemetry::AnalyticsBus::new()),
                vec![std::path::PathBuf::from("/tmp")],
            )
        };
        let plain = MCPTool::new_for_tool(
            ctx(),
            "mcp__srv__plain".into(),
            "d".into(),
            serde_json::json!({}),
            None,
            None,
            None,
            false,
            false,
        );
        assert!(!plain.requires_user_interaction());

        let interactive = MCPTool::new_for_tool(
            ctx(),
            "mcp__srv__interactive".into(),
            "d".into(),
            serde_json::json!({}),
            None,
            None,
            None,
            false,
            true,
        );
        assert!(interactive.requires_user_interaction());

        // The generic dispatcher (no bound per-tool DTO) is not marked
        // interactive — it has no single tool's `_meta` to read.
        assert!(!MCPTool::new(ctx()).requires_user_interaction());
    }

    #[test]
    fn parse_full_name_happy() {
        let (s, t) = parse_full_name("mcp__filesystem__read").unwrap();
        assert_eq!(s, "filesystem");
        assert_eq!(t, "read");
    }

    #[test]
    fn parse_full_name_tool_with_embedded_underscores() {
        let (s, t) = parse_full_name("mcp__fs__read_file").unwrap();
        assert_eq!(s, "fs");
        assert_eq!(t, "read_file");
        let (s2, t2) = parse_full_name("mcp__a__b__c").unwrap();
        assert_eq!(s2, "a");
        assert_eq!(t2, "b__c");
    }

    #[test]
    fn parse_full_name_rejects_missing_prefix() {
        let err = parse_full_name("filesystem__read").unwrap_err();
        match err {
            ToolError::InvalidInput(s) => {
                assert!(s.contains("does not match mcp__<server>__<tool> pattern"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn empty_schema_decodes_json_encoded_argument_types() {
        let schema = json!({});

        assert_eq!(
            normalize_empty_schema_arguments(Some(&schema), json!(r#"[1,2]"#)),
            json!([1, 2])
        );
        assert_eq!(
            normalize_empty_schema_arguments(Some(&schema), json!("42")),
            json!(42)
        );
        assert_eq!(
            normalize_empty_schema_arguments(Some(&schema), json!("true")),
            json!(true)
        );
        assert_eq!(
            normalize_empty_schema_arguments(Some(&schema), json!([1, 2])),
            json!([1, 2]),
            "already typed arguments remain unchanged"
        );
    }

    #[test]
    fn empty_schema_preserves_invalid_json_and_nonempty_schema_strings() {
        let empty_schema = json!({});
        assert_eq!(
            normalize_empty_schema_arguments(Some(&empty_schema), json!("not-json")),
            json!("not-json")
        );

        let object_schema = json!({"type":"object","properties":{}});
        assert_eq!(
            normalize_empty_schema_arguments(Some(&object_schema), json!(r#"[1,2]"#)),
            json!(r#"[1,2]"#),
            "only the literal empty schema relaxes argument typing"
        );
    }

    #[test]
    fn parse_full_name_rejects_empty_server() {
        let err = parse_full_name("mcp____read").unwrap_err();
        match err {
            ToolError::InvalidInput(s) => assert!(s.contains("has empty server segment")),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parse_full_name_rejects_empty_tool() {
        let err = parse_full_name("mcp__fs__").unwrap_err();
        match err {
            ToolError::InvalidInput(s) => assert!(s.contains("has empty tool segment")),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parse_full_name_rejects_no_separator_after_server() {
        let err = parse_full_name("mcp__filesystem").unwrap_err();
        match err {
            ToolError::InvalidInput(s) => {
                assert!(s.contains("does not match mcp__<server>__<tool> pattern"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn auth_kind_stdio() {
        let spec = McpTransportSpec::Stdio {
            command: "x".into(),
            args: vec![],
            env: StdHashMap::new(),
        };
        assert_eq!(auth_kind_from_spec(&spec), ("stdio", "none"));
    }

    #[test]
    fn auth_kind_sse_with_oauth() {
        let spec = McpTransportSpec::Sse {
            url: "https://x".into(),
            headers: platform_api::McpHeaders::new(),
            headers_helper: None,
            oauth: Some(platform_api::McpOAuthConfigDto {
                client_id: Some("cid".into()),
                callback_port: Some(8080),
                auth_server_metadata_url: Some("https://m".into()),
                scopes: None,
                xaa: Some(false),
            }),
        };
        assert_eq!(auth_kind_from_spec(&spec), ("sse", "oauth"));
    }

    #[test]
    fn auth_kind_sse_headers_helper() {
        let spec = McpTransportSpec::Sse {
            url: "https://x".into(),
            headers: platform_api::McpHeaders::new(),
            headers_helper: Some("/usr/bin/h".into()),
            oauth: None,
        };
        assert_eq!(auth_kind_from_spec(&spec), ("sse", "headers_helper"));
    }

    #[test]
    fn auth_kind_sse_static_headers() {
        let mut h = platform_api::McpHeaders::new();
        h.insert("Authorization".into(), "Bearer x".into());
        let spec = McpTransportSpec::Sse {
            url: "https://x".into(),
            headers: h,
            headers_helper: None,
            oauth: None,
        };
        assert_eq!(auth_kind_from_spec(&spec), ("sse", "static_headers"));
    }

    #[test]
    fn auth_kind_http_with_oauth() {
        let spec = McpTransportSpec::Http {
            url: "https://x".into(),
            headers: platform_api::McpHeaders::new(),
            headers_helper: None,
            oauth: Some(platform_api::McpOAuthConfigDto {
                client_id: None,
                callback_port: None,
                auth_server_metadata_url: None,
                scopes: None,
                xaa: None,
            }),
        };
        assert_eq!(auth_kind_from_spec(&spec), ("http", "oauth"));
    }

    #[test]
    fn auth_kind_websocket_static_headers() {
        let mut h = platform_api::McpHeaders::new();
        h.insert("X-Token".into(), "abc".into());
        let spec = McpTransportSpec::WebSocket {
            url: "wss://x".into(),
            headers: h,
            headers_helper: None,
        };
        assert_eq!(auth_kind_from_spec(&spec), ("websocket", "static_headers"));
    }

    #[test]
    fn auth_kind_in_process() {
        let spec = McpTransportSpec::InProcess {
            registry_key: "k".into(),
        };
        assert_eq!(auth_kind_from_spec(&spec), ("inProcess", "none"));
    }

    #[test]
    fn tool_name_locks() {
        assert_eq!(MCP_TOOL_NAME, "MCP");
        assert_eq!(MCP_AUTH_TOOL_NAME, "McpAuth");
        assert_eq!(LIST_MCP_RESOURCES_TOOL_NAME, "ListMcpResourcesTool");
        assert_eq!(READ_MCP_RESOURCE_TOOL_NAME, "ReadMcpResourceTool");
        assert_eq!(MCP_TOOL_FULL_NAME_PREFIX, "mcp__");
        assert_eq!(MCP_TOOL_FULL_NAME_SEPARATOR, "__");
    }

    #[test]
    fn mcp_timeout_byte_locked_literal() {
        // Cross-crate byte-lock with M2-02b lingxi-code/crates/mcp/src/client.rs:489.
        let err = mcp::McpClientError::Timeout {
            server: "filesystem".into(),
            tool: "read".into(),
            secs: 60,
        };
        assert_eq!(
            err.to_string(),
            "MCP server \"filesystem\" tool \"read\" timed out after 60s"
        );
    }

    #[test]
    fn error_string_templates() {
        // Concrete-string skeletons for parity fixture.
        let s = format!(
            "MCPTool: full_name {:?} does not match mcp__<server>__<tool> pattern",
            "x"
        );
        assert_eq!(
            s,
            "MCPTool: full_name \"x\" does not match mcp__<server>__<tool> pattern"
        );
        let s = format!("MCPTool: MCP server {:?} is not registered", "fs");
        assert_eq!(s, "MCPTool: MCP server \"fs\" is not registered");
        let s = format!("McpAuthTool: MCP server {:?} is not registered", "fs");
        assert_eq!(s, "McpAuthTool: MCP server \"fs\" is not registered");
        let s = format!(
            "ListMcpResourcesTool: MCP server {:?} is not registered",
            "fs"
        );
        assert_eq!(
            s,
            "ListMcpResourcesTool: MCP server \"fs\" is not registered"
        );
        let s = format!(
            "ReadMcpResourceTool: MCP server {:?} is not registered",
            "fs"
        );
        assert_eq!(
            s,
            "ReadMcpResourceTool: MCP server \"fs\" is not registered"
        );
    }

    // -- build_mcp_meta passthrough (MCP Batch 6) ----------------------------

    #[test]
    fn build_mcp_meta_both_present_byte_faithful() {
        let meta = json!({ "anthropic/trace": "abc", "nested": { "k": [1, 2, 3] } });
        let sc = json!({ "rows": [{ "id": 7 }], "total": 1 });
        let out = build_mcp_meta(Some(meta.clone()), Some(sc.clone())).expect("Some when present");
        // Mirrors claude-code `{ _meta, structuredContent }` with verbatim values.
        assert_eq!(out, json!({ "_meta": meta, "structuredContent": sc }));
    }

    #[test]
    fn build_mcp_meta_only_meta() {
        let meta = json!({ "x": 1 });
        let out = build_mcp_meta(Some(meta.clone()), None).expect("Some when _meta present");
        // Only the present key is included — no `structuredContent` placeholder.
        assert_eq!(out, json!({ "_meta": meta }));
        assert!(out
            .as_object()
            .expect("object")
            .get("structuredContent")
            .is_none());
    }

    #[test]
    fn build_mcp_meta_only_structured_content() {
        let sc = json!({ "y": 2 });
        let out =
            build_mcp_meta(None, Some(sc.clone())).expect("Some when structuredContent present");
        assert_eq!(out, json!({ "structuredContent": sc }));
        assert!(out.as_object().expect("object").get("_meta").is_none());
    }

    #[test]
    fn build_mcp_meta_both_absent_is_none() {
        // No `_meta`/`structuredContent` → `None` (never an empty object).
        assert!(build_mcp_meta(None, None).is_none());
    }

    #[test]
    fn build_mcp_meta_preserves_explicit_null_values() {
        // A present-but-`null` member is still "present" on the wire and is
        // forwarded verbatim (claude-code keys off truthiness; in Rust the
        // serde layer already distinguishes absent (`None`) from `null`
        // (`Some(Value::Null)`), so an explicit null is carried through).
        let out = build_mcp_meta(Some(Value::Null), None).expect("Some when key present");
        assert_eq!(out, json!({ "_meta": Value::Null }));
    }

    // -- MCP.4: progress-event payload shaping --------------------------------

    #[test]
    fn mcp_progress_event_data_full_payload() {
        // All optional fields present → full `mcp_progress`/`progress` payload
        // (client.ts:3104-3112).
        let ev = mcp::client::McpProgressEvent {
            progress: 3.0,
            total: Some(10.0),
            message: Some("halfway".into()),
        };
        let out = mcp_progress_event_data("srv", "tool", &ev);
        assert_eq!(
            out,
            json!({
                "type": "mcp_progress",
                "status": "progress",
                "serverName": "srv",
                "toolName": "tool",
                "progress": 3.0,
                "total": 10.0,
                "progressMessage": "halfway",
            }),
        );
    }

    #[test]
    fn mcp_progress_event_data_omits_absent_optional_fields() {
        // No `total` / `message` → those keys are omitted entirely (the TS
        // payload simply carries `undefined`, which serializes away).
        let ev = mcp::client::McpProgressEvent {
            progress: 1.0,
            total: None,
            message: None,
        };
        let out = mcp_progress_event_data("srv", "tool", &ev);
        assert_eq!(
            out,
            json!({
                "type": "mcp_progress",
                "status": "progress",
                "serverName": "srv",
                "toolName": "tool",
                "progress": 1.0,
            }),
        );
        let obj = out.as_object().expect("object");
        assert!(obj.get("total").is_none());
        assert!(obj.get("progressMessage").is_none());
    }

    // -- MCP.1: isError → tool error (first content block text lift) ----------

    #[test]
    fn first_content_block_text_lifts_first_text() {
        // Mirrors the TS `result.content[0].text` lookup (client.ts:3124-3142):
        // the FIRST block's text wins.
        let content = json!([
            { "type": "text", "text": "boom" },
            { "type": "text", "text": "second" },
        ]);
        assert_eq!(first_content_block_text(&content).as_deref(), Some("boom"));
    }

    #[test]
    fn first_content_block_text_none_for_non_text_shapes() {
        // A bare string, an empty array, or a first block without a string `text`
        // all yield None → the caller falls back to "Unknown error".
        assert!(first_content_block_text(&json!("a bare string")).is_none());
        assert!(first_content_block_text(&json!([])).is_none());
        assert!(first_content_block_text(&json!([{ "type": "image" }])).is_none());
        assert!(first_content_block_text(&json!([{ "text": 7 }])).is_none());
    }

    #[test]
    fn mcp_all_text_content_renders_model_facing_string() {
        // Single text block (the common MCP result) → exactly its text, matching
        // claude-code's single-block tool_result content (NOT a JSON dump).
        assert_eq!(
            mcp_all_text_content_to_string(&json!([{ "type": "text", "text": "result" }]))
                .as_deref(),
            Some("result")
        );
        // Multiple text blocks → newline-joined.
        assert_eq!(
            mcp_all_text_content_to_string(&json!([
                { "type": "text", "text": "a" },
                { "type": "text", "text": "b" },
            ]))
            .as_deref(),
            Some("a\nb")
        );
        // Bare string / empty / image-bearing / resource-bearing → None (left for
        // the `content` key or the block-array protocol change).
        assert!(mcp_all_text_content_to_string(&json!("bare")).is_none());
        assert!(mcp_all_text_content_to_string(&json!([])).is_none());
        assert!(mcp_all_text_content_to_string(
            &json!([{ "type": "text", "text": "a" }, { "type": "image", "source": {} }])
        )
        .is_none());
    }

    #[test]
    fn is_error_maps_to_io_error_with_first_text() {
        // The MCP.1 mapping: an isError:true result becomes ToolError::Io carrying
        // the first block's text (the orchestrator then prepends "Error: ").
        let content = json!([{ "type": "text", "text": "kaboom" }]);
        let details =
            first_content_block_text(&content).unwrap_or_else(|| "Unknown error".to_string());
        match ToolError::Io(details) {
            ToolError::Io(s) => assert_eq!(s, "kaboom"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn is_error_unknown_when_no_text() {
        let details =
            first_content_block_text(&json!([])).unwrap_or_else(|| "Unknown error".to_string());
        assert_eq!(details, "Unknown error");
    }

    // -- MCP.2: structuredContent priority → compact JSON string --------------

    #[test]
    fn structured_only_model_content_is_compact_json_string() {
        // MCP.2: when only structuredContent is present the model-facing content is
        // `serde_json::to_string(sc)` (compact JSON), run through the large-output
        // guard, which forwards under-threshold content verbatim. This is the exact
        // expression the Ok(dto) arm builds for the structured-content branch.
        let sc = json!({ "rows": [{ "id": 7 }], "total": 1 });
        let model_content = Value::String(serde_json::to_string(&sc).expect("serialize sc"));
        let out = crate::large_output::process_mcp_result(
            &model_content,
            "srv",
            "tool",
            &std::env::temp_dir(),
            0,
        );
        // The model sees the JSON string (NOT content:null, NOT pretty-printed).
        assert_eq!(out, json!(r#"{"rows":[{"id":7}],"total":1}"#));
    }

    #[test]
    fn output_schema_requires_structured_content() {
        let schema = json!({
            "type":"object",
            "properties":{"ok":{"type":"boolean"}},
            "required":["ok"],
            "additionalProperties":false
        });
        let err = validate_structured_content_against_output_schema(&schema, None).unwrap_err();
        assert!(err.contains("missing structuredContent"));
    }

    #[test]
    fn output_schema_mismatch_is_reported_as_tool_error_detail() {
        let schema = json!({
            "type":"object",
            "properties":{"ok":{"type":"boolean"}},
            "required":["ok"],
            "additionalProperties":false
        });
        let err =
            validate_structured_content_against_output_schema(&schema, Some(&json!({"ok":"nope"})))
                .unwrap_err();
        assert!(err.contains("/ok") || err.contains("expected"));
    }

    #[test]
    fn output_schema_required_does_not_need_properties() {
        let schema = json!({
            "type": "object",
            "required": ["token"],
            "additionalProperties": {"type": "string"}
        });
        let err = validate_structured_content_against_output_schema(
            &schema,
            Some(&json!({"other": "ok"})),
        )
        .unwrap_err();
        assert!(err.contains("missing required property \"token\""));
    }

    #[test]
    fn output_schema_validates_schema_valued_additional_properties_recursively() {
        let schema = json!({
            "type": "object",
            "properties": {"known": {"type": "boolean"}},
            "additionalProperties": {
                "type": "object",
                "additionalProperties": {"type": "integer"}
            }
        });
        let err = validate_structured_content_against_output_schema(
            &schema,
            Some(&json!({"known": true, "extra": {"count": "one"}})),
        )
        .unwrap_err();
        assert!(err.contains("/extra/count"));
    }

    #[tokio::test]
    async fn per_tool_permission_ceiling_tightens_tool_check() {
        let ctx = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            Arc::new(telemetry::AnalyticsBus::new()),
            vec![std::path::PathBuf::from("/tmp")],
        );
        let tool = MCPTool::new_for_tool(
            ctx,
            "mcp__srv__write".into(),
            "write".into(),
            json!({"type": "object"}),
            None,
            Some(McpPermissionCeiling::Ask),
            None,
            false,
            false,
        );
        assert!(matches!(
            tool.check_permissions(&json!({}), &tool_api::test_support::fresh_ctx())
                .await,
            PermissionResult::Ask { .. }
        ));

        let interaction_tool = MCPTool::new_for_tool(
            tool.ctx.clone(),
            "mcp__srv__interactive".into(),
            "interactive".into(),
            json!({"type": "object"}),
            None,
            None,
            None,
            false,
            true,
        );
        assert!(matches!(
            interaction_tool
                .check_permissions(&json!({}), &tool_api::test_support::fresh_ctx())
                .await,
            PermissionResult::Ask {
                reason: PermissionDecisionReason::PermissionPromptTool { tool_name },
                ..
            } if tool_name == "mcp__srv__interactive"
        ));

        let tool = MCPTool::new_for_tool(
            tool.ctx.clone(),
            "mcp__srv__blocked".into(),
            "blocked".into(),
            json!({"type": "object"}),
            None,
            Some(McpPermissionCeiling::Deny),
            None,
            false,
            false,
        );
        assert!(matches!(
            tool.check_permissions(&json!({}), &tool_api::test_support::fresh_ctx())
                .await,
            PermissionResult::Deny { .. }
        ));
    }
}

#[cfg(test)]
pub(crate) mod cached_resource_test_support {
    use super::*;
    use bytes::Bytes;
    use jsonrpc::{Connection, Mode};
    use mcp::{ConfigScope, McpConnectionState, McpServerConfig, RawConnectionProvider};
    use platform_api::{
        ElicitRequestDto, ElicitResultDto, McpError, McpNotificationStream, McpPromptDto,
        McpRawConnection, McpResourceContentDto, McpResourceDto, McpResourceTemplateDto,
        McpToolDto, McpToolResultDto, McpTransport, McpTransportKind, McpTransportSpec,
        ServerCapabilitiesDto,
    };
    use protocol::McpConnectionId;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    #[derive(Clone)]
    pub(crate) struct CachedServerBehavior {
        pub cached_capabilities: ServerCapabilitiesDto,
        pub live_capabilities: ServerCapabilitiesDto,
        pub resources: Vec<McpResourceDto>,
        pub read_contents: Vec<Value>,
        pub directory_entries: Vec<mcp::McpDirectoryEntry>,
        pub connect_error: Option<String>,
        pub resources_list_rpc_error: bool,
        pub directory_read_rpc_error: Option<(i32, String)>,
    }

    impl Default for CachedServerBehavior {
        fn default() -> Self {
            Self {
                cached_capabilities: resource_caps(false),
                live_capabilities: resource_caps(false),
                resources: Vec::new(),
                read_contents: Vec::new(),
                directory_entries: Vec::new(),
                connect_error: None,
                resources_list_rpc_error: false,
                directory_read_rpc_error: None,
            }
        }
    }

    pub(crate) struct CachedResourceTransport {
        behaviors: Mutex<HashMap<String, CachedServerBehavior>>,
        conns: Mutex<HashMap<McpConnectionId, Arc<Connection>>>,
        conn_to_server: Mutex<HashMap<McpConnectionId, String>>,
        connect_calls: Mutex<HashMap<String, usize>>,
        rpc_calls: Arc<Mutex<HashMap<(String, String), usize>>>,
    }

    impl CachedResourceTransport {
        #[must_use]
        pub(crate) fn new() -> Self {
            Self {
                behaviors: Mutex::new(HashMap::new()),
                conns: Mutex::new(HashMap::new()),
                conn_to_server: Mutex::new(HashMap::new()),
                connect_calls: Mutex::new(HashMap::new()),
                rpc_calls: Arc::new(Mutex::new(HashMap::new())),
            }
        }

        pub(crate) fn insert_server(&self, name: &str, behavior: CachedServerBehavior) {
            self.behaviors
                .lock()
                .unwrap()
                .insert(name.to_string(), behavior);
        }

        #[must_use]
        pub(crate) fn connect_calls(&self, name: &str) -> usize {
            *self.connect_calls.lock().unwrap().get(name).unwrap_or(&0)
        }

        #[must_use]
        pub(crate) fn rpc_call_count(&self, name: &str, method: &str) -> usize {
            *self
                .rpc_calls
                .lock()
                .unwrap()
                .get(&(name.to_string(), method.to_string()))
                .unwrap_or(&0)
        }
    }

    fn responding_connection(
        server_name: String,
        behavior: CachedServerBehavior,
        rpc_calls: Arc<Mutex<HashMap<(String, String), usize>>>,
    ) -> Arc<Connection> {
        let (peer_to_us_tx, peer_to_us_rx) = mpsc::channel::<Bytes>(8);
        let (us_to_peer_tx, mut us_to_peer_rx) = mpsc::channel::<Bytes>(8);
        let conn = Arc::new(Connection::new_streams(
            peer_to_us_rx,
            us_to_peer_tx,
            Mode::Lines,
        ));
        tokio::spawn(async move {
            while let Some(frame) = us_to_peer_rx.recv().await {
                let Ok(req) = serde_json::from_slice::<Value>(&frame) else {
                    continue;
                };
                let Some(id) = req.get("id").cloned() else {
                    continue;
                };
                let method = req.get("method").and_then(Value::as_str).unwrap_or("");
                *rpc_calls
                    .lock()
                    .unwrap()
                    .entry((server_name.clone(), method.to_string()))
                    .or_insert(0) += 1;
                let resp = match method {
                    "initialize" => {
                        let mut caps = serde_json::Map::new();
                        if behavior.live_capabilities.tools {
                            caps.insert("tools".into(), json!({}));
                        }
                        if behavior.live_capabilities.resources {
                            caps.insert("resources".into(), json!({}));
                        }
                        if behavior.live_capabilities.prompts {
                            caps.insert("prompts".into(), json!({}));
                        }
                        if behavior.live_capabilities.logging {
                            caps.insert("logging".into(), json!({}));
                        }
                        if behavior.live_capabilities.directory_read {
                            caps.insert(
                                "extensions".into(),
                                json!({
                                    crate::read_mcp_resource_dir::MCP_SKILLS_EXTENSION_KEY: {
                                        "directoryRead": true
                                    }
                                }),
                            );
                        }
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "protocolVersion": "2024-11-05",
                                "capabilities": Value::Object(caps),
                                "serverInfo": { "name": server_name, "version": "0" }
                            }
                        })
                    }
                    "resources/list" if behavior.resources_list_rpc_error => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32000, "message": "mock resources/list failure" }
                    }),
                    "resources/list" => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "resources": behavior
                                .resources
                                .iter()
                                .map(|resource| {
                                    let mut obj = serde_json::Map::new();
                                    obj.insert("uri".into(), json!(resource.uri));
                                    obj.insert("name".into(), json!(resource.name));
                                    if let Some(mime_type) = &resource.mime_type {
                                        obj.insert("mimeType".into(), json!(mime_type));
                                    }
                                    Value::Object(obj)
                                })
                                .collect::<Vec<_>>()
                        }
                    }),
                    "resources/read" => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "contents": behavior.read_contents }
                    }),
                    "resources/directory/read" => {
                        if let Some((code, message)) = &behavior.directory_read_rpc_error {
                            json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": { "code": code, "message": message }
                            })
                        } else {
                            json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": { "resources": behavior.directory_entries }
                            })
                        }
                    }
                    _ => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {}
                    }),
                };
                let mut bytes = serde_json::to_vec(&resp).unwrap();
                bytes.push(b'\n');
                if peer_to_us_tx.send(Bytes::from(bytes)).await.is_err() {
                    break;
                }
            }
        });
        conn
    }

    #[async_trait]
    impl McpTransport for CachedResourceTransport {
        async fn connect(&self, spec: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
            let McpTransportSpec::InProcess { registry_key } = spec else {
                return Err(McpError::Internal(
                    "test transport expects InProcess spec".into(),
                ));
            };
            let behavior = self
                .behaviors
                .lock()
                .unwrap()
                .get(registry_key)
                .cloned()
                .ok_or_else(|| McpError::Internal(format!("no behavior for {registry_key}")))?;
            *self
                .connect_calls
                .lock()
                .unwrap()
                .entry(registry_key.clone())
                .or_insert(0) += 1;
            if let Some(error) = &behavior.connect_error {
                return Err(McpError::Connection(error.clone()));
            }
            let conn_id = McpConnectionId::new();
            let conn =
                responding_connection(registry_key.clone(), behavior, self.rpc_calls.clone());
            self.conns.lock().unwrap().insert(conn_id, conn);
            self.conn_to_server
                .lock()
                .unwrap()
                .insert(conn_id, registry_key.clone());
            Ok(McpRawConnection {
                connection_id: conn_id,
            })
        }

        async fn initialize(
            &self,
            conn: &McpRawConnection,
        ) -> Result<ServerCapabilitiesDto, McpError> {
            let server = self
                .conn_to_server
                .lock()
                .unwrap()
                .get(&conn.connection_id)
                .cloned()
                .ok_or_else(|| McpError::Internal("missing conn->server mapping".into()))?;
            self.behaviors
                .lock()
                .unwrap()
                .get(&server)
                .map(|behavior| behavior.live_capabilities.clone())
                .ok_or_else(|| McpError::Internal(format!("no behavior for {server}")))
        }

        async fn list_tools(&self, _conn: &McpRawConnection) -> Result<Vec<McpToolDto>, McpError> {
            Ok(Vec::new())
        }

        async fn list_resources(
            &self,
            conn: &McpRawConnection,
        ) -> Result<Vec<McpResourceDto>, McpError> {
            let server = self
                .conn_to_server
                .lock()
                .unwrap()
                .get(&conn.connection_id)
                .cloned()
                .ok_or_else(|| McpError::Internal("missing conn->server mapping".into()))?;
            let behavior = self
                .behaviors
                .lock()
                .unwrap()
                .get(&server)
                .cloned()
                .ok_or_else(|| McpError::Internal(format!("no behavior for {server}")))?;
            *self
                .rpc_calls
                .lock()
                .unwrap()
                .entry((server.clone(), "transport::list_resources".to_string()))
                .or_insert(0) += 1;
            if behavior.resources_list_rpc_error {
                return Err(McpError::Internal("mock resources/list failure".into()));
            }
            Ok(behavior.resources)
        }

        async fn list_prompts(
            &self,
            _conn: &McpRawConnection,
        ) -> Result<Vec<McpPromptDto>, McpError> {
            Ok(Vec::new())
        }

        async fn call_tool(
            &self,
            _conn: &McpRawConnection,
            _tool: &str,
            _input: Value,
        ) -> Result<McpToolResultDto, McpError> {
            Ok(McpToolResultDto {
                content: json!("ok"),
                is_error: false,
                ..Default::default()
            })
        }

        async fn read_resource(
            &self,
            _conn: &McpRawConnection,
            _uri: &str,
        ) -> Result<McpResourceContentDto, McpError> {
            Err(McpError::Internal("not used in these tests".into()))
        }

        async fn ping(&self, _id: McpConnectionId) -> Result<(), McpError> {
            Ok(())
        }

        async fn notifications(
            &self,
            _conn: &McpRawConnection,
        ) -> Result<McpNotificationStream, McpError> {
            use futures::stream::empty;
            Ok(Box::pin(empty()))
        }

        async fn handle_elicitation(
            &self,
            _conn: &McpRawConnection,
            _request: ElicitRequestDto,
        ) -> Result<ElicitResultDto, McpError> {
            Err(McpError::Internal("not used in these tests".into()))
        }

        async fn disconnect(&self, conn_id: McpConnectionId) -> Result<(), McpError> {
            self.conns.lock().unwrap().remove(&conn_id);
            self.conn_to_server.lock().unwrap().remove(&conn_id);
            Ok(())
        }

        fn supported_transports(&self) -> Vec<McpTransportKind> {
            vec![McpTransportKind::InProcess]
        }
    }

    impl RawConnectionProvider for CachedResourceTransport {
        fn connection_for(&self, id: McpConnectionId) -> Option<Arc<Connection>> {
            self.conns.lock().unwrap().get(&id).cloned()
        }
    }

    #[must_use]
    pub(crate) fn cached_server_config(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.into(),
            spec: McpTransportSpec::InProcess {
                registry_key: name.into(),
            },
            scope: ConfigScope::Settings(protocol::SettingsScope::User),
            disabled: false,
            timeout_ms: None,
            always_load: false,
            discovery_cache: None,
            tools: Vec::new(),
            tool_permissions: std::collections::BTreeMap::new(),
            config_error: None,
            metadata: Default::default(),
        }
    }

    #[must_use]
    pub(crate) fn resource_caps(directory_read: bool) -> ServerCapabilitiesDto {
        ServerCapabilitiesDto {
            tools: false,
            resources: true,
            prompts: false,
            logging: false,
            directory_read,
            experimental: HashMap::new(),
            extensions: HashMap::new(),
        }
    }

    #[must_use]
    pub(crate) fn ctx_with_registry(registry: Arc<McpRegistry>) -> tool_api::BuiltinToolContext {
        let fs = tool_api::test_support::make_dummy_fs();
        let bus = Arc::new(telemetry::AnalyticsBus::new());
        let mut ctx =
            tool_api::test_support::ctx_for_file_tools(fs, bus, vec![std::env::temp_dir()]);
        ctx.mcp_registry = Some(registry);
        ctx
    }

    pub(crate) async fn new_cached_registry() -> (Arc<McpRegistry>, Arc<CachedResourceTransport>) {
        let transport = Arc::new(CachedResourceTransport::new());
        let registry = Arc::new(McpRegistry::with_raw_conn(
            transport.clone() as Arc<dyn McpTransport>,
            transport.clone() as Arc<dyn RawConnectionProvider>,
        ));
        (registry, transport)
    }

    pub(crate) async fn seed_cached_server(
        registry: &Arc<McpRegistry>,
        transport: &Arc<CachedResourceTransport>,
        name: &str,
        behavior: CachedServerBehavior,
    ) {
        transport.insert_server(name, behavior.clone());
        registry.connections.write().await.insert(
            name.into(),
            McpConnectionState::Cached {
                config: cached_server_config(name),
                connection_id: McpConnectionId::new(),
                capabilities: behavior.cached_capabilities,
                negotiated: platform_api::McpNegotiatedProtocol {
                    era: platform_api::McpProtocolEra::Legacy,
                    version: "2025-11-25".into(),
                },
                tools: vec![],
                resources: behavior.resources,
                resource_templates: Vec::<McpResourceTemplateDto>::new(),
                prompts: vec![],
                cache_saved_at_ms: 1,
                age_ms: 0,
            },
        );
    }

    pub(crate) async fn seed_connected_server(
        registry: &Arc<McpRegistry>,
        transport: &Arc<CachedResourceTransport>,
        name: &str,
        behavior: CachedServerBehavior,
    ) {
        transport.insert_server(name, behavior);
        registry
            .connect(cached_server_config(name))
            .await
            .expect("seed connected server");
    }
}

#[cfg(test)]
mod input_missing_required_preflight_tests {
    use super::*;
    use bytes::Bytes;
    use jsonrpc::{Connection, Mode};
    use platform_api::{
        ElicitRequestDto, ElicitResultDto, McpError, McpNotificationStream, McpPromptDto,
        McpRawConnection, McpResourceContentDto, McpResourceDto, McpToolDto, McpTransport,
        McpTransportKind, McpTransportSpec, ServerCapabilitiesDto,
    };
    use telemetry::InMemorySink;
    use tokio::sync::mpsc;

    struct StubTransport;

    #[async_trait]
    impl McpTransport for StubTransport {
        async fn connect(&self, _s: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
            unreachable!()
        }
        async fn initialize(
            &self,
            _c: &McpRawConnection,
        ) -> Result<ServerCapabilitiesDto, McpError> {
            unreachable!()
        }
        async fn list_tools(&self, _c: &McpRawConnection) -> Result<Vec<McpToolDto>, McpError> {
            unreachable!()
        }
        async fn list_resources(
            &self,
            _c: &McpRawConnection,
        ) -> Result<Vec<McpResourceDto>, McpError> {
            unreachable!()
        }
        async fn list_prompts(&self, _c: &McpRawConnection) -> Result<Vec<McpPromptDto>, McpError> {
            unreachable!()
        }
        async fn call_tool(
            &self,
            _c: &McpRawConnection,
            _t: &str,
            _i: Value,
        ) -> Result<platform_api::McpToolResultDto, McpError> {
            unreachable!()
        }
        async fn read_resource(
            &self,
            _c: &McpRawConnection,
            _u: &str,
        ) -> Result<McpResourceContentDto, McpError> {
            unreachable!()
        }
        async fn ping(&self, _id: protocol::McpConnectionId) -> Result<(), McpError> {
            unreachable!()
        }
        async fn notifications(
            &self,
            _c: &McpRawConnection,
        ) -> Result<McpNotificationStream, McpError> {
            unreachable!()
        }
        async fn handle_elicitation(
            &self,
            _c: &McpRawConnection,
            _r: ElicitRequestDto,
        ) -> Result<ElicitResultDto, McpError> {
            unreachable!()
        }
        async fn disconnect(&self, _id: protocol::McpConnectionId) -> Result<(), McpError> {
            unreachable!()
        }
        fn supported_transports(&self) -> Vec<McpTransportKind> {
            vec![McpTransportKind::Stdio]
        }
    }

    fn paired() -> (Arc<Connection>, mpsc::Sender<Bytes>, mpsc::Receiver<Bytes>) {
        let (peer_to_us_tx, peer_to_us_rx) = mpsc::channel::<Bytes>(8);
        let (us_to_peer_tx, us_to_peer_rx) = mpsc::channel::<Bytes>(8);
        let conn = Arc::new(Connection::new_streams(
            peer_to_us_rx,
            us_to_peer_tx,
            Mode::Lines,
        ));
        (conn, peer_to_us_tx, us_to_peer_rx)
    }

    #[test]
    fn inspect_missing_required_matches_oracle_summary_fields() {
        let schema = json!({
            "type": "object",
            "required": ["token", "body"],
            "properties": {
                "token": { "type": "string" },
                "body": { "type": "string" },
                "note": { "type": "string" }
            }
        });
        let input = json!({
            "token": "ok",
            "note": "payload </body>"
        });

        let preflight =
            inspect_missing_required_input(&input, Some(&schema)).expect("missing required");
        assert_eq!(preflight.required_count, 2);
        assert_eq!(preflight.missing_count, 1);
        assert_eq!(preflight.present_key_count, 2);
        assert_eq!(preflight.max_string_value_len, 15);
        assert!(preflight.has_pseudo_tag_debris);
        assert_eq!(preflight.missing_keys, vec!["body".to_string()]);
    }

    #[test]
    fn inspect_missing_required_uses_javascript_utf16_lengths() {
        let schema = json!({
            "type": "object",
            "required": ["token"],
            "properties": { "token": { "type": "string" } }
        });
        let input = json!({ "note": "😀😀" });
        let preflight = inspect_missing_required_input(&input, Some(&schema)).unwrap();
        assert_eq!(preflight.max_string_value_len, 4);
        assert_eq!(js_string_len(&serde_json::to_string(&input).unwrap()), 15);
    }

    #[tokio::test]
    async fn missing_required_preflight_emits_event_and_preserves_transport() {
        let (conn, peer_tx, mut peer_rx) = paired();
        let client =
            Arc::new(mcp::McpClient::new("slow", std::path::PathBuf::from("/tmp"), conn).await);
        let registry = Arc::new(McpRegistry::new(Arc::new(StubTransport)));
        registry.register_client("slow", client).await;

        let fs = tool_api::test_support::make_dummy_fs();
        let bus = Arc::new(telemetry::AnalyticsBus::new());
        let sink = Arc::new(InMemorySink::new());
        bus.attach_sink(sink.clone()).await;
        let mut ctx =
            tool_api::test_support::ctx_for_file_tools(fs, bus, vec![std::env::temp_dir()]);
        ctx.mcp_registry = Some(registry);

        let tool = MCPTool::new_for_tool(
            ctx,
            "mcp__slow__slowtool".to_string(),
            "slow tool".to_string(),
            json!({
                "type": "object",
                "required": ["token"],
                "properties": {
                    "token": { "type": "string" },
                    "note": { "type": "string" }
                }
            }),
            None,
            None,
            None,
            false,
            false,
        );

        let mut use_ctx = tool_api::test_support::fresh_ctx();
        use_ctx.tool_use_id = Some(protocol::ToolUseId::from("tu-missing"));
        let assistant_message_id = protocol::MessageId::new();
        use_ctx.assistant_message_id = Some(assistant_message_id);

        let responder = tokio::spawn(async move {
            let frame = peer_rx.recv().await.expect("request reaches MCP server");
            let request: Value = serde_json::from_slice(&frame).unwrap();
            assert_eq!(
                request["params"]["arguments"],
                json!({"note":"leftover </token>"})
            );
            let response = json!({"jsonrpc":"2.0","id":request["id"],"result":{"content":[{"type":"text","text":"accepted"}],"isError":false}});
            let mut bytes = serde_json::to_vec(&response).unwrap();
            bytes.push(b'\n');
            peer_tx.send(Bytes::from(bytes)).await.unwrap();
        });
        tool.call(
            json!({"note":"leftover </token>"}),
            use_ctx,
            tool_api::test_support::fresh_tx(),
        )
        .await
        .expect("server accepts foreign-schema input");
        responder.await.unwrap();

        let events = sink.events().await;
        let event = events
            .iter()
            .find(|event| event.name == tengu_mcp::INPUT_MISSING_REQUIRED)
            .expect("missing-required telemetry emitted");
        assert!(matches!(
            event.metadata.get("toolInputSizeBytes"),
            Some(AnalyticsValue::Int(value)) if *value > 0
        ));
        for (key, expected) in [
            ("requiredCount", 1),
            ("missingCount", 1),
            ("presentKeyCount", 1),
            ("maxStringValueLen", 17),
        ] {
            assert!(matches!(
                event.metadata.get(key),
                Some(AnalyticsValue::Int(value)) if *value == expected
            ));
        }
        assert!(matches!(
            event.metadata.get("hasPseudoTagDebris"),
            Some(AnalyticsValue::Bool(true))
        ));
        assert!(matches!(
            event.metadata.get("toolUseID"),
            Some(AnalyticsValue::String(value)) if value == "tu-missing"
        ));
        assert!(matches!(
            event.metadata.get("messageID"),
            Some(AnalyticsValue::String(value))
                if value == &assistant_message_id.as_uuid().to_string()
        ));
        assert!(matches!(
            event.metadata.get("toolName"),
            Some(AnalyticsValue::String(value)) if value == "mcp_tool"
        ));
        assert!(matches!(
            event.metadata.get("isMcp"),
            Some(AnalyticsValue::Bool(true))
        ));
        assert!(!event.metadata.contains_key("_PROTO_server_name"));
        assert!(!event.metadata.contains_key("_PROTO_tool_name"));
        assert!(
            events.iter().any(|event| event.name == MCP_STARTED),
            "missing-required telemetry must not prevent MCP_STARTED"
        );
    }

    #[test]
    fn foreign_ref_allof_and_conditional_schemas_are_advertisement_only() {
        let ctx = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            Arc::new(AnalyticsBus::new()),
            vec![std::env::temp_dir()],
        );
        let schema = json!({"type":"object","$defs":{"token":{"type":"string"}},"allOf":[{"properties":{"token":{"$ref":"#/$defs/token"}},"required":["token"]}],"if":{"required":["flag"]},"then":{"required":["other"]}});
        let tool = MCPTool::new_for_tool(
            ctx,
            "mcp__local__tool".into(),
            "local tool".into(),
            schema.clone(),
            None,
            None,
            None,
            false,
            false,
        );
        assert_eq!(tool.input_schema(), &schema);
        assert_eq!(tool.input_validation_schema(), &json!({"type":"object"}));
    }

    #[tokio::test]
    async fn schema_rejection_hook_emits_missing_required_before_call() {
        let bus = Arc::new(AnalyticsBus::new());
        let sink = Arc::new(InMemorySink::new());
        bus.attach_sink(sink.clone()).await;
        let ctx = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            bus,
            vec![std::env::temp_dir()],
        );
        let tool = MCPTool::new_for_tool(
            ctx,
            "mcp__slow__slowtool".to_string(),
            "slow tool".to_string(),
            json!({
                "type": "object",
                "required": ["token"],
                "properties": { "token": { "type": "string" } }
            }),
            None,
            None,
            None,
            false,
            false,
        );
        let assistant_message_id = protocol::MessageId::new();

        tool.on_input_schema_rejected(
            &json!({ "note": "😀" }),
            Some("tu-schema"),
            Some(&assistant_message_id),
        )
        .await;

        let events = sink.events().await;
        let event = events
            .iter()
            .find(|event| event.name == tengu_mcp::INPUT_MISSING_REQUIRED)
            .expect("schema-rejection hook emits missing-required telemetry");
        assert!(matches!(
            event.metadata.get("toolUseID"),
            Some(AnalyticsValue::String(value)) if value == "tu-schema"
        ));
        assert!(matches!(
            event.metadata.get("messageID"),
            Some(AnalyticsValue::String(value))
                if value == &assistant_message_id.as_uuid().to_string()
        ));
        assert!(matches!(
            event.metadata.get("toolInputSizeBytes"),
            Some(AnalyticsValue::Int(value)) if *value == 13
        ));
        assert!(!event.metadata.contains_key("_PROTO_server_name"));
        assert!(!event.metadata.contains_key("_PROTO_tool_name"));
    }

    #[tokio::test]
    async fn large_result_handled_event_uses_oracle_field_names() {
        let bus = Arc::new(AnalyticsBus::new());
        let sink = Arc::new(InMemorySink::new());
        bus.attach_sink(sink.clone()).await;

        emit_mcp_large_result_handled(
            &bus,
            LargeResultHandledPayload {
                outcome: Verified::assert_safe("persisted".to_string()),
                reason: Verified::assert_safe("file_saved".to_string()),
                size_estimate_tokens: 15_000,
                persisted_size_chars: Some(60_123),
                result_type: Some(Verified::assert_safe("contentArray".to_string())),
                block_count: Some(2),
                persisted_as: Some(Verified::assert_safe("json".to_string())),
            },
        )
        .await;

        let events = sink.events().await;
        let event = events
            .iter()
            .find(|event| event.name == tengu_mcp::LARGE_RESULT_HANDLED)
            .expect("large-result telemetry emitted");
        assert!(matches!(
            event.metadata.get("outcome"),
            Some(AnalyticsValue::String(value)) if value == "persisted"
        ));
        assert!(matches!(
            event.metadata.get("reason"),
            Some(AnalyticsValue::String(value)) if value == "file_saved"
        ));
        for (key, expected) in [
            ("sizeEstimateTokens", 15_000),
            ("persistedSizeChars", 60_123),
            ("blockCount", 2),
        ] {
            assert!(matches!(
                event.metadata.get(key),
                Some(AnalyticsValue::Int(value)) if *value == expected
            ));
        }
        assert!(matches!(
            event.metadata.get("resultType"),
            Some(AnalyticsValue::String(value)) if value == "contentArray"
        ));
        assert!(matches!(
            event.metadata.get("persistedAs"),
            Some(AnalyticsValue::String(value)) if value == "json"
        ));
        assert!(!event.metadata.contains_key("_PROTO_server_name"));
        assert!(!event.metadata.contains_key("_PROTO_tool_name"));
    }
}

// ===== G07/G08: MCP tool-call auto-background race integration ===============

#[cfg(test)]
mod auto_background_race_tests {
    use super::*;
    use bytes::Bytes;
    use jsonrpc::{Connection, Mode};
    use platform_api::task_registry::{
        McpTaskRegistration, TaskCreateInput, TaskListFilter, TaskRecord, TaskRegistryError,
        TaskRegistryHandle, TaskUpdatePatch,
    };
    use platform_api::{
        ElicitRequestDto, ElicitResultDto, McpError, McpNotificationStream, McpPromptDto,
        McpRawConnection, McpResourceContentDto, McpResourceDto, McpToolDto, McpTransport,
        McpTransportKind, McpTransportSpec, ServerCapabilitiesDto,
    };
    use std::sync::Mutex as StdMutex;
    use tokio::sync::mpsc;

    /// Minimal transport — the registry needs one to construct, but these tests
    /// drive a directly-`register_client`ed [`mcp::McpClient`], so nothing here
    /// is ever called.
    struct StubTransport;

    #[async_trait]
    impl McpTransport for StubTransport {
        async fn connect(&self, _s: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
            unreachable!()
        }
        async fn initialize(
            &self,
            _c: &McpRawConnection,
        ) -> Result<ServerCapabilitiesDto, McpError> {
            unreachable!()
        }
        async fn list_tools(&self, _c: &McpRawConnection) -> Result<Vec<McpToolDto>, McpError> {
            unreachable!()
        }
        async fn list_resources(
            &self,
            _c: &McpRawConnection,
        ) -> Result<Vec<McpResourceDto>, McpError> {
            unreachable!()
        }
        async fn list_prompts(&self, _c: &McpRawConnection) -> Result<Vec<McpPromptDto>, McpError> {
            unreachable!()
        }
        async fn call_tool(
            &self,
            _c: &McpRawConnection,
            _t: &str,
            _i: Value,
        ) -> Result<platform_api::McpToolResultDto, McpError> {
            unreachable!()
        }
        async fn read_resource(
            &self,
            _c: &McpRawConnection,
            _u: &str,
        ) -> Result<McpResourceContentDto, McpError> {
            unreachable!()
        }
        async fn ping(&self, _id: protocol::McpConnectionId) -> Result<(), McpError> {
            unreachable!()
        }
        async fn notifications(
            &self,
            _c: &McpRawConnection,
        ) -> Result<McpNotificationStream, McpError> {
            unreachable!()
        }
        async fn handle_elicitation(
            &self,
            _c: &McpRawConnection,
            _r: ElicitRequestDto,
        ) -> Result<ElicitResultDto, McpError> {
            unreachable!()
        }
        async fn disconnect(&self, _id: protocol::McpConnectionId) -> Result<(), McpError> {
            unreachable!()
        }
        fn supported_transports(&self) -> Vec<McpTransportKind> {
            vec![McpTransportKind::Stdio]
        }
    }

    /// A paired in-memory `jsonrpc::Connection`: `peer_tx` sends frames TO the
    /// client, `peer_rx` receives the frames the client emits (its requests).
    fn paired() -> (Arc<Connection>, mpsc::Sender<Bytes>, mpsc::Receiver<Bytes>) {
        let (peer_to_us_tx, peer_to_us_rx) = mpsc::channel::<Bytes>(8);
        let (us_to_peer_tx, us_to_peer_rx) = mpsc::channel::<Bytes>(8);
        let conn = Arc::new(Connection::new_streams(
            peer_to_us_rx,
            us_to_peer_tx,
            Mode::Lines,
        ));
        (conn, peer_to_us_tx, us_to_peer_rx)
    }

    /// Like [`paired`] but sets a large per-call router timeout so the jsonrpc
    /// layer's own 60s default does NOT fire before the 120s auto-background
    /// deadline under `start_paused` — the timeout race being tested is the
    /// tool's, not the transport's.
    fn paired_with_timeout(
        d: std::time::Duration,
    ) -> (Arc<Connection>, mpsc::Sender<Bytes>, mpsc::Receiver<Bytes>) {
        use futures::stream::unfold;
        let (peer_to_us_tx, peer_to_us_rx) = mpsc::channel::<Bytes>(8);
        let (us_to_peer_tx, us_to_peer_rx) = mpsc::channel::<Bytes>(8);
        let inbound = Box::pin(unfold(peer_to_us_rx, |mut rx| async move {
            rx.recv()
                .await
                .map(|v| (Ok::<Bytes, std::io::Error>(v), rx))
        }));
        let outbound = Box::pin(futures::sink::unfold(
            us_to_peer_tx.clone(),
            |tx, item: Bytes| async move {
                tx.send(item)
                    .await
                    .map_err(|_| std::io::Error::other("writer closed"))?;
                Ok::<_, std::io::Error>(tx)
            },
        ));
        let conn = Arc::new(
            Connection::builder(jsonrpc::LineCodec::default())
                .default_timeout(d)
                .build(inbound, outbound),
        );
        (conn, peer_to_us_tx, us_to_peer_rx)
    }

    /// Records every `register_mcp_task` call so a test can assert the timeout
    /// branch fired with the right server/tool. All other CRUD is unused here.
    #[derive(Default)]
    struct RecordingRegistry {
        registered: StdMutex<Vec<McpTaskRegistration>>,
        settled: StdMutex<Vec<(String, String, bool)>>,
        saved_hints: StdMutex<Vec<String>>,
        /// The cancel token handed to the most recent `register_mcp_task` — a
        /// test fires it to simulate a `TaskStop` (F3-2 regression).
        captured_cancel: StdMutex<Option<tokio_util::sync::CancellationToken>>,
    }

    #[async_trait]
    impl TaskRegistryHandle for RecordingRegistry {
        async fn create(&self, _i: TaskCreateInput) -> Result<TaskRecord, TaskRegistryError> {
            unreachable!()
        }
        async fn get(&self, _id: &str) -> Result<Option<TaskRecord>, TaskRegistryError> {
            Ok(None)
        }
        async fn list(&self, _f: TaskListFilter) -> Result<Vec<TaskRecord>, TaskRegistryError> {
            Ok(vec![])
        }
        async fn update(
            &self,
            _id: &str,
            _p: TaskUpdatePatch,
        ) -> Result<TaskRecord, TaskRegistryError> {
            unreachable!()
        }
        async fn set_status(&self, _id: &str, _s: &str) -> Result<TaskRecord, TaskRegistryError> {
            unreachable!()
        }
        async fn kill(&self, _id: &str) -> Result<TaskRecord, TaskRegistryError> {
            unreachable!()
        }
        async fn output(
            &self,
            _id: &str,
            _o: Option<u64>,
        ) -> Result<platform_api::task_registry::TaskOutputChunk, TaskRegistryError> {
            unreachable!()
        }
        async fn register_mcp_task(
            &self,
            reg: McpTaskRegistration,
            cancel: tokio_util::sync::CancellationToken,
        ) -> Result<String, TaskRegistryError> {
            self.registered.lock().unwrap().push(reg);
            *self.captured_cancel.lock().unwrap() = Some(cancel);
            Ok("ktest0001".to_string())
        }
        async fn settle_mcp_task_with_hint(
            &self,
            id: &str,
            text: &str,
            failed: bool,
            hint: Option<&str>,
        ) -> Result<bool, TaskRegistryError> {
            if let Some(hint) = hint {
                self.saved_hints.lock().unwrap().push(hint.to_string());
            }
            self.settle_mcp_task(id, text, failed).await
        }

        async fn settle_mcp_task(
            &self,
            id: &str,
            result_text: &str,
            failed: bool,
        ) -> Result<bool, TaskRegistryError> {
            self.settled
                .lock()
                .unwrap()
                .push((id.to_string(), result_text.to_string(), failed));
            // The mock always "wins" the terminal transition (it has no prior
            // state), so the caller emits the `mcp_auto_background` outcome — the
            // real registry returns `Ok(false)` for an already-terminal task.
            Ok(true)
        }
    }

    fn ctx_with(
        registry: Arc<McpRegistry>,
        task_registry: Option<Arc<dyn TaskRegistryHandle>>,
    ) -> tool_api::BuiltinToolContext {
        let fs = tool_api::test_support::make_dummy_fs();
        let bus = Arc::new(telemetry::AnalyticsBus::new());
        let mut ctx =
            tool_api::test_support::ctx_for_file_tools(fs, bus, vec![std::env::temp_dir()]);
        ctx.mcp_registry = Some(registry);
        ctx.task_registry = task_registry;
        ctx
    }

    fn call_input() -> Value {
        json!({ "full_name": "mcp__slow__slowtool", "arguments": {} })
    }

    // Drive the peer: read the client's `tools/call` request frame and answer it
    // with a canned text result so the awaiting call resolves.
    fn spawn_responder(peer_rx: mpsc::Receiver<Bytes>, peer_tx: mpsc::Sender<Bytes>) {
        spawn_tagged_responder(peer_rx, peer_tx, "ok");
    }

    /// Like [`spawn_responder`], but the canned `tools/call` result text is
    /// caller-supplied — lets a test with TWO live peers tell which one
    /// actually answered a given dispatch.
    fn spawn_tagged_responder(
        mut peer_rx: mpsc::Receiver<Bytes>,
        peer_tx: mpsc::Sender<Bytes>,
        text: &'static str,
    ) {
        tokio::spawn(async move {
            let frame = peer_rx.recv().await.expect("client sent a request frame");
            let req: Value = serde_json::from_slice(&frame).expect("json request");
            let id = req["id"].clone();
            let resp = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "content": [{ "type": "text", "text": text }], "isError": false },
            });
            let mut bytes = serde_json::to_vec(&resp).unwrap();
            bytes.push(b'\n');
            let _ = peer_tx.send(Bytes::from(bytes)).await;
        });
    }

    // A slow call (peer never responds) auto-backgrounds after the threshold:
    // registers an mcp_task and returns the byte-exact background message. Uses
    // `start_paused` so the tokio runtime auto-advances to the 120s auto-bg
    // timer once the in-flight call parks on the never-answered request.
    #[tokio::test(start_paused = true)]
    async fn slow_call_auto_backgrounds_after_threshold() {
        // Peer never responds; the router timeout is set well above the 120s
        // auto-background deadline so the tool's race — not the transport's —
        // decides the outcome.
        let (conn, _peer_tx, _peer_rx) = paired_with_timeout(std::time::Duration::from_secs(600));
        let client =
            Arc::new(mcp::McpClient::new("slow", std::path::PathBuf::from("/tmp"), conn).await);
        let registry = Arc::new(McpRegistry::new(Arc::new(StubTransport)));
        registry.register_client("slow", client).await;

        let recorder = Arc::new(RecordingRegistry::default());
        let ctx = ctx_with(
            registry,
            Some(recorder.clone() as Arc<dyn TaskRegistryHandle>),
        );
        let tool = MCPTool::new(ctx);

        let mut use_ctx = tool_api::test_support::fresh_ctx();
        use_ctx.tool_use_id = Some(protocol::ToolUseId::from("tu-slow"));
        use_ctx.agent_name = Some("builder".into());
        use_ctx.team_name = Some("alpha".into());
        let creator_agent_id = protocol::AgentId::new();
        use_ctx.agent_id = Some(creator_agent_id);
        // Interactive session → default 120000ms threshold (flag default on).

        let result = tool
            .call(call_input(), use_ctx, tool_api::test_support::fresh_tx())
            .await
            .expect("auto-background returns Ok(message), never an error");

        let text = result.model_content.expect("background message present");
        assert!(
            text.contains("It was moved to the background as task ktest0001"),
            "returns the byte-exact background message: {text}"
        );
        assert!(
            text.starts_with("MCP tool \"slow/slowtool\" is still running after"),
            "message names the server/tool: {text}"
        );

        let regd = recorder.registered.lock().unwrap();
        assert_eq!(regd.len(), 1, "exactly one mcp_task registered");
        assert_eq!(regd[0].server_name, "slow");
        assert_eq!(regd[0].tool_name, "slowtool");
        assert_eq!(regd[0].tool_use_id.as_deref(), Some("tu-slow"));
        assert_eq!(regd[0].creator_teammate_name.as_deref(), Some("builder"));
        assert_eq!(regd[0].creator_team_name.as_deref(), Some("alpha"));
        assert_eq!(regd[0].creator_agent_id, Some(creator_agent_id));
    }

    // Auto-background disabled (`auto_bg_ms == 0`, here via the non-interactive
    // gate with no CLAUDE_AUTO_BACKGROUND_TASKS opt-in) → the call awaits
    // directly and returns the real result; NOTHING is backgrounded.
    #[tokio::test]
    async fn disabled_threshold_awaits_directly() {
        let (conn, peer_tx, peer_rx) = paired();
        let client =
            Arc::new(mcp::McpClient::new("slow", std::path::PathBuf::from("/tmp"), conn).await);
        let registry = Arc::new(McpRegistry::new(Arc::new(StubTransport)));
        registry.register_client("slow", client).await;
        spawn_responder(peer_rx, peer_tx);

        let recorder = Arc::new(RecordingRegistry::default());
        let ctx = ctx_with(
            registry,
            Some(recorder.clone() as Arc<dyn TaskRegistryHandle>),
        );
        let tool = MCPTool::new(ctx);

        let mut use_ctx = tool_api::test_support::fresh_ctx();
        use_ctx.tool_use_id = Some(protocol::ToolUseId::from("tu-x"));
        // Non-interactive with no opt-in → getMcpAutoBackgroundMs == 0.
        use_ctx.options.is_non_interactive_session = true;

        let result = tool
            .call(call_input(), use_ctx, tool_api::test_support::fresh_tx())
            .await
            .expect("direct await returns the real result");
        assert_eq!(result.model_content.as_deref(), Some("ok"));
        assert!(
            recorder.registered.lock().unwrap().is_empty(),
            "auto_bg_ms == 0 never backgrounds"
        );
    }

    // §24b: `bound_server_key` must be the ONLY thing that decides which live
    // connection a per-tool wire entry dispatches to — `full_name` (the
    // model-facing FQN) stays the plain `mcp__docs__search` either way.
    #[tokio::test]
    async fn bound_server_key_dispatches_to_the_scoped_connection_not_the_plain_name() {
        // Two DIFFERENT live clients both registered under names that would
        // satisfy a naive "parse the server out of full_name" lookup for
        // "docs" — one under the PLAIN key (simulating an unrelated
        // shared/session "docs" server), one under an agent-scoped key
        // (simulating `McpRegistry::connect_agent_scoped`'s table key).
        let (plain_conn, plain_peer_tx, plain_peer_rx) = paired();
        let plain_client = Arc::new(
            mcp::McpClient::new("docs", std::path::PathBuf::from("/tmp"), plain_conn).await,
        );
        let (scoped_conn, scoped_peer_tx, scoped_peer_rx) = paired();
        let scoped_client = Arc::new(
            mcp::McpClient::new("docs", std::path::PathBuf::from("/tmp"), scoped_conn).await,
        );

        let registry = Arc::new(McpRegistry::new(Arc::new(StubTransport)));
        registry.register_client("docs", plain_client).await;
        let scoped_key = "__lingxi_agent_scope__deadbeef__docs";
        registry.register_client(scoped_key, scoped_client).await;

        // Each peer answers with a DISTINCT text, so the result proves WHICH
        // client actually received the call.
        spawn_tagged_responder(plain_peer_rx, plain_peer_tx, "plain-answered");
        spawn_tagged_responder(scoped_peer_rx, scoped_peer_tx, "scoped-answered");

        let ctx = ctx_with(registry, None);
        let tool = MCPTool::new_for_tool(
            ctx,
            "mcp__docs__search".to_string(),
            "search docs".to_string(),
            json!({"type": "object"}),
            None,
            None,
            None,
            false,
            false,
        )
        .with_bound_server_key(scoped_key.to_string());

        let mut use_ctx = tool_api::test_support::fresh_ctx();
        use_ctx.tool_use_id = Some(protocol::ToolUseId::from("tu-scoped"));

        let result = tool
            .call(json!({}), use_ctx, tool_api::test_support::fresh_tx())
            .await
            .expect("scoped dispatch succeeds");
        assert_eq!(
            result.model_content.as_deref(),
            Some("scoped-answered"),
            "bound_server_key must route dispatch to the SCOPED client, not a same-named plain one"
        );
    }

    // A per-tool entry with NO `bound_server_key` (every construction site
    // before §24b, and the shared/session per-tool entries built by
    // `build_registered_mcp_tools`) must keep dispatching by the plain name
    // parsed from `full_name` — byte-identical to legacy.
    #[tokio::test]
    async fn unbound_per_tool_entry_dispatches_by_the_plain_parsed_name() {
        let (conn, peer_tx, peer_rx) = paired();
        let client =
            Arc::new(mcp::McpClient::new("docs", std::path::PathBuf::from("/tmp"), conn).await);
        let registry = Arc::new(McpRegistry::new(Arc::new(StubTransport)));
        registry.register_client("docs", client).await;
        spawn_tagged_responder(peer_rx, peer_tx, "plain-answered");

        let ctx = ctx_with(registry, None);
        let tool = MCPTool::new_for_tool(
            ctx,
            "mcp__docs__search".to_string(),
            "search docs".to_string(),
            json!({"type": "object"}),
            None,
            None,
            None,
            false,
            false,
        );
        // No `.with_bound_server_key(..)` — legacy path.

        let mut use_ctx = tool_api::test_support::fresh_ctx();
        use_ctx.tool_use_id = Some(protocol::ToolUseId::from("tu-plain"));

        let result = tool
            .call(json!({}), use_ctx, tool_api::test_support::fresh_tx())
            .await
            .expect("unbound dispatch succeeds");
        assert_eq!(result.model_content.as_deref(), Some("plain-answered"));
    }

    // Threshold enabled but NO task registry wired on the context → the seam is
    // unavailable, so the call falls back to a direct await (no regression).
    #[tokio::test]
    async fn unwired_registry_awaits_directly() {
        let (conn, peer_tx, peer_rx) = paired();
        let client =
            Arc::new(mcp::McpClient::new("slow", std::path::PathBuf::from("/tmp"), conn).await);
        let registry = Arc::new(McpRegistry::new(Arc::new(StubTransport)));
        registry.register_client("slow", client).await;
        spawn_responder(peer_rx, peer_tx);

        // task_registry = None → unwired seam.
        let ctx = ctx_with(registry, None);
        let tool = MCPTool::new(ctx);

        let mut use_ctx = tool_api::test_support::fresh_ctx();
        use_ctx.tool_use_id = Some(protocol::ToolUseId::from("tu-y"));
        // Interactive → threshold WOULD be 120000, but the unwired seam forces
        // the direct-await path regardless.

        let result = tool
            .call(call_input(), use_ctx, tool_api::test_support::fresh_tx())
            .await
            .expect("unwired seam falls back to a direct await");
        assert_eq!(result.model_content.as_deref(), Some("ok"));
    }

    // F3-2: once a slow call auto-backgrounds, firing the mcp_task cancel token
    // (the port equivalent of `TaskStop`) must genuinely abort the in-flight
    // call — NONE of the completed-side-effects may run. Concretely: even if the
    // server LATER answers the (now abandoned) request, no MCP_COMPLETED
    // telemetry is emitted and the task is never settled-as-completed. Before
    // the fix the cancel token merely woke the settle-waiter, which DETACHED the
    // still-running call_task; the late server response then ran
    // `process_mcp_call_result`, emitting MCP_COMPLETED for a killed task.
    #[tokio::test(start_paused = true)]
    async fn cancel_after_background_suppresses_completed_side_effects() {
        use telemetry::InMemorySink;

        // Large router timeout so the tool's 120s auto-bg race, not the
        // transport's, decides the outcome.
        let (conn, peer_tx, mut peer_rx) = paired_with_timeout(std::time::Duration::from_secs(600));
        let client =
            Arc::new(mcp::McpClient::new("slow", std::path::PathBuf::from("/tmp"), conn).await);
        let registry = Arc::new(McpRegistry::new(Arc::new(StubTransport)));
        registry.register_client("slow", client).await;

        let recorder = Arc::new(RecordingRegistry::default());
        let ctx = ctx_with(
            registry,
            Some(recorder.clone() as Arc<dyn TaskRegistryHandle>),
        );
        let sink = Arc::new(InMemorySink::new());
        ctx.bus.attach_sink(sink.clone()).await;
        let tool = MCPTool::new(ctx);

        let mut use_ctx = tool_api::test_support::fresh_ctx();
        use_ctx.tool_use_id = Some(protocol::ToolUseId::from("tu-cancel"));

        // Auto-backgrounds (peer has not answered) and registers the mcp_task.
        let result = tool
            .call(call_input(), use_ctx, tool_api::test_support::fresh_tx())
            .await
            .expect("auto-background returns Ok(message)");
        assert!(
            result
                .model_content
                .as_deref()
                .is_some_and(|t| t.contains("moved to the background as task ktest0001")),
            "call auto-backgrounded"
        );

        // Simulate `TaskStop`: fire the token the registry captured.
        let cancel = recorder
            .captured_cancel
            .lock()
            .unwrap()
            .clone()
            .expect("register_mcp_task captured a cancel token");
        cancel.cancel();

        // The server now answers the abandoned request. A NON-cancelled call
        // would resolve here and run the completed-side-effects; the cancelled
        // call must ignore it.
        let frame = peer_rx.recv().await.expect("client sent a request frame");
        let req: Value = serde_json::from_slice(&frame).expect("json request");
        let id = req["id"].clone();
        let resp = json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "content": [{ "type": "text", "text": "ok" }], "isError": false },
        });
        let mut bytes = serde_json::to_vec(&resp).unwrap();
        bytes.push(b'\n');
        let _ = peer_tx.send(Bytes::from(bytes)).await;

        // Let the detached call_task + settle-waiter observe the cancel/response.
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }

        let names: Vec<String> = sink.events().await.into_iter().map(|e| e.name).collect();
        assert!(
            names.iter().any(|n| n == MCP_STARTED),
            "sanity: STARTED telemetry captured: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n == MCP_COMPLETED),
            "cancelled call must NOT emit COMPLETED telemetry: {names:?}"
        );
        assert!(
            recorder.settled.lock().unwrap().is_empty(),
            "cancelled call must NOT be settled-as-completed"
        );
        // F3-3: a cancelled (never-settled) call must NOT emit the
        // `mcp_auto_background` outcome counter (the binary's `!k` guard).
        assert!(
            !names
                .iter()
                .any(|n| n == TENGU_FEATURE_OK || n == TENGU_FEATURE_SAD || n == TENGU_FEATURE_BAD),
            "cancelled call must NOT emit an mcp_auto_background outcome: {names:?}"
        );
    }

    // F3-3: `emit_auto_background_outcome` maps each terminal outcome to the
    // binary's `ve`/`Ue`/`me` feature counter with the exact wire event name +
    // `feature_name` / `error_code` payload.
    #[tokio::test]
    async fn auto_background_outcome_events_map_to_feature_counters() {
        use telemetry::InMemorySink;

        for (outcome, event, error_code) in [
            (AutoBackgroundOutcome::Completed, TENGU_FEATURE_OK, None),
            (
                AutoBackgroundOutcome::ToolError,
                TENGU_FEATURE_SAD,
                Some("tool_error"),
            ),
            (
                AutoBackgroundOutcome::CallFailed,
                TENGU_FEATURE_BAD,
                Some("call_failed"),
            ),
        ] {
            let bus = Arc::new(AnalyticsBus::new());
            let sink = Arc::new(InMemorySink::new());
            bus.attach_sink(sink.clone()).await;

            emit_auto_background_outcome(&bus, outcome).await;

            let events = sink.events().await;
            let ev = events
                .iter()
                .find(|e| e.name == event)
                .unwrap_or_else(|| panic!("{event} emitted for {outcome:?}: {events:?}"));
            assert!(
                matches!(
                    ev.metadata.get("feature_name"),
                    Some(AnalyticsValue::String(s)) if s == MCP_AUTO_BACKGROUND_FEATURE
                ),
                "feature_name==mcp_auto_background for {outcome:?}"
            );
            match error_code {
                Some(code) => assert!(
                    matches!(
                        ev.metadata.get("error_code"),
                        Some(AnalyticsValue::String(s)) if s == code
                    ),
                    "error_code=={code} for {outcome:?}"
                ),
                // `ve("mcp_auto_background")` (completed) carries NO error_code.
                None => assert!(
                    ev.metadata.get("error_code").is_none(),
                    "completed outcome carries no error_code"
                ),
            }
        }
    }

    // Drive the peer: read the client's `tools/call` request frame and answer it
    // with the given `result`/`error` JSON-RPC response object.
    async fn answer_call(
        peer_rx: &mut mpsc::Receiver<Bytes>,
        peer_tx: &mpsc::Sender<Bytes>,
        response: Value,
    ) {
        let frame = peer_rx.recv().await.expect("client sent a request frame");
        let req: Value = serde_json::from_slice(&frame).expect("json request");
        let mut resp = json!({ "jsonrpc": "2.0", "id": req["id"].clone() });
        for (k, v) in response.as_object().expect("response object") {
            resp[k] = v.clone();
        }
        let mut bytes = serde_json::to_vec(&resp).unwrap();
        bytes.push(b'\n');
        let _ = peer_tx.send(Bytes::from(bytes)).await;
    }

    // F3-3: a backgrounded call that later RESOLVES (isError:false) settles the
    // mcp_task and emits `ve("mcp_auto_background")` → `tengu_feature_ok`.
    #[tokio::test(start_paused = true)]
    async fn backgrounded_call_completed_emits_feature_ok() {
        use telemetry::InMemorySink;

        let (conn, peer_tx, mut peer_rx) = paired_with_timeout(std::time::Duration::from_secs(600));
        let client =
            Arc::new(mcp::McpClient::new("slow", std::path::PathBuf::from("/tmp"), conn).await);
        let registry = Arc::new(McpRegistry::new(Arc::new(StubTransport)));
        registry.register_client("slow", client).await;

        let recorder = Arc::new(RecordingRegistry::default());
        let ctx = ctx_with(
            registry,
            Some(recorder.clone() as Arc<dyn TaskRegistryHandle>),
        );
        let sink = Arc::new(InMemorySink::new());
        ctx.bus.attach_sink(sink.clone()).await;
        let tool = MCPTool::new(ctx);

        let mut use_ctx = tool_api::test_support::fresh_ctx();
        use_ctx.tool_use_id = Some(protocol::ToolUseId::from("tu-ok"));

        let result = tool
            .call(call_input(), use_ctx, tool_api::test_support::fresh_tx())
            .await
            .expect("auto-background returns Ok(message)");
        assert!(result
            .model_content
            .as_deref()
            .is_some_and(|t| t.contains("moved to the background as task ktest0001")));

        // The call now RESOLVES successfully; the settle-waiter settles + emits.
        answer_call(
            &mut peer_rx,
            &peer_tx,
            json!({ "result": { "content": [{ "type": "text", "text": "done" }], "isError": false } }),
        )
        .await;
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }

        assert_eq!(
            recorder.settled.lock().unwrap().len(),
            1,
            "resolved call settled the mcp_task"
        );
        let events = sink.events().await;
        let ev = events
            .iter()
            .find(|e| e.name == TENGU_FEATURE_OK)
            .unwrap_or_else(|| panic!("tengu_feature_ok emitted: {events:?}"));
        assert!(matches!(
            ev.metadata.get("feature_name"),
            Some(AnalyticsValue::String(s)) if s == MCP_AUTO_BACKGROUND_FEATURE
        ));
        assert!(
            !events
                .iter()
                .any(|e| e.name == TENGU_FEATURE_SAD || e.name == TENGU_FEATURE_BAD),
            "completed path emits ONLY feature_ok: {events:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn backgrounded_call_produces_saved_hint_from_raw_persisted_result() {
        let (conn, peer_tx, mut peer_rx) = paired_with_timeout(std::time::Duration::from_secs(600));
        let client =
            Arc::new(mcp::McpClient::new("slow", std::path::PathBuf::from("/tmp"), conn).await);
        let registry = Arc::new(McpRegistry::new(Arc::new(StubTransport)));
        registry.register_client("slow", client).await;
        let recorder = Arc::new(RecordingRegistry::default());
        let ctx = ctx_with(registry, Some(recorder.clone()));
        let output_dir = ctx.tool_results_dir();
        let tool = MCPTool::new(ctx);
        let mut use_ctx = tool_api::test_support::fresh_ctx();
        use_ctx.tool_use_id = Some(protocol::ToolUseId::from("tu-saved"));
        tool.call(call_input(), use_ctx, tool_api::test_support::fresh_tx())
            .await
            .unwrap();
        let raw = "&".repeat(30_000); // Under generic token threshold; over escaped notification budget.
        answer_call(
            &mut peer_rx,
            &peer_tx,
            json!({"result":{"content":[{"type":"text","text":raw}],"isError":false}}),
        )
        .await;
        for _ in 0..200 {
            if !recorder.settled.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        let hints = recorder.saved_hints.lock().unwrap();
        assert_eq!(hints.len(), 1);
        assert!(hints[0].contains("complete 30000-character output was saved"));
        let path = hints[0]
            .split("output was saved to ")
            .nth(1)
            .unwrap()
            .split(';')
            .next()
            .unwrap();
        assert!(std::path::Path::new(path).starts_with(&output_dir));
        assert_eq!(std::fs::read_to_string(path).unwrap(), raw);
        assert!(recorder.settled.lock().unwrap()[0]
            .1
            .ends_with("… [truncated]"));
        std::fs::remove_file(path).unwrap();
    }

    // F3-3: a backgrounded call whose result carries `isError:true` settles the
    // mcp_task as failed and emits `Ue("mcp_auto_background","tool_error")` →
    // `tengu_feature_sad` (a recognized MCP failure, `BZu` true).
    #[tokio::test(start_paused = true)]
    async fn backgrounded_call_tool_error_emits_feature_sad() {
        use telemetry::InMemorySink;

        let (conn, peer_tx, mut peer_rx) = paired_with_timeout(std::time::Duration::from_secs(600));
        let client =
            Arc::new(mcp::McpClient::new("slow", std::path::PathBuf::from("/tmp"), conn).await);
        let registry = Arc::new(McpRegistry::new(Arc::new(StubTransport)));
        registry.register_client("slow", client).await;

        let recorder = Arc::new(RecordingRegistry::default());
        let ctx = ctx_with(
            registry,
            Some(recorder.clone() as Arc<dyn TaskRegistryHandle>),
        );
        let sink = Arc::new(InMemorySink::new());
        ctx.bus.attach_sink(sink.clone()).await;
        let tool = MCPTool::new(ctx);

        let mut use_ctx = tool_api::test_support::fresh_ctx();
        use_ctx.tool_use_id = Some(protocol::ToolUseId::from("tu-toolerr"));

        let result = tool
            .call(call_input(), use_ctx, tool_api::test_support::fresh_tx())
            .await
            .expect("auto-background returns Ok(message)");
        assert!(result
            .model_content
            .as_deref()
            .is_some_and(|t| t.contains("moved to the background as task ktest0001")));

        // The call resolves with an `isError:true` tool result → tool_error.
        answer_call(
            &mut peer_rx,
            &peer_tx,
            json!({ "result": { "content": [{ "type": "text", "text": "boom" }], "isError": true } }),
        )
        .await;
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }

        let settled = recorder.settled.lock().unwrap();
        assert_eq!(settled.len(), 1, "errored call settled the mcp_task");
        assert!(settled[0].2, "settled as failed");
        drop(settled);

        let events = sink.events().await;
        let ev = events
            .iter()
            .find(|e| e.name == TENGU_FEATURE_SAD)
            .unwrap_or_else(|| panic!("tengu_feature_sad emitted: {events:?}"));
        assert!(matches!(
            ev.metadata.get("feature_name"),
            Some(AnalyticsValue::String(s)) if s == MCP_AUTO_BACKGROUND_FEATURE
        ));
        assert!(matches!(
            ev.metadata.get("error_code"),
            Some(AnalyticsValue::String(s)) if s == "tool_error"
        ));
        assert!(
            !events
                .iter()
                .any(|e| e.name == TENGU_FEATURE_OK || e.name == TENGU_FEATURE_BAD),
            "tool_error path emits ONLY feature_sad: {events:?}"
        );
    }
}

// ===== TR-02: MCP-resource-tool gating ======================================

/// 2.1.238 `getTools` (`iJ`) strips `ListMcpResourcesTool` /
/// `ReadMcpResourceTool` / `ReadMcpResourceDirTool` out of the base tool list by
/// name and MCP discovery pushes them back only when a CONNECTED server declares
/// `capabilities.resources`. These tests seed the registry's `pub connections`
/// state map directly — the transport is never dialled — so the assertion is on
/// the push DECISION, which is the ported logic.
#[cfg(test)]
mod resource_tool_gating_tests {
    use super::*;
    use mcp::{ConfigScope, McpConnectionState, McpServerConfig};
    use platform_api::{
        ElicitRequestDto, ElicitResultDto, McpConfiguredToolPolicyDto, McpError,
        McpNotificationStream, McpPermissionCeiling, McpPromptDto, McpRawConnection,
        McpResourceContentDto, McpResourceDto, McpToolDto, McpTransport, McpTransportKind,
        McpTransportSpec, ServerCapabilitiesDto,
    };

    struct NeverDialled;

    #[async_trait]
    impl McpTransport for NeverDialled {
        async fn connect(&self, _s: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
            unreachable!()
        }
        async fn initialize(
            &self,
            _c: &McpRawConnection,
        ) -> Result<ServerCapabilitiesDto, McpError> {
            unreachable!()
        }
        async fn list_tools(&self, _c: &McpRawConnection) -> Result<Vec<McpToolDto>, McpError> {
            unreachable!()
        }
        async fn list_resources(
            &self,
            _c: &McpRawConnection,
        ) -> Result<Vec<McpResourceDto>, McpError> {
            unreachable!()
        }
        async fn list_prompts(&self, _c: &McpRawConnection) -> Result<Vec<McpPromptDto>, McpError> {
            unreachable!()
        }
        async fn call_tool(
            &self,
            _c: &McpRawConnection,
            _t: &str,
            _i: Value,
        ) -> Result<platform_api::McpToolResultDto, McpError> {
            unreachable!()
        }
        async fn read_resource(
            &self,
            _c: &McpRawConnection,
            _u: &str,
        ) -> Result<McpResourceContentDto, McpError> {
            unreachable!()
        }
        async fn ping(&self, _id: protocol::McpConnectionId) -> Result<(), McpError> {
            unreachable!()
        }
        async fn notifications(
            &self,
            _c: &McpRawConnection,
        ) -> Result<McpNotificationStream, McpError> {
            unreachable!()
        }
        async fn handle_elicitation(
            &self,
            _c: &McpRawConnection,
            _r: ElicitRequestDto,
        ) -> Result<ElicitResultDto, McpError> {
            unreachable!()
        }
        async fn disconnect(&self, _id: protocol::McpConnectionId) -> Result<(), McpError> {
            unreachable!()
        }
        fn supported_transports(&self) -> Vec<McpTransportKind> {
            vec![McpTransportKind::Stdio]
        }
    }

    fn config(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.into(),
            spec: McpTransportSpec::InProcess {
                registry_key: name.into(),
            },
            scope: ConfigScope::Settings(protocol::SettingsScope::User),
            disabled: false,
            timeout_ms: None,
            always_load: false,
            discovery_cache: None,
            tools: Vec::new(),
            tool_permissions: std::collections::BTreeMap::new(),
            config_error: None,
            metadata: Default::default(),
        }
    }

    fn caps(resources: bool) -> ServerCapabilitiesDto {
        ServerCapabilitiesDto {
            tools: true,
            resources,
            prompts: false,
            logging: false,
            directory_read: false,
            experimental: std::collections::HashMap::new(),
            extensions: std::collections::HashMap::new(),
        }
    }

    fn dto(server_name: &str, tool_name: &str) -> McpToolDto {
        McpToolDto {
            server_name: server_name.into(),
            tool_name: tool_name.into(),
            description: format!("{tool_name} tool"),
            input_schema: serde_json::json!({"type":"object"}),
            output_schema: None,
            annotations: None,
            icons: Vec::new(),
            meta: None,
            full_name: format!(
                "mcp__{}__{}",
                mcp::normalization::normalize_name_for_mcp(server_name),
                tool_name
            ),
            search_hint: None,
            always_load: None,
            requires_user_interaction: false,
        }
    }

    #[tokio::test]
    async fn shared_builder_applies_strictest_configured_permission_ceiling() {
        let registry = Arc::new(McpRegistry::new(Arc::new(NeverDialled)));
        let connection_id = protocol::McpConnectionId::new();
        let mut server_config = config("srv");
        server_config.tools = vec![
            McpConfiguredToolPolicyDto {
                name: "allow".into(),
                permission_policy: None,
                org_max_permission: Some(McpPermissionCeiling::Allow),
            },
            McpConfiguredToolPolicyDto {
                name: "ask".into(),
                permission_policy: None,
                org_max_permission: Some(McpPermissionCeiling::Ask),
            },
            McpConfiguredToolPolicyDto {
                name: "deny".into(),
                permission_policy: None,
                org_max_permission: Some(McpPermissionCeiling::Ask),
            },
        ];
        server_config.tool_permissions = std::collections::BTreeMap::from([
            ("allow".into(), McpPermissionCeiling::Allow),
            ("ask".into(), McpPermissionCeiling::Allow),
            ("deny".into(), McpPermissionCeiling::Deny),
        ]);
        registry.connections.write().await.insert(
            "srv".into(),
            McpConnectionState::Connected {
                config: server_config,
                connection_id,
                capabilities: caps(false),
                negotiated: platform_api::McpNegotiatedProtocol {
                    era: platform_api::McpProtocolEra::Legacy,
                    version: "2025-11-25".into(),
                },
                tools: vec![dto("srv", "allow"), dto("srv", "ask"), dto("srv", "deny")],
                resources: vec![],
                resource_templates: vec![],
                prompts: vec![],
                connected_at: std::time::SystemTime::now(),
            },
        );

        let mut ctx = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            Arc::new(telemetry::AnalyticsBus::new()),
            vec![std::path::PathBuf::from("/tmp")],
        );
        ctx.mcp_registry = Some(registry.clone());
        let built = build_registered_mcp_tools(&registry, ctx).await;
        assert_eq!(built.len(), 1);
        let registered = &built[0].1;
        assert_eq!(registered.len(), 3);

        assert!(matches!(
            registered[0]
                .check_permissions(&serde_json::json!({}), &tool_api::test_support::fresh_ctx())
                .await,
            PermissionResult::Allow { .. }
        ));
        assert!(matches!(
            registered[1]
                .check_permissions(&serde_json::json!({}), &tool_api::test_support::fresh_ctx())
                .await,
            PermissionResult::Ask { .. }
        ));
        assert!(matches!(
            registered[2]
                .check_permissions(&serde_json::json!({}), &tool_api::test_support::fresh_ctx())
                .await,
            PermissionResult::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn shared_builder_preserves_output_schema_and_retrieval_hints() {
        let registry = Arc::new(McpRegistry::new(Arc::new(NeverDialled)));
        let connection_id = protocol::McpConnectionId::new();
        let mut server_config = config("srv");
        server_config.always_load = true;
        let mut advertised = dto("srv", "structured");
        let output_schema = serde_json::json!({
            "type": "object",
            "required": ["ok"],
            "properties": { "ok": { "type": "boolean" } }
        });
        advertised.output_schema = Some(output_schema.clone());
        advertised.search_hint = Some("structured lookup".into());
        // Server-level alwaysLoad forces the tool on even when its own
        // metadata explicitly says false.
        advertised.always_load = Some(false);
        registry.connections.write().await.insert(
            "srv".into(),
            McpConnectionState::Connected {
                config: server_config,
                connection_id,
                capabilities: caps(false),
                negotiated: platform_api::McpNegotiatedProtocol {
                    era: platform_api::McpProtocolEra::Legacy,
                    version: "2025-11-25".into(),
                },
                tools: vec![advertised],
                resources: vec![],
                resource_templates: vec![],
                prompts: vec![],
                connected_at: std::time::SystemTime::now(),
            },
        );

        let mut ctx = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            Arc::new(telemetry::AnalyticsBus::new()),
            vec![std::path::PathBuf::from("/tmp")],
        );
        ctx.mcp_registry = Some(registry.clone());
        let built = build_registered_mcp_tools(&registry, ctx).await;
        let tool = &built[0].1[0];
        assert_eq!(tool.output_schema(), Some(&output_schema));
        assert_eq!(tool.search_hint(), Some("structured lookup"));
        assert!(tool.always_load());
        assert!(!tool.should_defer());
    }

    #[tokio::test]
    async fn generic_dispatcher_resolves_discovered_metadata_before_allowing_call() {
        let registry = Arc::new(McpRegistry::new(Arc::new(NeverDialled)));
        let connection_id = protocol::McpConnectionId::new();
        let mut server_config = config("srv");
        server_config.tools = vec![
            McpConfiguredToolPolicyDto {
                name: "write".into(),
                permission_policy: Some(platform_api::McpToolPermissionPolicy::AlwaysAllow),
                org_max_permission: Some(McpPermissionCeiling::Ask),
            },
            McpConfiguredToolPolicyDto {
                name: "interactive".into(),
                permission_policy: None,
                org_max_permission: None,
            },
        ];
        server_config.tool_permissions =
            std::collections::BTreeMap::from([("write".into(), McpPermissionCeiling::Deny)]);
        let mut interactive = dto("srv", "interactive");
        interactive.requires_user_interaction = true;
        registry.connections.write().await.insert(
            "srv".into(),
            McpConnectionState::Connected {
                config: server_config,
                connection_id,
                capabilities: caps(false),
                negotiated: platform_api::McpNegotiatedProtocol {
                    era: platform_api::McpProtocolEra::Legacy,
                    version: "2025-11-25".into(),
                },
                tools: vec![dto("srv", "write"), interactive],
                resources: vec![],
                resource_templates: vec![],
                prompts: vec![],
                connected_at: std::time::SystemTime::now(),
            },
        );
        let mut ctx = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            Arc::new(telemetry::AnalyticsBus::new()),
            vec![std::path::PathBuf::from("/tmp")],
        );
        ctx.mcp_registry = Some(registry);
        let dispatcher = MCPTool::new(ctx);

        let denied = dispatcher
            .check_permissions(
                &serde_json::json!({
                    "full_name": "mcp__srv__write",
                    "arguments": {}
                }),
                &tool_api::test_support::fresh_ctx(),
            )
            .await;
        assert!(matches!(denied, PermissionResult::Deny { .. }));

        let interactive = dispatcher
            .check_permissions(
                &serde_json::json!({
                    "full_name": "mcp__srv__interactive",
                    "arguments": {}
                }),
                &tool_api::test_support::fresh_ctx(),
            )
            .await;
        assert!(matches!(
            interactive,
            PermissionResult::Ask {
                reason: PermissionDecisionReason::PermissionPromptTool { ref tool_name },
                ..
            } if tool_name == "mcp__srv__interactive"
        ));

        let unknown = dispatcher
            .check_permissions(
                &serde_json::json!({
                    "full_name": "mcp__srv__not_advertised",
                    "arguments": {}
                }),
                &tool_api::test_support::fresh_ctx(),
            )
            .await;
        assert!(matches!(unknown, PermissionResult::Deny { .. }));
    }

    async fn build_names_by_connection(
        registry: &Arc<McpRegistry>,
    ) -> Vec<(protocol::McpConnectionId, Vec<String>)> {
        let mut ctx = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            Arc::new(telemetry::AnalyticsBus::new()),
            vec![std::path::PathBuf::from("/tmp")],
        );
        ctx.mcp_registry = Some(registry.clone());
        build_registered_mcp_tools(registry, ctx)
            .await
            .into_iter()
            .map(|(id, tools)| {
                (
                    id,
                    tools
                        .into_iter()
                        .map(|tool| tool.name().to_string())
                        .collect(),
                )
            })
            .collect()
    }

    /// Seed one `Connected` server whose `capabilities.resources` is `resources`
    /// and return every tool name `build_registered_mcp_tools` produced.
    async fn names_for(resources: bool) -> Vec<String> {
        let registry = Arc::new(McpRegistry::new(Arc::new(NeverDialled)));
        registry.connections.write().await.insert(
            "srv".into(),
            McpConnectionState::Connected {
                config: config("srv"),
                connection_id: protocol::McpConnectionId::new(),
                capabilities: caps(resources),
                negotiated: platform_api::McpNegotiatedProtocol {
                    era: platform_api::McpProtocolEra::Legacy,
                    version: "2025-11-25".into(),
                },
                tools: vec![],
                resources: vec![],
                resource_templates: vec![],
                prompts: vec![],
                connected_at: std::time::SystemTime::now(),
            },
        );
        let mut ctx = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            Arc::new(telemetry::AnalyticsBus::new()),
            vec![std::path::PathBuf::from("/tmp")],
        );
        ctx.mcp_registry = Some(registry.clone());
        build_registered_mcp_tools(&registry, ctx)
            .await
            .into_iter()
            .flat_map(|(_, ts)| ts.into_iter().map(|t| t.name().to_string()))
            .collect()
    }

    #[tokio::test]
    async fn per_tool_mcp_role_survives_connected_and_cached_rebuilds() {
        let registry = Arc::new(McpRegistry::new(Arc::new(NeverDialled)));
        let mut connected_config = config("connected");
        connected_config.metadata.role = Some(mcp::McpServerRole::Comms);
        let mut cached_config = config("cached");
        cached_config.metadata.role = Some(mcp::McpServerRole::Comms);
        registry.connections.write().await.insert(
            "connected".into(),
            McpConnectionState::Connected {
                config: connected_config,
                connection_id: protocol::McpConnectionId::new(),
                capabilities: caps(false),
                negotiated: platform_api::McpNegotiatedProtocol {
                    era: platform_api::McpProtocolEra::Legacy,
                    version: "2025-11-25".into(),
                },
                tools: vec![dto("connected", "send")],
                resources: vec![],
                resource_templates: vec![],
                prompts: vec![],
                connected_at: std::time::SystemTime::now(),
            },
        );
        registry.connections.write().await.insert(
            "cached".into(),
            McpConnectionState::Cached {
                config: cached_config,
                connection_id: protocol::McpConnectionId::new(),
                capabilities: caps(false),
                negotiated: platform_api::McpNegotiatedProtocol {
                    era: platform_api::McpProtocolEra::Legacy,
                    version: "2025-11-25".into(),
                },
                tools: vec![dto("cached", "send")],
                resources: vec![],
                resource_templates: vec![],
                prompts: vec![],
                cache_saved_at_ms: 1,
                age_ms: 0,
            },
        );

        let mut ctx = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            Arc::new(telemetry::AnalyticsBus::new()),
            vec![std::path::PathBuf::from("/tmp")],
        );
        ctx.mcp_registry = Some(registry.clone());
        let built = build_registered_mcp_tools(&registry, ctx).await;
        for server in ["connected", "cached"] {
            let full_name = format!("mcp__{server}__send");
            let tool = built
                .iter()
                .flat_map(|(_, tools)| tools.iter())
                .find(|tool| tool.name() == full_name)
                .unwrap_or_else(|| panic!("missing rebuilt tool {full_name}"));
            assert_eq!(
                tool.mcp_role(),
                Some("comms"),
                "config role must reach every per-tool entry for {server}"
            );
        }
    }

    /// `if(n.some((l)=>l.type==="connected"&&!!l.capabilities?.resources))` —
    /// a server WITHOUT the resources capability contributes none of the three.
    #[tokio::test]
    async fn no_resource_capability_pushes_no_resource_tools() {
        let names = names_for(false).await;
        assert!(
            names.is_empty(),
            "a resource-less server must not add resource tools; got {names:?}"
        );
    }

    /// `o.push(K8,Z8,Yme)` — all THREE, including `ReadMcpResourceDirTool`.
    #[tokio::test]
    async fn resource_capability_pushes_all_three() {
        let names = names_for(true).await;
        assert_eq!(
            names,
            vec![
                "ListMcpResourcesTool".to_string(),
                "ReadMcpResourceTool".to_string(),
                "ReadMcpResourceDirTool".to_string(),
            ],
            "oracle pushes K8, Z8 and Yme in that order"
        );
    }

    /// The oracle's once-only latch (`if(fe.capabilities?.resources&&!k)k=!0`):
    /// two resource-capable servers still yield exactly one copy of each tool.
    #[tokio::test]
    async fn two_capable_servers_push_the_trio_once() {
        let registry = Arc::new(McpRegistry::new(Arc::new(NeverDialled)));
        for name in ["a", "b"] {
            registry.connections.write().await.insert(
                name.into(),
                McpConnectionState::Connected {
                    config: config(name),
                    connection_id: protocol::McpConnectionId::new(),
                    capabilities: caps(true),
                    negotiated: platform_api::McpNegotiatedProtocol {
                        era: platform_api::McpProtocolEra::Legacy,
                        version: "2025-11-25".into(),
                    },
                    tools: vec![],
                    resources: vec![],
                    resource_templates: vec![],
                    prompts: vec![],
                    connected_at: std::time::SystemTime::now(),
                },
            );
        }
        let mut ctx = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            Arc::new(telemetry::AnalyticsBus::new()),
            vec![std::path::PathBuf::from("/tmp")],
        );
        ctx.mcp_registry = Some(registry.clone());
        let names: Vec<String> = build_registered_mcp_tools(&registry, ctx)
            .await
            .into_iter()
            .flat_map(|(_, ts)| ts.into_iter().map(|t| t.name().to_string()))
            .collect();
        assert_eq!(
            names
                .iter()
                .filter(|n| *n == "ListMcpResourcesTool")
                .count(),
            1,
            "the trio is pushed once, not once per capable server; got {names:?}"
        );
        assert_eq!(names.len(), 3, "exactly the trio; got {names:?}");
    }

    #[tokio::test]
    async fn shared_builder_ignores_agent_scoped_entries_and_keeps_trio_for_shared_server() {
        let registry = Arc::new(McpRegistry::new(Arc::new(NeverDialled)));
        let shared_connection_id = protocol::McpConnectionId::new();
        let scoped_connection_id = protocol::McpConnectionId::new();
        let agent_id = protocol::AgentId::new();
        let scoped_key = mcp::registry::agent_scope_table_key(agent_id, "shared");

        let mut conns = registry.connections.write().await;
        conns.insert(
            scoped_key,
            McpConnectionState::Connected {
                config: config("shared"),
                connection_id: scoped_connection_id,
                capabilities: caps(true),
                negotiated: platform_api::McpNegotiatedProtocol {
                    era: platform_api::McpProtocolEra::Legacy,
                    version: "2025-11-25".into(),
                },
                tools: vec![dto("shared", "scoped_only")],
                resources: vec![],
                resource_templates: vec![],
                prompts: vec![],
                connected_at: std::time::SystemTime::now(),
            },
        );
        conns.insert(
            "shared".into(),
            McpConnectionState::Connected {
                config: config("shared"),
                connection_id: shared_connection_id,
                capabilities: caps(true),
                negotiated: platform_api::McpNegotiatedProtocol {
                    era: platform_api::McpProtocolEra::Legacy,
                    version: "2025-11-25".into(),
                },
                tools: vec![dto("shared", "shared_only")],
                resources: vec![],
                resource_templates: vec![],
                prompts: vec![],
                connected_at: std::time::SystemTime::now(),
            },
        );
        drop(conns);

        let built = build_names_by_connection(&registry).await;
        assert_eq!(built.len(), 1, "only shared/session entry should build");
        assert_eq!(built[0].0, shared_connection_id);
        assert_eq!(
            built[0].1,
            vec![
                "mcp__shared__shared_only".to_string(),
                "ListMcpResourcesTool".to_string(),
                "ReadMcpResourceTool".to_string(),
                "ReadMcpResourceDirTool".to_string(),
            ]
        );
        assert!(
            !built[0]
                .1
                .iter()
                .any(|name| name == "mcp__shared__scoped_only"),
            "agent-scoped tool must not leak into the shared unbound builder"
        );
    }

    #[tokio::test]
    async fn rebuild_reassigns_resource_trio_to_remaining_shared_owner_after_removal() {
        let registry = Arc::new(McpRegistry::new(Arc::new(NeverDialled)));
        let a_id = protocol::McpConnectionId::new();
        let b_id = protocol::McpConnectionId::new();
        registry.connections.write().await.insert(
            "a".into(),
            McpConnectionState::Connected {
                config: config("a"),
                connection_id: a_id,
                capabilities: caps(true),
                negotiated: platform_api::McpNegotiatedProtocol {
                    era: platform_api::McpProtocolEra::Legacy,
                    version: "2025-11-25".into(),
                },
                tools: vec![dto("a", "a_only")],
                resources: vec![],
                resource_templates: vec![],
                prompts: vec![],
                connected_at: std::time::SystemTime::now(),
            },
        );
        registry.connections.write().await.insert(
            "b".into(),
            McpConnectionState::Connected {
                config: config("b"),
                connection_id: b_id,
                capabilities: caps(true),
                negotiated: platform_api::McpNegotiatedProtocol {
                    era: platform_api::McpProtocolEra::Legacy,
                    version: "2025-11-25".into(),
                },
                tools: vec![dto("b", "b_only")],
                resources: vec![],
                resource_templates: vec![],
                prompts: vec![],
                connected_at: std::time::SystemTime::now(),
            },
        );

        let first = build_names_by_connection(&registry).await;
        let first_owner = first
            .iter()
            .find(|(_, names)| names.iter().any(|name| name == "ListMcpResourcesTool"))
            .expect("one shared owner gets the trio");
        assert_eq!(
            first
                .iter()
                .filter(|(_, names)| names.iter().any(|name| name == "ListMcpResourcesTool"))
                .count(),
            1
        );

        let removed_server = if first_owner.0 == a_id { "a" } else { "b" };
        let remaining_id = if first_owner.0 == a_id { b_id } else { a_id };
        registry.connections.write().await.remove(removed_server);

        let second = build_names_by_connection(&registry).await;
        assert_eq!(second.len(), 1, "only remaining shared server should build");
        assert_eq!(second[0].0, remaining_id);
        assert!(
            second[0]
                .1
                .iter()
                .any(|name| name == "ListMcpResourcesTool"),
            "after a full rebuild the remaining shared resource server must pick up the trio"
        );
    }

    /// `register_all` must not register the generic dispatcher or the resource
    /// trio as builtins — discovered per-tool entries are the only production
    /// MCP invocation surface, so an FQN cannot bypass its own permission rule.
    #[test]
    fn register_all_omits_generic_dispatcher_and_resource_trio() {
        let ctx = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            Arc::new(telemetry::AnalyticsBus::new()),
            vec![std::path::PathBuf::from("/tmp")],
        );
        let mut reg = tool_api::ToolRegistry::new();
        crate::register_all(&mut reg, ctx);
        let names = reg.all_names();
        assert!(
            !names.iter().any(|n| n == MCP_TOOL_NAME),
            "generic MCP dispatcher must not be a production builtin; got {names:?}"
        );
        for banned in [
            "ListMcpResourcesTool",
            "ReadMcpResourceTool",
            "ReadMcpResourceDirTool",
        ] {
            assert!(
                !names.iter().any(|n| n == banned),
                "{banned} must not be a base-list builtin; got {names:?}"
            );
        }
        assert!(
            names.iter().any(|n| n == "WaitForMcpServers"),
            "WaitForMcpServers IS a base-list tool in both 2.1.220 and 2.1.238; got {names:?}"
        );
    }
}

#[cfg(test)]
mod cached_resource_tool_tests {
    use super::*;
    use crate::mcp_tool::cached_resource_test_support::{
        ctx_with_registry, new_cached_registry, resource_caps, seed_cached_server,
        seed_connected_server, CachedServerBehavior,
    };
    use telemetry::InMemorySink;

    async fn call_list_tool(
        registry: Arc<McpRegistry>,
        input: Value,
    ) -> Result<ToolCallResult, ToolError> {
        ListMcpResourcesTool::new(ctx_with_registry(registry))
            .call(
                input,
                tool_api::test_support::fresh_ctx(),
                tool_api::test_support::fresh_tx(),
            )
            .await
    }

    async fn call_read_tool(
        registry: Arc<McpRegistry>,
        input: Value,
    ) -> Result<ToolCallResult, ToolError> {
        ReadMcpResourceTool::new(ctx_with_registry(registry))
            .call(
                input,
                tool_api::test_support::fresh_ctx(),
                tool_api::test_support::fresh_tx(),
            )
            .await
    }

    #[tokio::test]
    async fn list_resources_accepts_raw_and_normalized_cached_server_names() {
        let (registry, transport) = new_cached_registry().await;
        seed_cached_server(
            &registry,
            &transport,
            "my.server",
            CachedServerBehavior {
                resources: vec![platform_api::McpResourceDto {
                    uri: "cached://guide".into(),
                    name: "guide.md".into(),
                    description: None,
                    mime_type: Some("text/markdown".into()),
                    meta: None,
                }],
                ..Default::default()
            },
        )
        .await;
        seed_cached_server(
            &registry,
            &transport,
            "claude.ai Linear",
            CachedServerBehavior {
                resources: vec![platform_api::McpResourceDto {
                    uri: "cached://linear".into(),
                    name: "linear.md".into(),
                    description: None,
                    mime_type: None,
                    meta: None,
                }],
                ..Default::default()
            },
        )
        .await;

        let result = call_list_tool(
            registry.clone(),
            json!({ "server_name": mcp::normalization::normalize_name_for_mcp("my.server") }),
        )
        .await
        .expect("normalized cached list succeeds");
        assert_eq!(
            result.data["resources"],
            json!([{
                "uri": "cached://guide",
                "name": "guide.md",
                "mimeType": "text/markdown",
                "server": "my.server"
            }])
        );

        let result = call_list_tool(registry, json!({ "server_name": "claude.ai Linear" }))
            .await
            .expect("raw cached list succeeds");
        assert_eq!(
            result.data["resources"],
            json!([{
                "uri": "cached://linear",
                "name": "linear.md",
                "server": "claude.ai Linear"
            }])
        );
        assert_eq!(transport.connect_calls("my.server"), 1);
        assert_eq!(transport.connect_calls("claude.ai Linear"), 1);
    }

    #[tokio::test]
    async fn list_resources_named_cached_dial_failure_returns_error_without_completed_event() {
        let (registry, transport) = new_cached_registry().await;
        seed_cached_server(
            &registry,
            &transport,
            "cached",
            CachedServerBehavior {
                connect_error: Some("dial exploded".into()),
                ..Default::default()
            },
        )
        .await;

        let ctx = ctx_with_registry(registry);
        let sink = Arc::new(InMemorySink::new());
        ctx.bus.attach_sink(sink.clone()).await;

        let err = ListMcpResourcesTool::new(ctx)
            .call(
                json!({ "server_name": "cached" }),
                tool_api::test_support::fresh_ctx(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .expect_err("lazy dial failure must hard-fail named server");
        assert!(format!("{err}").contains("dial exploded"));

        let events = sink.events().await;
        assert_eq!(
            events
                .iter()
                .filter(|event| event.name == LIST_MCP_RESOURCES_FAILED)
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.name == LIST_MCP_RESOURCES_COMPLETED)
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn list_resources_named_capability_downgrade_errors_without_any_resources_list_rpc() {
        let (registry, transport) = new_cached_registry().await;
        seed_cached_server(
            &registry,
            &transport,
            "cached",
            CachedServerBehavior {
                cached_capabilities: resource_caps(false),
                live_capabilities: platform_api::ServerCapabilitiesDto {
                    resources: false,
                    ..resource_caps(false)
                },
                ..Default::default()
            },
        )
        .await;

        let err = call_list_tool(registry, json!({ "server_name": "cached" }))
            .await
            .expect_err("live capability downgrade must fail named list");
        assert!(format!("{err}").contains("does not support resources"));
        assert_eq!(transport.connect_calls("cached"), 1);
        assert_eq!(
            transport.rpc_call_count("cached", "transport::list_resources"),
            0
        );
        assert_eq!(transport.rpc_call_count("cached", "resources/list"), 0);
    }

    #[tokio::test]
    async fn list_resources_all_servers_includes_cached_and_isolates_cached_failures() {
        let (registry, transport) = new_cached_registry().await;
        seed_connected_server(
            &registry,
            &transport,
            "alpha",
            CachedServerBehavior {
                resources: vec![platform_api::McpResourceDto {
                    uri: "live://a".into(),
                    name: "alpha.txt".into(),
                    description: None,
                    mime_type: None,
                    meta: None,
                }],
                ..Default::default()
            },
        )
        .await;
        seed_cached_server(
            &registry,
            &transport,
            "beta",
            CachedServerBehavior {
                resources: vec![platform_api::McpResourceDto {
                    uri: "cached://b".into(),
                    name: "beta.txt".into(),
                    description: None,
                    mime_type: None,
                    meta: None,
                }],
                ..Default::default()
            },
        )
        .await;
        seed_cached_server(
            &registry,
            &transport,
            "gamma",
            CachedServerBehavior {
                resources_list_rpc_error: true,
                ..Default::default()
            },
        )
        .await;

        let tool = ListMcpResourcesTool::new(ctx_with_registry(registry));
        let result = tool
            .call(
                json!({}),
                tool_api::test_support::fresh_ctx(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .expect("best-effort all-server listing succeeds");

        assert_eq!(
            result.data["resources"],
            json!([
                { "uri": "live://a", "name": "alpha.txt", "server": "alpha" },
                { "uri": "cached://b", "name": "beta.txt", "server": "beta" }
            ])
        );
        assert_eq!(transport.connect_calls("alpha"), 1, "connected seed dial");
        assert_eq!(
            transport.connect_calls("beta"),
            1,
            "cached success lazy dial"
        );
        assert_eq!(
            transport.connect_calls("gamma"),
            1,
            "cached failure still isolated"
        );
    }

    #[tokio::test]
    async fn list_resources_all_servers_skips_capability_downgrade_without_any_resources_list_rpc()
    {
        let (registry, transport) = new_cached_registry().await;
        seed_connected_server(
            &registry,
            &transport,
            "alpha",
            CachedServerBehavior {
                resources: vec![platform_api::McpResourceDto {
                    uri: "live://a".into(),
                    name: "alpha.txt".into(),
                    description: None,
                    mime_type: None,
                    meta: None,
                }],
                ..Default::default()
            },
        )
        .await;
        seed_cached_server(
            &registry,
            &transport,
            "beta",
            CachedServerBehavior {
                cached_capabilities: resource_caps(false),
                live_capabilities: platform_api::ServerCapabilitiesDto {
                    resources: false,
                    ..resource_caps(false)
                },
                ..Default::default()
            },
        )
        .await;

        let result = call_list_tool(registry, json!({}))
            .await
            .expect("all-server list keeps best-effort semantics");
        assert_eq!(
            result.data["resources"],
            json!([{ "uri": "live://a", "name": "alpha.txt", "server": "alpha" }])
        );
        assert_eq!(transport.connect_calls("beta"), 1);
        assert_eq!(
            transport.rpc_call_count("beta", "transport::list_resources"),
            0
        );
        assert_eq!(transport.rpc_call_count("beta", "resources/list"), 0);
    }

    #[tokio::test]
    async fn list_resources_named_and_read_ignore_scoped_only_entries() {
        let (registry, _transport) = new_cached_registry().await;
        let agent_id = protocol::AgentId::new();
        let scoped_key = mcp::registry::agent_scope_table_key(agent_id, "shared");
        registry.connections.write().await.insert(
            scoped_key,
            mcp::McpConnectionState::Cached {
                config: cached_resource_test_support::cached_server_config("shared"),
                connection_id: protocol::McpConnectionId::new(),
                capabilities: resource_caps(true),
                negotiated: platform_api::McpNegotiatedProtocol {
                    era: platform_api::McpProtocolEra::Legacy,
                    version: "2025-11-25".into(),
                },
                tools: vec![],
                resources: vec![],
                resource_templates: vec![],
                prompts: vec![],
                cache_saved_at_ms: 0,
                age_ms: 0,
            },
        );

        let err = call_list_tool(registry.clone(), json!({ "server_name": "shared" }))
            .await
            .expect_err("scoped-only entry must not satisfy shared lookup");
        assert!(format!("{err}").contains("is not registered"));

        let err = call_read_tool(
            registry,
            json!({ "server_name": "shared", "uri": "cached://shared" }),
        )
        .await
        .expect_err("scoped-only entry must not satisfy shared lookup");
        assert!(format!("{err}").contains("is not registered"));
    }

    #[tokio::test]
    async fn list_resources_exact_raw_name_beats_normalized_collision() {
        let (registry, transport) = new_cached_registry().await;
        seed_connected_server(
            &registry,
            &transport,
            "my.server",
            CachedServerBehavior {
                resources: vec![platform_api::McpResourceDto {
                    uri: "cached://dot".into(),
                    name: "dot.md".into(),
                    description: None,
                    mime_type: None,
                    meta: None,
                }],
                ..Default::default()
            },
        )
        .await;
        seed_connected_server(
            &registry,
            &transport,
            "my_server",
            CachedServerBehavior {
                resources: vec![platform_api::McpResourceDto {
                    uri: "cached://underscore".into(),
                    name: "underscore.md".into(),
                    description: None,
                    mime_type: None,
                    meta: None,
                }],
                ..Default::default()
            },
        )
        .await;

        let dot = call_list_tool(registry.clone(), json!({ "server_name": "my.server" }))
            .await
            .expect("exact raw dot name should resolve to dot server");
        assert_eq!(
            dot.data["resources"],
            json!([{ "uri": "cached://dot", "name": "dot.md", "server": "my.server" }])
        );
        assert_eq!(transport.connect_calls("my.server"), 1);
        assert_eq!(
            transport.connect_calls("my_server"),
            1,
            "connected fixture dials both exact raw keys up front"
        );
        assert_eq!(transport.rpc_call_count("my.server", "resources/list"), 1);
        assert_eq!(transport.rpc_call_count("my_server", "resources/list"), 0);

        let underscore = call_list_tool(registry, json!({ "server_name": "my_server" }))
            .await
            .expect("exact raw underscore name should resolve to underscore server");
        assert_eq!(
            underscore.data["resources"],
            json!([{ "uri": "cached://underscore", "name": "underscore.md", "server": "my_server" }])
        );
        assert_eq!(transport.connect_calls("my.server"), 1);
        assert_eq!(transport.connect_calls("my_server"), 1);
        assert_eq!(transport.rpc_call_count("my.server", "resources/list"), 1);
        assert_eq!(transport.rpc_call_count("my_server", "resources/list"), 1);
    }

    #[tokio::test]
    async fn list_resources_ambiguous_normalized_alias_fails_closed_without_rpc() {
        let (registry, transport) = new_cached_registry().await;
        for name in ["my.server", "my_server"] {
            seed_cached_server(&registry, &transport, name, CachedServerBehavior::default()).await;
        }

        let err = call_list_tool(registry, json!({ "server_name": "my/server" }))
            .await
            .expect_err("colliding normalized alias must fail closed");
        let err_text = format!("{err}");
        assert!(err_text.contains("ambiguous"));
        assert!(err_text.contains("my.server"));
        assert!(err_text.contains("my_server"));
        assert_eq!(transport.connect_calls("my.server"), 0);
        assert_eq!(transport.connect_calls("my_server"), 0);
        assert_eq!(transport.rpc_call_count("my.server", "resources/list"), 0);
        assert_eq!(transport.rpc_call_count("my_server", "resources/list"), 0);
    }

    #[tokio::test]
    async fn list_resources_all_servers_ignores_same_named_scoped_entry() {
        let (registry, transport) = new_cached_registry().await;
        seed_cached_server(
            &registry,
            &transport,
            "shared",
            CachedServerBehavior {
                resources: vec![platform_api::McpResourceDto {
                    uri: "cached://shared".into(),
                    name: "shared.md".into(),
                    description: None,
                    mime_type: Some("text/markdown".into()),
                    meta: None,
                }],
                ..Default::default()
            },
        )
        .await;
        let agent_id = protocol::AgentId::new();
        let scoped_key = mcp::registry::agent_scope_table_key(agent_id, "shared");
        registry.connections.write().await.insert(
            scoped_key,
            mcp::McpConnectionState::Cached {
                config: cached_resource_test_support::cached_server_config("shared"),
                connection_id: protocol::McpConnectionId::new(),
                capabilities: resource_caps(true),
                negotiated: platform_api::McpNegotiatedProtocol {
                    era: platform_api::McpProtocolEra::Legacy,
                    version: "2025-11-25".into(),
                },
                tools: vec![],
                resources: vec![platform_api::McpResourceDto {
                    uri: "cached://scoped".into(),
                    name: "scoped.md".into(),
                    description: None,
                    mime_type: None,
                    meta: None,
                }],
                resource_templates: vec![],
                prompts: vec![],
                cache_saved_at_ms: 0,
                age_ms: 0,
            },
        );

        let result = call_list_tool(registry, json!({}))
            .await
            .expect("same-named scoped row must not duplicate shared listing");

        assert_eq!(
            result.data["resources"],
            json!([{
                "uri": "cached://shared",
                "name": "shared.md",
                "mimeType": "text/markdown",
                "server": "shared"
            }])
        );
        assert_eq!(transport.connect_calls("shared"), 1);
        assert_eq!(
            transport.rpc_call_count("shared", "resources/list"),
            1,
            "shared row should be listed once even if a same-named scoped row exists"
        );
    }

    #[tokio::test]
    async fn read_resource_accepts_raw_and_normalized_cached_server_names() {
        let (registry, transport) = new_cached_registry().await;
        seed_cached_server(
            &registry,
            &transport,
            "my.server",
            CachedServerBehavior {
                read_contents: vec![json!({
                    "uri": "cached://guide",
                    "mimeType": "text/markdown",
                    "text": "# Cached"
                })],
                ..Default::default()
            },
        )
        .await;
        seed_cached_server(
            &registry,
            &transport,
            "claude.ai Linear",
            CachedServerBehavior {
                read_contents: vec![json!({
                    "uri": "cached://linear",
                    "text": "linear"
                })],
                ..Default::default()
            },
        )
        .await;

        let result = call_read_tool(
            registry.clone(),
            json!({
                "server_name": mcp::normalization::normalize_name_for_mcp("my.server"),
                "uri": "cached://guide"
            }),
        )
        .await
        .expect("normalized cached read succeeds");
        assert_eq!(
            result.data["contents"],
            json!([{
                "uri": "cached://guide",
                "mimeType": "text/markdown",
                "text": "# Cached"
            }])
        );

        let result = call_read_tool(
            registry,
            json!({ "server_name": "claude.ai Linear", "uri": "cached://linear" }),
        )
        .await
        .expect("raw cached read succeeds");
        assert_eq!(
            result.data["contents"],
            json!([{ "uri": "cached://linear", "text": "linear" }])
        );
        assert_eq!(transport.connect_calls("my.server"), 1);
        assert_eq!(transport.connect_calls("claude.ai Linear"), 1);
    }

    #[tokio::test]
    async fn read_resource_exact_raw_name_beats_normalized_collision() {
        let (registry, transport) = new_cached_registry().await;
        seed_connected_server(
            &registry,
            &transport,
            "my.server",
            CachedServerBehavior {
                read_contents: vec![json!({
                    "uri": "cached://dot",
                    "text": "dot"
                })],
                ..Default::default()
            },
        )
        .await;
        seed_connected_server(
            &registry,
            &transport,
            "my_server",
            CachedServerBehavior {
                read_contents: vec![json!({
                    "uri": "cached://underscore",
                    "text": "underscore"
                })],
                ..Default::default()
            },
        )
        .await;

        let dot = call_read_tool(
            registry.clone(),
            json!({ "server_name": "my.server", "uri": "cached://dot" }),
        )
        .await
        .expect("exact raw dot name should resolve to dot server");
        assert_eq!(
            dot.data["contents"],
            json!([{ "uri": "cached://dot", "text": "dot" }])
        );
        assert_eq!(transport.connect_calls("my.server"), 1);
        assert_eq!(
            transport.connect_calls("my_server"),
            1,
            "connected fixture dials both exact raw keys up front"
        );
        assert_eq!(transport.rpc_call_count("my.server", "resources/read"), 1);
        assert_eq!(transport.rpc_call_count("my_server", "resources/read"), 0);

        let underscore = call_read_tool(
            registry,
            json!({ "server_name": "my_server", "uri": "cached://underscore" }),
        )
        .await
        .expect("exact raw underscore name should resolve to underscore server");
        assert_eq!(
            underscore.data["contents"],
            json!([{ "uri": "cached://underscore", "text": "underscore" }])
        );
        assert_eq!(transport.connect_calls("my.server"), 1);
        assert_eq!(transport.connect_calls("my_server"), 1);
        assert_eq!(transport.rpc_call_count("my.server", "resources/read"), 1);
        assert_eq!(transport.rpc_call_count("my_server", "resources/read"), 1);
    }

    #[tokio::test]
    async fn read_resource_ambiguous_normalized_alias_fails_closed_without_rpc() {
        let (registry, transport) = new_cached_registry().await;
        for name in ["my.server", "my_server"] {
            seed_cached_server(&registry, &transport, name, CachedServerBehavior::default()).await;
        }

        let err = call_read_tool(
            registry,
            json!({ "server_name": "my/server", "uri": "cached://ambiguous" }),
        )
        .await
        .expect_err("colliding normalized alias must fail closed");
        let err_text = format!("{err}");
        assert!(err_text.contains("ambiguous"));
        assert!(err_text.contains("my.server"));
        assert!(err_text.contains("my_server"));
        assert_eq!(transport.connect_calls("my.server"), 0);
        assert_eq!(transport.connect_calls("my_server"), 0);
        assert_eq!(transport.rpc_call_count("my.server", "resources/read"), 0);
        assert_eq!(transport.rpc_call_count("my_server", "resources/read"), 0);
    }

    #[tokio::test]
    async fn read_resource_capability_downgrade_errors_without_any_read_rpc() {
        let (registry, transport) = new_cached_registry().await;
        seed_cached_server(
            &registry,
            &transport,
            "cached",
            CachedServerBehavior {
                cached_capabilities: resource_caps(false),
                live_capabilities: platform_api::ServerCapabilitiesDto {
                    resources: false,
                    ..resource_caps(false)
                },
                ..Default::default()
            },
        )
        .await;

        let err = call_read_tool(
            registry,
            json!({ "server_name": "cached", "uri": "cached://guide" }),
        )
        .await
        .expect_err("live capability downgrade must fail read");
        assert!(format!("{err}").contains("does not support resources"));
        assert_eq!(transport.connect_calls("cached"), 1);
        assert_eq!(transport.rpc_call_count("cached", "resources/read"), 0);
    }
}
